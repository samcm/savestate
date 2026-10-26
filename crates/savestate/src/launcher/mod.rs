//! The launcher (one per VM pod): starts and supervises QEMU, drives it over QMP for
//! checkpoints and restores, does the guest-agent work around them, and streams the
//! serial console to the pod log. It has no Kubernetes credentials; the node agent
//! drives it over `launcher.sock` in its run directory (`proto/savestate/launcher`).
//!
//! As root it builds the guest's network in the pod's namespace and hands its run
//! directory to [`QEMU_UID`]; then it drops to that user, with the groups of the
//! VM devices, for good, so QEMU runs unprivileged (and with `-sandbox on`). It
//! creates `ready` in the run directory once the VM runs, and exits non-zero when
//! QEMU stops or the VM cannot start. Exec and file copy reach the guest through
//! `savestate guest` (see [`guest`]), run in the pod through `pods/exec`. One root
//! process stays: `savestate launcher-ports` (see [`ports`]), which keeps the exposed
//! ports in step with the sandbox.

pub mod guest;
mod net;
pub mod ports;
mod vm;

use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr},
    os::{fd::AsRawFd as _, unix::fs::MetadataExt as _},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
};

use anyhow::Context as _;
use ipnet::IpNet;
use nix::unistd::{Gid, Uid};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::store::{QEMU_GID, QEMU_UID};

pub mod proto {
    #![expect(
        unused_qualifications,
        clippy::doc_markdown,
        clippy::default_trait_access,
        clippy::too_many_lines,
        reason = "generated code"
    )]
    tonic::include_proto!("savestate.launcher.v1");
}

use proto::{
    CheckpointReply, CheckpointRequest, FailReply, FailRequest, StartReply, StartRequest,
    StatusReply, StatusRequest,
    launcher_server::{self, LauncherServer},
    status_reply::State,
};

/// The run directory inside the VM pod: the host's
/// `/run/savestate/<namespace>/<sandbox>/<sandbox UID>/`.
pub const RUN_DIR: &str = "/run/savestate";
/// The launcher's socket in the run directory.
pub const SOCKET: &str = "launcher.sock";
/// Created in the run directory once the VM runs; the pod's readiness probe.
pub const READY: &str = "ready";
/// The active qcow2 path in a VM pod whose disk is PVC-backed.
pub(crate) const ACTIVE_DISK: &str = "/var/lib/savestate-active/disk.qcow2";
/// The guest agent's vsock context id, in the run directory; written by the node
/// agent, which allocates them per node.
pub const CID: &str = "vsock-cid";
/// The guest kernel in the launcher image.
pub const KERNEL: &str = "/usr/local/share/savestate/bzImage";

/// Devices QEMU opens; the launcher keeps their groups when it drops to [`QEMU_UID`].
const DEVICES: &[&str] = &["/dev/kvm", "/dev/net/tun", "/dev/vhost-vsock"];

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Destinations in private and link-local ranges the guest may reach (repeat).
    #[arg(long = "egress-allow", value_name = "CIDR")]
    pub egress_allow: Vec<IpNet>,
    /// DNS servers for the guest (repeat) [default: the pod's own resolvers].
    #[arg(long = "dns", value_name = "IP")]
    pub dns: Vec<IpAddr>,
    /// The node's addresses, which the guest may not reach.
    #[arg(long = "host-ip", env = "SAVESTATE_HOST_IPS", value_delimiter = ',')]
    pub host_ip: Vec<IpAddr>,
    /// The guest kernel.
    #[arg(long, default_value = KERNEL)]
    pub kernel: PathBuf,
    /// The exposed ports, which `savestate launcher-ports` follows.
    #[arg(long, default_value = ports::FILE)]
    pub ports_file: PathBuf,
    #[arg(long, default_value = RUN_DIR)]
    pub run_dir: PathBuf,
}

/// Run the launcher until its VM stops (non-zero) or the pod is deleted (zero).
pub fn run(args: &Args) -> ExitCode {
    crate::telemetry::init_logging();
    match launch(args) {
        Ok(code) => code,
        Err(error) => fail(&format!("{error:#}")),
    }
}

fn launch(args: &Args) -> anyhow::Result<ExitCode> {
    // A replacement pod may inherit its predecessor's readiness marker.
    let _ = std::fs::remove_file(args.run_dir.join(READY));
    // The guest has IPv4 only.
    let dns: Vec<Ipv4Addr> = if args.dns.is_empty() {
        net::nameservers(&std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default())
    } else {
        args.dns
            .iter()
            .filter_map(|ip| match ip {
                IpAddr::V4(ip) => Some(*ip),
                IpAddr::V6(_) => None,
            })
            .collect()
    };
    // Privileged: the guest's network, and the run directory for QEMU.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(net::setup(&net::Egress {
            allow: &args.egress_allow,
            dns: &dns,
            host: &args.host_ip,
        }))
        .context("setting up the guest network")?;
    std::os::unix::fs::chown(&args.run_dir, Some(QEMU_UID), Some(QEMU_GID))
        .with_context(|| format!("chown {}", args.run_dir.display()))?;
    // Root, with NET_ADMIN, for the life of the pod: the exposed ports follow the
    // sandbox's spec while everything else runs as the QEMU user.
    let mut ports = std::process::Command::new(std::env::current_exe()?)
        .arg("launcher-ports")
        .arg("--file")
        .arg(&args.ports_file)
        .spawn()
        .context("starting savestate launcher-ports")?;
    // Without O_CLOEXEC: QEMU inherits it.
    let vhost_vsock = nix::fcntl::open(
        "/dev/vhost-vsock",
        nix::fcntl::OFlag::O_RDWR,
        nix::sys::stat::Mode::empty(),
    )
    .context("opening /dev/vhost-vsock")?;
    drop_privileges().context("dropping privileges")?;
    tracing::info!(dns = ?dns, "network ready; running as the QEMU user");
    std::thread::spawn(move || {
        let status = ports.wait();
        tracing::error!(
            ?status,
            "launcher-ports stopped: exposed ports no longer change"
        );
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(args, dns, vhost_vsock.as_raw_fd()))
}

