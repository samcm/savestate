//! One VM's QEMU: booting it from an image or restoring it from a checkpoint, the
//! guest-agent work around both, and checkpointing it while it keeps running (see
//! "QEMU mechanics" in `docs/design.md`).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail, ensure};
use orbd_qemu::{
    launch::{self, HostFiles, Machine},
    migration, probe,
    qmp::{FileMigration, Qmp},
    vm::{fsutil, process::LaunchKind},
};
use savestate_api::Fingerprint;
use savestate_guest_proto::{
    Client, ExitStatus,
    proto::{Exec, Restore},
};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::{
    net,
    proto::{
        Boot, CheckpointReply, CheckpointRequest, Restore as RestoreSource, StartRequest,
        start_request::Source,
    },
};
use crate::store::{self, MEMORY, Manifest, Store};

const QEMU: &str = "qemu-system-x86_64";
/// `vm.cpuModel` that gives guests the node's own CPU.
const HOST_CPU: &str = "host";
const QEMU_IMG: &str = "qemu-img";
/// The guest kernel's command line (the guest contract in `guest/README.md`). No
/// interface renaming: udev renaming the NIC while the launcher configures it fails
/// the boot. The root filesystem is mounted with `discard`, so blocks the guest
/// frees leave its disk layer on the node (QEMU unmaps them) rather than staying
/// allocated until the sandbox is deleted.
const KERNEL_CMDLINE: &str = "console=ttyS0 root=/dev/vda rootflags=discard rw net.ifnames=0";
/// multifd channels writing and reading the memory file.
const MULTIFD_CHANNELS: u8 = 8;
/// Migration bandwidth limit for saves, bytes/s: far above any disk's write rate.
const MAX_BANDWIDTH: u64 = 100 << 30;
const POLL: Duration = Duration::from_millis(50);
const BOOT_TIMEOUT: Duration = Duration::from_secs(300);
const RESTORE_TIMEOUT: Duration = Duration::from_secs(120);
const GUEST_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The guest kernel's free page reporting hands freed memory back every 2 s, a part
/// of it each time: memory has settled once QEMU's resident set has not shrunk by
/// `REPORTING_STEP` for `REPORTING_WINDOW`.
const REPORTING_WINDOW: Duration = Duration::from_millis(2500);
const REPORTING_STEP: u64 = 16 << 20;
/// At most this long: a guest that frees memory continuously never settles.
const REPORTING_MAX: Duration = Duration::from_secs(30);

/// What every VM of this launcher shares.
pub struct Context {
    pub run_dir: PathBuf,
    pub kernel: PathBuf,
    pub dns: Vec<std::net::Ipv4Addr>,
    /// The launcher exits non-zero with the first message sent here.
    pub fatal: mpsc::Sender<String>,
    /// QEMU's pid once started (0 before), to stop it when the launcher exits.
    pub qemu_pid: Arc<AtomicI32>,
    /// `/dev/vhost-vsock`, opened while the launcher was root (the device is usually
    /// root's alone) and inherited by QEMU as `vhostfd`.
    pub vhost_vsock: std::os::fd::RawFd,
}

pub struct Vm {
    qmp: Qmp,
    qemu_pid: i32,
    guest: Client,
    store: Store,
    layer_prefix: String,
    /// The sandbox image the chain starts from, by digest.
    image: String,
    /// QEMU's node name of the active disk layer.
    top_node: String,
    /// The disk, base image first, active layer last.
    chain: Vec<String>,
    next_layer: u32,
    machine_args: Vec<String>,
    migration: FileMigration,
    /// Whether the mutable disk is one self-contained file on a PVC.
    active_disk: bool,
    /// Direct I/O support in the node-local state directory (checkpoint targets).
    direct_io: bool,
    vcpus: u32,
    memory_mib: u64,
    disk_bytes: u64,
    sandbox: String,
    qemu_version: String,
    machine_type: String,
    cpu_model: String,
    /// Checkpoints taken, by id, so a repeated request gets the first one's result.
    done: HashMap<String, Result<CheckpointReply, String>>,
}

