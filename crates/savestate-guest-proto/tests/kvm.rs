//! The guest agent end to end on real QEMU/KVM: boots the guest image, runs a command
//! and copies a file through the agent, saves the VM to a file, restores it into a new
//! QEMU with a different vsock context id and a new VM generation id, and checks the
//! post-restore fixups.
//!
//! Ignored by default. Needs /dev/kvm, /dev/vhost-vsock, `qemu-system-x86_64` and
//! `qemu-img` on PATH (the launcher image's), and the guest built by `guest/`:
//! `SAVESTATE_TEST_KERNEL` (bzImage) and `SAVESTATE_TEST_ROOTFS` (rootfs.qcow2).

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use orbd_qemu::migration::{load_from_file, save_to_file};
use orbd_qemu::qmp::{FileMigration, Qmp};
use savestate_guest_proto::proto::{Exec, Restore};
use savestate_guest_proto::{Client, ExitStatus};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MACHINE: &str = "pc-q35-11.1";
const POLL: Duration = Duration::from_millis(100);
const MIGRATION: FileMigration = FileMigration {
    multifd_channels: 0,
    direct_io: false,
};
const HOOK: &str = r#"#!/bin/sh
echo "$SAVESTATE_SANDBOX $SAVESTATE_CHECKPOINT $SAVESTATE_CLOCK $(hostname)" >/run/after-restore
"#;

#[tokio::test]
#[ignore = "needs /dev/kvm, /dev/vhost-vsock and the guest image"]
async fn exec_files_and_restore_fixups() {
    let kernel = env_path("SAVESTATE_TEST_KERNEL");
    let rootfs = env_path("SAVESTATE_TEST_ROOTFS");
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.qcow2");
    let memory = dir.path().join("memory");
    let ok = Command::new("qemu-img")
        .args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(&rootfs)
        .arg(&disk)
        .arg("4G")
        .status()
        .unwrap();
    assert!(ok.success(), "qemu-img create");
    // vsock context ids are global on the host.
    let cid = 10_000 + 2 * std::process::id();

    let a = Vm::start(dir.path(), "a", &kernel, &disk, cid, false).await;
    let hello = a.wait_for_agent().await;

    // A command gets its stdin, and its exit code and both output streams come back.
    let (status, out, err) = sh(&a.client, "cat; echo err >&2; exit 3", b"in\n").await;
    assert_eq!(
        (status, out.as_str(), err.as_str()),
        (ExitStatus::Code(3), "in\n", "err\n")
    );
    // The root filesystem fills the sandbox's disk, not just the image.
    let (_, root_bytes, _) = sh(&a.client, "df -B1 --output=size / | tail -1", b"").await;
    let root_bytes: u64 = root_bytes.trim().parse().unwrap();
    assert!(
        root_bytes > 3 << 30,
        "/ is {root_bytes} bytes on a 4 GiB disk"
    );

    let hook = "/etc/savestate/after-restore.d/50-record";
    let written = a
        .client
        .write_file(hook, 0o755, HOOK.as_bytes())
        .await
        .unwrap();
    assert_eq!(written, HOOK.len() as u64);
    let mut read = Vec::new();
    let download = a.client.read_file(hook).await.unwrap();
    assert_eq!(
        (download.info.directory, download.info.mode),
        (false, 0o755)
    );
    download.copy_to(&mut read).await.unwrap();
    assert_eq!(String::from_utf8(read).unwrap(), HOOK);
    let (_, machine_id, _) = sh(&a.client, "cat /etc/machine-id", b"").await;

    // Checkpoint: the guest prepares, QEMU saves its state and exits.
    a.client.prepare_checkpoint().await.unwrap();
    a.qmp.configure_migration(MIGRATION, 0).await.unwrap();
    a.qmp.stop().await.unwrap();
    save_to_file(&a.qmp, &memory, POLL, |_| Ok(()))
        .await
        .unwrap();
    a.quit().await;

    // Restore into a new QEMU with another vsock context id and VM generation id.
    let b = Vm::start(dir.path(), "b", &kernel, &disk, cid + 1, true).await;
    load_from_file(&b.qmp, &memory, MIGRATION, POLL)
        .await
        .unwrap();
    b.qmp.cont().await.unwrap();
    let restored = b.wait_for_agent().await;
    assert_eq!(restored.boot_id, hello.boot_id, "restored, not rebooted");

    let stepped_to = SystemTime::now() + Duration::from_hours(30 * 24);
    let stepped_to = stepped_to.duration_since(UNIX_EPOCH).unwrap();
    let mut entropy = vec![0; 32];
    std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").unwrap(),
        &mut entropy,
    )
    .unwrap();
    b.client
        .restore(Restore {
            wall_clock_unix_ns: i64::try_from(stepped_to.as_nanos()).unwrap(),
            entropy,
            hostname: "web-2".into(),
            sandbox: "web-2".into(),
            checkpoint: "web-1-parked".into(),
        })
        .await
        .unwrap();

    let (_, after, _) = sh(
        &b.client,
        "cat /etc/machine-id /run/after-restore; hostname; date +%s",
        b"",
    )
    .await;
    let lines: Vec<&str> = after.lines().collect();
    let [new_id, hook_saw, hostname, now] = lines[..] else {
        panic!("unexpected output {after:?}");
    };
    assert_ne!(new_id, machine_id.trim(), "a new machine id");
    assert!(new_id.len() == 32 && new_id.bytes().all(|b| b.is_ascii_hexdigit()));
    // Hooks run last, with the restore context, after the hostname changed.
    assert_eq!(hook_saw, "web-2 web-1-parked stepped web-2");
    assert_eq!(hostname, "web-2");
    let now: u64 = now.parse().unwrap();
    assert!(
        (stepped_to.as_secs()..stepped_to.as_secs() + 300).contains(&now),
        "guest clock {now}, stepped to {}",
        stepped_to.as_secs()
    );
}