/// Becomes [`QEMU_UID`] with the groups that own the VM devices, for good.
fn drop_privileges() -> anyhow::Result<()> {
    let mut groups = BTreeSet::from([QEMU_GID]);
    for device in DEVICES {
        let meta = std::fs::metadata(device).with_context(|| format!("{device} is missing"))?;
        groups.insert(meta.gid());
    }
    let groups: Vec<Gid> = groups.into_iter().map(Gid::from_raw).collect();
    nix::unistd::setgroups(&groups)?;
    nix::unistd::setgid(Gid::from_raw(QEMU_GID))?;
    nix::unistd::setuid(Uid::from_raw(QEMU_UID))?;
    Ok(())
}

async fn serve(
    args: &Args,
    dns: Vec<Ipv4Addr>,
    vhost_vsock: std::os::fd::RawFd,
) -> anyhow::Result<ExitCode> {
    let (fatal, mut fatal_rx) = mpsc::channel(1);
    let qemu_pid = Arc::new(AtomicI32::new(0));
    let launcher = Arc::new(Launcher {
        ctx: Arc::new(vm::Context {
            run_dir: args.run_dir.clone(),
            kernel: args.kernel.clone(),
            dns,
            fatal,
            qemu_pid: qemu_pid.clone(),
            vhost_vsock,
        }),
        phase: Arc::new(Mutex::new(Phase::Waiting)),
    });
    let socket = args.run_dir.join(SOCKET);
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    tracing::info!(socket = %socket.display(), "waiting for the node agent");
    let server = tonic::transport::Server::builder()
        .add_service(LauncherServer::from_arc(launcher))
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener));
    let server = tokio::spawn(server);

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let code = tokio::select! {
        message = fatal_rx.recv() => fail(&message.unwrap_or_default()),
        _ = terminate.recv() => {
            let message = "the launcher was terminated with its pod; it stopped the VM";
            tracing::info!(message);
            let _ = std::fs::write("/dev/termination-log", message);
            ExitCode::SUCCESS
        }
        result = server => {
            fail(&format!("the launcher's server stopped: {result:?}"))
        }
    };
    let pid = qemu_pid.load(Ordering::SeqCst);
    if pid > 0 {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    Ok(code)
}

/// Reports why the launcher stops (also as the container's termination message) and
/// gives its exit code.
fn fail(message: &str) -> ExitCode {
    tracing::error!(message, "the VM stopped");
    let _ = std::fs::write("/dev/termination-log", message);
    ExitCode::FAILURE
}

enum Phase {
    Waiting,
    Starting,
    Running(Arc<tokio::sync::Mutex<vm::Vm>>),
}

struct Launcher {
    ctx: Arc<vm::Context>,
    phase: Arc<Mutex<Phase>>,
}

impl Launcher {
    fn phase(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[tonic::async_trait]
impl launcher_server::Launcher for Launcher {
    async fn start(&self, request: Request<StartRequest>) -> Result<Response<StartReply>, Status> {
        let request = request.into_inner();
        {
            let mut phase = self.phase();
            if !matches!(*phase, Phase::Waiting) {
                return Err(Status::failed_precondition("the VM was started already"));
            }
            *phase = Phase::Starting;
        }
        tracing::info!(?request, "starting the VM");
        let (ctx, phase) = (self.ctx.clone(), self.phase.clone());
        tokio::spawn(async move {
            match vm::start(&ctx, request).await {
                Ok(vm) => {
                    *phase
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Phase::Running(Arc::new(tokio::sync::Mutex::new(vm)));
                    if let Err(error) = std::fs::write(ctx.run_dir.join(READY), b"") {
                        let _ = ctx.fatal.send(format!("creating {READY}: {error}")).await;
                    }
                    tracing::info!("the VM runs");
                }
                Err(error) => {
                    let _ = ctx.fatal.send(format!("{error:#}")).await;
                }
            }
        });
        Ok(Response::new(StartReply {}))
    }

    async fn status(&self, _: Request<StatusRequest>) -> Result<Response<StatusReply>, Status> {
        let state = match *self.phase() {
            Phase::Waiting => State::Waiting,
            Phase::Starting => State::Starting,
            Phase::Running(_) => State::Running,
        };
        Ok(Response::new(StatusReply {
            state: state.into(),
        }))
    }

    async fn checkpoint(
        &self,
        request: Request<CheckpointRequest>,
    ) -> Result<Response<CheckpointReply>, Status> {
        let vm = match &*self.phase() {
            Phase::Running(vm) => vm.clone(),
            _ => return Err(Status::failed_precondition("the VM is not running")),
        };
        // One checkpoint at a time.
        let mut vm = vm.lock().await;
        vm.checkpoint(&request.into_inner())
            .await
            .map(Response::new)
            .map_err(Status::internal)
    }

    async fn fail(&self, request: Request<FailRequest>) -> Result<Response<FailReply>, Status> {
        let message = request.into_inner().message;
        let _ = self.ctx.fatal.send(message).await;
        Ok(Response::new(FailReply {}))
    }
}

/// The guest agent's context id, from the run directory.
pub fn read_cid(run_dir: &Path) -> anyhow::Result<u32> {
    let path = run_dir.join(CID);
    std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .parse()
        .with_context(|| format!("{} is not a context id", path.display()))
}