/// The machine a start runs: booted fresh, or restored from a checkpoint.
struct Planned {
    kind: LaunchKind,
    machine_args: Vec<String>,
    /// The sandbox image the chain starts from, by digest.
    image: String,
    /// The disk below the sandbox's first layer.
    chain: Vec<String>,
    vcpus: u32,
    memory_mib: u64,
    disk_bytes: u64,
    cpu_model: String,
}

/// Boots or restores the VM `request` describes and returns once it runs with the
/// guest agent answering.
pub async fn start(ctx: &Context, request: StartRequest) -> anyhow::Result<Vm> {
    let source = request.source.clone().context("Start without a source")?;
    let store = Store::new(PathBuf::from(&request.state_dir));
    let qemu_version = probe::qemu_version(Path::new(QEMU)).context("qemu --version")?;
    let machine_type = probe::q35_machine(Path::new(QEMU)).map_err(anyhow::Error::msg)?;
    // Checkpoint files bypass the page cache where the node-local filesystem allows;
    // memory files only if the installation asks. A PVC may be a different
    // filesystem, so probe its directory separately for QEMU's active disk.
    let direct_io = probe::direct_io(&store.layers());
    let memory_direct_io = request.memory_file_direct_io && direct_io;
    let layer0 = store::layer_id(&request.layer_prefix, 0);
    let active_disk =
        (!request.active_disk.is_empty()).then(|| PathBuf::from(&request.active_disk));
    let active_exists = active_disk.as_ref().is_some_and(|path| path.is_file());
    let layer0_path = active_disk.clone().unwrap_or_else(|| store.layer(&layer0));
    let disk_direct_io = active_disk
        .as_deref()
        .and_then(Path::parent)
        .map_or(direct_io, probe::direct_io);

    let (machine, restore) = match source {
        Source::Boot(boot) => {
            let machine = boot_machine(
                ctx,
                &store,
                &boot,
                &machine_type,
                &layer0_path,
                active_disk.is_some(),
                active_exists,
            )
            .await?;
            (machine, None)
        }
        Source::Restore(restore) => {
            let dir = PathBuf::from(&restore.checkpoint_dir);
            let manifest = store::read_manifest(&dir)?;
            let machine = restore_machine(
                &store,
                &manifest,
                &qemu_version,
                &machine_type,
                &layer0_path,
                active_disk.is_some(),
                active_exists,
            )
            .await?;
            // A replacement pod cold-boots the durable disk. Loading the source
            // checkpoint's RAM again would discard everything done since restore.
            let restore = (!active_exists).then_some((restore, dir, manifest));
            (machine, restore)
        }
    };
    let qmp = spawn_qemu(ctx, &request, &machine, &layer0_path, disk_direct_io).await?;
    let guest = Client::vsock(request.vsock_cid);
    ready_guest(
        ctx,
        &request,
        &qmp,
        &guest,
        restore.as_ref(),
        memory_direct_io,
    )
    .await?;

    let mut chain = machine.chain;
    if active_disk.is_none() {
        chain.push(store::layer_path(&layer0));
    }
    Ok(Vm {
        qmp,
        qemu_pid: ctx.qemu_pid.load(Ordering::SeqCst),
        guest,
        store,
        layer_prefix: request.layer_prefix,
        image: machine.image,
        top_node: "disk0".into(),
        chain,
        next_layer: 1,
        machine_args: machine.machine_args,
        migration: FileMigration {
            multifd_channels: MULTIFD_CHANNELS,
            direct_io: memory_direct_io,
        },
        active_disk: active_disk.is_some(),
        direct_io,
        vcpus: machine.vcpus,
        memory_mib: machine.memory_mib,
        disk_bytes: machine.disk_bytes,
        sandbox: request.sandbox,
        qemu_version,
        machine_type,
        cpu_model: machine.cpu_model,
        done: HashMap::new(),
    })
}