fn env_path(name: &str) -> PathBuf {
    let path = std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set"));
    std::fs::canonicalize(&path).unwrap_or_else(|e| panic!("{name}: {e}"))
}

async fn sh(client: &Client, script: &str, stdin: &[u8]) -> (ExitStatus, String, String) {
    let exec = Exec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        ..Default::default()
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = client
        .exec(exec, stdin, &mut out, &mut err)
        .await
        .unwrap_or_else(|e| panic!("{script}: {e}"));
    (
        status,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

struct Vm {
    qemu: Child,
    qmp: Qmp,
    client: Client,
    console: PathBuf,
}

impl Vm {
    async fn start(
        dir: &Path,
        name: &str,
        kernel: &Path,
        disk: &Path,
        cid: u32,
        incoming: bool,
    ) -> Self {
        let console = dir.join(format!("console-{name}.log"));
        let qmp = dir.join(format!("qmp-{name}.sock"));
        let blockdev = format!(
            r#"{{"driver":"qcow2","node-name":"disk0","file":{{"driver":"file","filename":"{}"}}}}"#,
            disk.display()
        );
        let mut cmd = Command::new("qemu-system-x86_64");
        cmd.args(["-nodefaults", "-no-user-config", "-display", "none"])
            .args(["-machine", &format!("{MACHINE},accel=kvm")])
            .args([
                "-cpu", "host", "-smp", "2", "-m", "1024", "-rtc", "clock=vm",
            ])
            .arg("-kernel")
            .arg(kernel)
            .args(["-append", "console=ttyS0 root=/dev/vda rw"])
            .args(["-serial", &format!("file:{}", console.display())])
            .args(["-blockdev", &blockdev])
            .args(["-device", "virtio-blk-pci,id=disk0dev,drive=disk0"])
            .args([
                "-device",
                &format!("vhost-vsock-pci,id=vsock0,guest-cid={cid}"),
            ])
            .args(["-device", "vmgenid,id=vmgenid0,guid=auto"])
            .args([
                "-qmp",
                &format!("unix:{},server=on,wait=off", qmp.display()),
            ]);
        if incoming {
            cmd.args(["-S", "-incoming", "defer"]);
        }
        let qemu = cmd.spawn().expect("qemu-system-x86_64");
        let deadline = Instant::now() + Duration::from_secs(10);
        let qmp = loop {
            match Qmp::connect(&qmp, Duration::from_secs(1)).await {
                Ok(q) => break q,
                Err(e) if Instant::now() > deadline => panic!("QMP: {e}"),
                Err(_) => tokio::time::sleep(POLL).await,
            }
        };
        Self {
            qemu,
            qmp,
            client: Client::vsock(cid),
            console,
        }
    }

    async fn wait_for_agent(&self) -> savestate_guest_proto::proto::HelloReply {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match self.client.hello().await {
                Ok(h) => return h,
                Err(e) if Instant::now() > deadline => panic!("guest agent: {e}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    async fn quit(mut self) {
        self.qmp.quit().await.unwrap();
        self.qemu.wait().unwrap();
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.qemu.kill();
        let _ = self.qemu.wait();
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.console).unwrap_or_default();
            let _ = writeln!(
                std::io::stderr().lock(),
                "--- {} ---\n{log}",
                self.console.display()
            );
        }
    }
}