async fn ready_guest(
    ctx: &Context,
    request: &StartRequest,
    qmp: &Qmp,
    guest: &Client,
    restore: Option<&(RestoreSource, PathBuf, Manifest)>,
    memory_direct_io: bool,
) -> anyhow::Result<()> {
    let hostname = hostname(&request.sandbox);
    let started = Instant::now();
    if let Some((restore, dir, manifest)) = restore {
        let migration = FileMigration {
            multifd_channels: manifest.migration.multifd_channels,
            direct_io: memory_direct_io,
        };
        migration::load_from_file(qmp, &dir.join(MEMORY), migration, POLL)
            .await
            .context("loading the checkpoint's memory")?;
        qmp.cont().await?;
        tracing::info!(
            loaded_ms = started.elapsed().as_millis(),
            "restored; guest running"
        );
        wait_for_agent(guest, RESTORE_TIMEOUT).await?;
        let wall_clock_unix_ns = if restore.stepped {
            i64::try_from(jiff::Timestamp::now().as_nanosecond()).unwrap_or(i64::MAX)
        } else {
            0
        };
        let fixups = Restore {
            wall_clock_unix_ns,
            entropy: entropy()?,
            hostname,
            sandbox: request.sandbox.clone(),
            checkpoint: restore.checkpoint.clone(),
        };
        tokio::time::timeout(GUEST_CALL_TIMEOUT, guest.restore(fixups))
            .await
            .context("post-restore fixups timed out")?
            .context("post-restore fixups")?;
    } else {
        wait_for_agent(guest, BOOT_TIMEOUT).await?;
        sh(guest, &net::guest_script(&ctx.dns, &hostname))
            .await
            .context("configuring the guest's network")?;
    }
    tracing::info!(
        ready_ms = started.elapsed().as_millis(),
        "guest agent ready"
    );

    Ok(())
}

/// A fresh machine on a first layer over the base image.
async fn boot_machine(
    ctx: &Context,
    store: &Store,
    boot: &Boot,
    machine_type: &str,
    layer0: &Path,
    active_disk: bool,
    active_exists: bool,
) -> anyhow::Result<Planned> {
    let image = store.root().join(&boot.image);
    ensure!(image.exists(), "base image {} is missing", image.display());
    if active_disk {
        if !active_exists {
            materialize_disk(&image, layer0, boot.disk_bytes).await?;
        }
    } else {
        qemu_img(&[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-F",
            "qcow2",
            "-b",
            &format!("../{}", boot.image),
            &layer0.display().to_string(),
            &boot.disk_bytes.to_string(),
        ])
        .await?;
    }
    // A named model must run in full on this host, never silently weakened.
    let cpu = if boot.cpu_model == HOST_CPU {
        HOST_CPU.to_owned()
    } else {
        format!("{},enforce", boot.cpu_model)
    };
    let machine_args = Machine {
        machine_type,
        cpu_model: &cpu,
        vcpus: boot.vcpus,
        memory_mib: boot.memory_mib,
        kernel: &ctx.kernel,
        kernel_cmdline: KERNEL_CMDLINE,
        net_args: net_args(),
        extra_args: [
            "-rtc",
            "base=utc,clock=vm",
            "-device",
            "vmgenid,id=vmgenid0,guid=auto",
        ]
        .map(String::from)
        .into(),
    }
    .args();
    Ok(Planned {
        kind: LaunchKind::Boot,
        machine_args,
        image: boot.image_reference.clone(),
        chain: vec![boot.image.clone()],
        vcpus: boot.vcpus,
        memory_mib: boot.memory_mib,
        disk_bytes: boot.disk_bytes,
        cpu_model: boot.cpu_model.clone(),
    })
}

/// The checkpoint's machine, on a first layer over the checkpoint's frozen layer.
async fn restore_machine(
    store: &Store,
    manifest: &Manifest,
    qemu_version: &str,
    machine_type: &str,
    layer0: &Path,
    active_disk: bool,
    active_exists: bool,
) -> anyhow::Result<Planned> {
    let fingerprint = &manifest.fingerprint;
    ensure!(
        fingerprint.qemu == qemu_version && fingerprint.machine_type == machine_type,
        "the checkpoint was taken on {} ({}); this launcher runs {qemu_version} ({machine_type})",
        fingerprint.qemu,
        fingerprint.machine_type,
    );
    if let Some(taken_on) = &fingerprint.host_cpu {
        let here = crate::host_cpu();
        ensure!(
            *taken_on == here,
            "the checkpoint gives the guest the CPU it was taken on ({taken_on}); this node's is {here}"
        );
    }
    let frozen = manifest
        .chain
        .last()
        .context("the checkpoint's chain has no layer")?;
    if active_disk {
        if !active_exists {
            materialize_disk(&store.root().join(frozen), layer0, manifest.disk_bytes).await?;
        }
    } else {
        let frozen = frozen.strip_prefix("layers/").unwrap_or(frozen);
        qemu_img(&[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-F",
            "qcow2",
            "-b",
            frozen,
            &layer0.display().to_string(),
        ])
        .await?;
    }
    Ok(Planned {
        kind: if active_exists {
            LaunchKind::Boot
        } else {
            LaunchKind::Restore { generation: 0 }
        },
        machine_args: manifest.machine_args.clone(),
        image: manifest.image.clone(),
        chain: manifest.chain.clone(),
        vcpus: manifest.vcpus,
        memory_mib: manifest.memory_mib,
        disk_bytes: manifest.disk_bytes,
        cpu_model: fingerprint.cpu_model.clone(),
    })
}

/// Atomically creates a self-contained qcow2 active disk from `source`. A partial
/// conversion is never mistaken for a durable disk by a replacement pod.
async fn materialize_disk(
    source: &Path,
    destination: &Path,
    disk_bytes: u64,
) -> anyhow::Result<()> {
    let part = destination.with_extension("qcow2.part");
    let _ = std::fs::remove_file(&part);
    let result = async {
        qemu_img(&[
            "convert",
            "-q",
            "-f",
            "qcow2",
            "-O",
            "qcow2",
            &source.display().to_string(),
            &part.display().to_string(),
        ])
        .await?;
        qemu_img(&[
            "resize",
            "-q",
            "-f",
            "qcow2",
            &part.display().to_string(),
            &disk_bytes.to_string(),
        ])
        .await?;
        std::fs::File::open(&part)?.sync_all()?;
        fsutil::rename_durable(&part, destination)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Starts QEMU (its serial console on a FIFO the launcher copies to stdout), has the
/// launcher exit when it does, and connects to its monitor.
async fn spawn_qemu(
    ctx: &Context,
    request: &StartRequest,
    machine: &Planned,
    layer0: &Path,
    direct_io: bool,
) -> anyhow::Result<Qmp> {
    let run = &ctx.run_dir;
    let console = run.join("console");
    let _ = std::fs::remove_file(&console);
    nix::unistd::mkfifo(&console, nix::sys::stat::Mode::from_bits_truncate(0o600))
        .context("console FIFO")?;
    let mut console_reader = tokio::net::unix::pipe::OpenOptions::new()
        .read_write(true)
        .open_receiver(&console)
        .context("console FIFO")?;
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut console_reader, &mut tokio::io::stdout()).await;
    });
    let qmp_path = run.join("qmp.sock");
    let mut args = launch::host_args(
        &request.sandbox,
        &HostFiles {
            pidfile: &run.join("qemu.pid"),
            qmp: &qmp_path,
            guest_sock: &run.join("guest.sock"),
            console_log: &console,
            disk: layer0,
        },
        direct_io,
        machine.kind,
    );
    args.extend(machine.machine_args.iter().cloned());
    // The guest agent's channel; its context id is not part of the saved machine, so
    // a restore gets its own.
    args.extend([
        "-device".into(),
        format!(
            "vhost-vsock-pci,id=vsock0,guest-cid={},vhostfd={}",
            request.vsock_cid, ctx.vhost_vsock
        ),
    ]);
    tracing::info!(?args, "starting QEMU");
    let mut child = tokio::process::Command::new(QEMU)
        .args(&args)
        .stdin(Stdio::null())
        .spawn()
        .context("starting QEMU")?;
    ctx.qemu_pid
        .store(child.id().map_or(0, u32::cast_signed), Ordering::SeqCst);
    let fatal = ctx.fatal.clone();
    tokio::spawn(async move {
        let message = match child.wait().await {
            Ok(status) => format!("QEMU exited ({status})"),
            Err(error) => format!("waiting for QEMU: {error}"),
        };
        let _ = fatal.send(message).await;
    });
    connect_qmp(&qmp_path).await
}

impl Vm {
    /// Checkpoints the running VM into `checkpoints/<id>.tmp/`, which the node agent
    /// created; the guest pauses for the save, then carries on.
    pub async fn checkpoint(
        &mut self,
        request: &CheckpointRequest,
    ) -> Result<CheckpointReply, String> {
        if let Some(done) = self.done.get(&request.id) {
            return done.clone();
        }
        let result = self
            .save(request)
            .await
            .map_err(|error| format!("{error:#}"));
        self.done.insert(request.id.clone(), result.clone());
        result
    }

    async fn save(&mut self, request: &CheckpointRequest) -> anyhow::Result<CheckpointReply> {
        let dir = self.store.checkpoints().tmp(&request.id);
        ensure!(dir.is_dir(), "{} does not exist", dir.display());
        // Less guest memory holding data means a smaller memory file and pause.
        match tokio::time::timeout(GUEST_CALL_TIMEOUT, self.guest.prepare_checkpoint()).await {
            Ok(Ok(())) => settle_reporting(self.qemu_pid).await,
            Ok(Err(error)) => tracing::warn!(%error, "pre-checkpoint hook failed"),
            Err(_) => tracing::warn!("pre-checkpoint hook timed out"),
        }
        let guest_time_unix_ns = sh(&self.guest, "date +%s%N")
            .await
            .ok()
            .and_then(|out| out.trim().parse().ok());
        // QEMU's default max-bandwidth (128 MiB/s) throttles saves to file too.
        self.qmp
            .configure_migration(self.migration, MAX_BANDWIDTH)
            .await?;

        let paused = Instant::now();
        self.qmp.stop().await?;
        if self.active_disk {
            let saved = self
                .save_active_disk(&dir, request, guest_time_unix_ns, paused)
                .await;
            // save_active_disk normally resumes the VM. On any earlier failure it
            // leaves QEMU stopped, so resume here as a best effort. Calling cont on
            // an already running VM is harmless and its error is intentionally
            // ignored on an error path.
            if saved.is_err() {
                let _ = migration::cancel_outgoing(&self.qmp, POLL).await;
                let _ = self.qmp.cont().await;
            }
            return saved;
        }
        let saved = self.save_stopped(&dir, request, guest_time_unix_ns).await;
        if saved.is_err() {
            let _ = migration::cancel_outgoing(&self.qmp, POLL).await;
        }
        // After a completed save `cont` also reactivates the disks.
        let resumed = self.qmp.cont().await;
        let save_ms = saved?;
        resumed?;
        let pause_ms = millis(paused.elapsed());
        tracing::info!(id = request.id, pause_ms, save_ms, "checkpoint saved");
        Ok(CheckpointReply { pause_ms, save_ms })
    }

    /// Saves a PVC-backed disk without making it a backing chain. The full block
    /// backup and RAM save both finish while the VM is stopped, so they describe the
    /// same point in time. QEMU cannot inactivate its block graph at RAM migration's
    /// switchover while a backup job is still using it.
    async fn save_active_disk(
        &mut self,
        dir: &Path,
        request: &CheckpointRequest,
        guest_time_unix_ns: Option<i64>,
        paused: Instant,
    ) -> anyhow::Result<CheckpointReply> {
        let (id, path) = loop {
            let id = store::layer_id(&self.layer_prefix, self.next_layer);
            let path = self.store.layer(&id);
            if !path.exists() {
                break (id, path);
            }
            self.next_layer += 1;
        };
        let created = qemu_img(&[
            "create",
            "-q",
            "-f",
            "qcow2",
            &path.display().to_string(),
            &self.disk_bytes.to_string(),
        ])
        .await;
        if let Err(error) = created {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let node = format!("checkpoint-disk{}", self.next_layer);
        let job = format!("checkpoint-backup{}", self.next_layer);
        let cache = serde_json::json!({ "direct": self.direct_io });
        let added = self
            .qmp
            .blockdev_add(serde_json::json!({
                "driver": "qcow2",
                "node-name": node,
                "discard": "unmap",
                "cache": cache,
                "file": {
                    "driver": "file",
                    "filename": path,
                    "aio": if self.direct_io { "native" } else { "threads" },
                    "cache": cache,
                },
            }))
            .await;
        if let Err(error) = added {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        if let Err(error) = self.qmp.blockdev_backup(&job, &self.top_node, &node).await {
            let _ = self.qmp.blockdev_del(&node).await;
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }

        if let Err(error) = self.wait_for_backup(&job).await {
            self.cancel_backup(&job, &node, &path).await;
            return Err(error);
        }
        if let Err(error) = self.qmp.job_dismiss(&job).await {
            self.cancel_backup(&job, &node, &path).await;
            return Err(error.into());
        }
        if let Err(error) = self.qmp.blockdev_del(&node).await {
            self.cancel_backup(&job, &node, &path).await;
            return Err(error.into());
        }
        if let Err(error) = std::fs::File::open(&path).and_then(|file| file.sync_all()) {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        set_read_only(&path);

        let memory = dir.join(MEMORY);
        let started = Instant::now();
        let saved = migration::save_to_file(&self.qmp, &memory, POLL, |_| Ok(())).await;
        let save_ms = millis(started.elapsed());
        if saved.is_err() {
            let _ = migration::cancel_outgoing(&self.qmp, POLL).await;
        }
        let resumed = self.qmp.cont().await;
        let pause_ms = millis(paused.elapsed());
        if let Err(error) = saved {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        if let Err(error) = resumed {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        self.next_layer += 1;

        let base = self.chain.first().cloned().context("empty chain")?;
        let chain = vec![base, store::layer_path(&id)];
        self.write_manifest(dir, request, guest_time_unix_ns, chain, &memory, &path)?;
        tracing::info!(id = request.id, pause_ms, save_ms, "checkpoint saved");
        Ok(CheckpointReply { pause_ms, save_ms })
    }

    async fn wait_for_backup(&self, job: &str) -> anyhow::Result<()> {
        loop {
            let jobs = self.qmp.query_jobs().await?;
            let info = jobs
                .iter()
                .find(|info| info.id == job)
                .with_context(|| format!("block backup job {job} disappeared"))?;
            match info.status.as_str() {
                "created" | "running" | "waiting" | "pending" => {}
                "concluded" => {
                    ensure!(
                        info.error.is_none(),
                        "block backup job {job}: {}",
                        info.error.as_deref().unwrap_or_default()
                    );
                    return Ok(());
                }
                status => {
                    bail!(
                        "block backup job {job} entered unexpected state {status}: {}",
                        info.error.as_deref().unwrap_or("no error reported")
                    );
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn cancel_backup(&self, job: &str, node: &str, path: &Path) {
        let _ = self.qmp.job_cancel(job).await;
        loop {
            match self.qmp.query_jobs().await {
                Ok(jobs) => {
                    let Some(info) = jobs.iter().find(|info| info.id == job) else {
                        break;
                    };
                    if info.status == "concluded" {
                        let _ = self.qmp.job_dismiss(job).await;
                        break;
                    }
                }
                Err(_) => break,
            }
            tokio::time::sleep(POLL).await;
        }
        let _ = self.qmp.blockdev_del(node).await;
        let _ = std::fs::remove_file(path);
    }

    /// The part of a checkpoint during which the guest is paused, up to the memory
    /// file and manifest being durable. Returns how long the memory save took.
    async fn save_stopped(
        &mut self,
        dir: &Path,
        request: &CheckpointRequest,
        guest_time_unix_ns: Option<i64>,
    ) -> anyhow::Result<u64> {
        // A new active layer over the current one, which is frozen from now on and
        // belongs to the checkpoint.
        let id = store::layer_id(&self.layer_prefix, self.next_layer);
        let path = self.store.layer(&id);
        let frozen = self.chain.last().cloned().context("empty chain")?;
        let checkpoint_chain = self.chain.clone();
        let frozen_file = frozen.strip_prefix("layers/").unwrap_or(&frozen);
        // -u: the current layer is open for writing; the size is given instead.
        qemu_img(&[
            "create",
            "-q",
            "-f",
            "qcow2",
            "-u",
            "-F",
            "qcow2",
            "-b",
            frozen_file,
            &path.display().to_string(),
            &self.disk_bytes.to_string(),
        ])
        .await?;
        let node = format!("disk{}", self.next_layer);
        let cache = serde_json::json!({ "direct": self.direct_io });
        self.qmp
            .blockdev_add(serde_json::json!({
                "driver": "qcow2",
                "node-name": node,
                "discard": "unmap",
                "cache": cache,
                "backing": null,
                "file": {
                    "driver": "file",
                    "filename": path,
                    "aio": if self.direct_io { "native" } else { "threads" },
                    "cache": cache,
                },
            }))
            .await?;
        self.qmp.blockdev_snapshot(&self.top_node, &node).await?;
        self.top_node = node;
        self.next_layer += 1;
        self.chain.push(store::layer_path(&id));
        let frozen_path = self.store.root().join(&frozen);
        set_read_only(&frozen_path);

        let memory = dir.join(MEMORY);
        let started = Instant::now();
        migration::save_to_file(&self.qmp, &memory, POLL, |_| Ok(())).await?;
        let save_ms = millis(started.elapsed());
        self.write_manifest(
            dir,
            request,
            guest_time_unix_ns,
            checkpoint_chain,
            &memory,
            &frozen_path,
        )?;
        Ok(save_ms)
    }

    fn write_manifest(
        &self,
        dir: &Path,
        request: &CheckpointRequest,
        guest_time_unix_ns: Option<i64>,
        chain: Vec<String>,
        memory: &Path,
        layer: &Path,
    ) -> anyhow::Result<()> {
        let fingerprint = Fingerprint {
            launcher_image: request.launcher_image.clone(),
            qemu: self.qemu_version.clone(),
            machine_type: self.machine_type.clone(),
            cpu_model: self.cpu_model.clone(),
            host_cpu: (self.cpu_model == HOST_CPU).then(crate::host_cpu),
            devices: devices_digest(&self.machine_args),
        };
        let manifest = Manifest {
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            sandbox: self.sandbox.clone(),
            parent: (!request.parent.is_empty()).then(|| request.parent.clone()),
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            disk_bytes: self.disk_bytes,
            guest_time_unix_ns,
            image: self.image.clone(),
            chain,
            machine_args: self.machine_args.clone(),
            migration: self.migration,
            memory_bytes: store::allocated(memory),
            layer_bytes: store::allocated(layer),
            fingerprint,
        };
        // Last: its presence marks the save complete.
        fsutil::write_json_atomic(&dir.join(orbd_vm::checkpoint::MANIFEST), &manifest)?;
        Ok(())
    }
}

/// Waits until the memory the pre-checkpoint hook freed is back with the host (free
/// page reporting), so the memory file leaves it out: until QEMU's resident set stops
/// shrinking, at most `REPORTING_MAX`.
async fn settle_reporting(qemu_pid: i32) {
    let started = Instant::now();
    let Some(before) = resident_bytes(qemu_pid) else {
        return;
    };
    let (mut low, mut since) = (before, started);
    while since.elapsed() < REPORTING_WINDOW && started.elapsed() < REPORTING_MAX {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let Some(now) = resident_bytes(qemu_pid) else {
            return;
        };
        if now + REPORTING_STEP <= low {
            (low, since) = (now, Instant::now());
        }
    }
    tracing::info!(
        waited_ms = millis(started.elapsed()),
        resident_mib_before = before >> 20,
        resident_mib = low >> 20,
        "free page reporting settled"
    );
}

/// QEMU's resident memory (`VmRSS`), in bytes.
fn resident_bytes(pid: i32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(kib << 10)
}

/// `sha256:<hex>` of the guest-visible machine arguments.
fn devices_digest(machine_args: &[String]) -> String {
    let mut hash = Sha256::new();
    for arg in machine_args {
        hash.update(arg.as_bytes());
        hash.update([0]);
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
}

fn net_args() -> Vec<String> {
    vec![
        "-netdev".into(),
        format!("tap,id=net0,ifname={},script=no,downscript=no", net::TAP),
        "-device".into(),
        format!("virtio-net-pci,id=net0dev,netdev=net0,mac={}", net::MAC),
    ]
}

/// The guest's hostname for a sandbox: an RFC 1123 label.
pub fn hostname(sandbox: &str) -> String {
    let label: String = sandbox
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() { c } else { '-' }
        })
        .take(63)
        .collect();
    let label = label.trim_matches('-');
    if label.is_empty() {
        "sandbox".into()
    } else {
        label.into()
    }
}

async fn connect_qmp(path: &Path) -> anyhow::Result<Qmp> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match Qmp::connect(path, Duration::from_secs(1)).await {
            Ok(qmp) => return Ok(qmp),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).context("QEMU's monitor did not answer");
            }
            Err(_) => tokio::time::sleep(POLL).await,
        }
    }
}

async fn wait_for_agent(guest: &Client, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), guest.hello()).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) if Instant::now() >= deadline => {
                return Err(error).context("the guest agent did not answer");
            }
            Err(_) if Instant::now() >= deadline => bail!("the guest agent did not answer"),
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// Runs `script` with `sh -c` in the guest; its stdout, or an error unless it exits 0.
pub async fn sh(guest: &Client, script: &str) -> anyhow::Result<String> {
    let exec = Exec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        ..Exec::default()
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = tokio::time::timeout(
        GUEST_CALL_TIMEOUT,
        guest.exec(exec, tokio::io::empty(), &mut out, &mut err),
    )
    .await
    .context("timed out")??;
    if status != ExitStatus::Code(0) {
        bail!("{status:?}: {}", String::from_utf8_lossy(&err).trim());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

async fn qemu_img(args: &[&str]) -> anyhow::Result<()> {
    let out = tokio::process::Command::new(QEMU_IMG)
        .args(args)
        .output()
        .await
        .context("qemu-img")?;
    ensure!(
        out.status.success(),
        "qemu-img {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// 32 bytes from the host's random source.
fn entropy() -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = vec![0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn set_read_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444)) {
        tracing::warn!(path = %path.display(), %error, "making a frozen layer read-only");
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
