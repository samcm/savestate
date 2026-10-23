//! The pre-checkpoint hook and the post-restore fixups (see `Restore` in the
//! protocol for the order and its reasons).

use nix::sys::time::TimeSpec;
use nix::time::{ClockId, clock_settime};
use orbd_guest::cmd;
use orbd_guest::files::write_atomic;
use savestate_guest_proto::proto::Restore;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HOOKS: &str = "/etc/savestate/after-restore.d";
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_ENTROPY: usize = 32;
/// `_IOW('R', 0x03, int[2])` from linux/random.h.
const RNDADDENTROPY: libc::Ioctl = 0x4008_5203;

/// Syncs filesystems, drops the page cache and compacts memory, so the memory freed
/// forms the 2 MiB blocks free page reporting hands back to the host, and the
/// checkpoint does not save it. Without compaction most of a dropped cache lies in
/// smaller holes between used pages, which are never reported.
pub fn prepare_checkpoint() -> Result<(), String> {
    nix::unistd::sync();
    std::fs::write("/proc/sys/vm/drop_caches", "3")
        .map_err(|e| format!("dropping the page cache: {e}"))?;
    std::fs::write("/proc/sys/vm/compact_memory", "1")
        .map_err(|e| format!("compacting memory: {e}"))?;
    tracing::info!("prepared for checkpoint");
    Ok(())
}

pub fn restore(r: &Restore) -> Result<(), String> {
    if r.entropy.len() < MIN_ENTROPY {
        return Err(format!(
            "restore: {} bytes of entropy, need at least {MIN_ENTROPY}",
            r.entropy.len()
        ));
    }
    if r.hostname.is_empty() {
        return Err("restore: no hostname".into());
    }
    if r.wall_clock_unix_ns != 0 {
        step_clock(r.wall_clock_unix_ns)?;
    }
    add_entropy(&r.entropy)?;
    new_machine_id()?;
    orbd_guest::init::set_hostname(&r.hostname)?;
    ssh_host_keys()?;
    hooks(r)?;
    tracing::info!(sandbox = r.sandbox, checkpoint = r.checkpoint, "restored");
    Ok(())
}

fn step_clock(unix_ns: i64) -> Result<(), String> {
    let ts = TimeSpec::new(
        unix_ns.div_euclid(1_000_000_000),
        unix_ns.rem_euclid(1_000_000_000),
    );
    clock_settime(ClockId::CLOCK_REALTIME, ts).map_err(|e| format!("clock_settime: {e}"))?;
    tracing::info!(unix_ns, "wall clock stepped");
    Ok(())
}

/// Mixes `entropy` into the kernel's pool and credits it in full (RNDADDENTROPY).
#[expect(unsafe_code, reason = "RNDADDENTROPY is an ioctl")]
fn add_entropy(entropy: &[u8]) -> Result<(), String> {
    let len = i32::try_from(entropy.len()).map_err(|_| "too much entropy".to_owned())?;
    // struct rand_pool_info { int entropy_count; int buf_size; __u32 buf[]; }
    let mut info = Vec::with_capacity(8 + entropy.len());
    info.extend_from_slice(&len.saturating_mul(8).to_ne_bytes());
    info.extend_from_slice(&len.to_ne_bytes());
    info.extend_from_slice(entropy);
    let random = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/random")
        .map_err(|e| format!("/dev/random: {e}"))?;
    // SAFETY: RNDADDENTROPY reads a rand_pool_info of the size its header states, and
    // `info` holds exactly that; the kernel copies it without alignment requirements.
    let r = unsafe { libc::ioctl(random.as_raw_fd(), RNDADDENTROPY, info.as_ptr()) };
    if r < 0 {
        return Err(format!(
            "RNDADDENTROPY: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// A new random /etc/machine-id (and D-Bus's copy, where it is a file of its own).
/// Programs that already read the old one keep it.
fn new_machine_id() -> Result<(), String> {
    let mut id = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut id))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    let line: String = id
        .iter()
        .map(|b| format!("{b:02x}"))
        .chain(["\n".into()])
        .collect();
    let dbus = Path::new("/var/lib/dbus/machine-id");
    let mut paths = vec![Path::new("/etc/machine-id")];
    if dbus.symlink_metadata().is_ok_and(|m| m.is_file()) {
        paths.push(dbus);
    }
    for path in paths {
        write_atomic(path, line.as_bytes(), 0o444, None)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    tracing::info!(machine_id = line.trim(), "new machine id");
    Ok(())
}

/// Regenerates every SSH host key in /etc/ssh (of the same types), then has a running
/// SSH server load them.
fn ssh_host_keys() -> Result<(), String> {
    let mut keys: Vec<(PathBuf, String)> = match std::fs::read_dir("/etc/ssh") {
        Ok(dir) => dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let kind = name.strip_prefix("ssh_host_")?.strip_suffix("_key")?;
                Some((e.path(), kind.to_owned()))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("/etc/ssh: {e}")),
    };
    if keys.is_empty() {
        return Ok(());
    }
    keys.sort();
    for (key, kind) in &keys {
        let path = key.to_str().ok_or("non-UTF-8 SSH key path")?;
        for old in [path.to_owned(), format!("{path}.pub")] {
            match std::fs::remove_file(&old) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{old}: {e}")),
            }
        }
        cmd::check(
            &["ssh-keygen", "-q", "-N", "", "-t", kind, "-f", path],
            TOOL_TIMEOUT,
        )?;
    }
    tracing::info!(keys = keys.len(), "regenerated SSH host keys");
    let reload = cmd::run(
        &[
            "systemctl",
            "try-reload-or-restart",
            "ssh.service",
            "sshd.service",
        ],
        TOOL_TIMEOUT,
    )?;
    if !reload.ok {
        tracing::warn!(
            stderr = reload.stderr.trim(),
            "reloading the SSH server failed"
        );
    }
    Ok(())
}

/// Runs the after-restore hooks: executable files in [`HOOKS`] whose names do not start
/// with `.`, in name order.
fn hooks(r: &Restore) -> Result<(), String> {
    let mut hooks: Vec<PathBuf> = match std::fs::read_dir(HOOKS) {
        Ok(dir) => dir
            .flatten()
            .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
            .map(|e| e.path())
            .filter(|p| {
                std::fs::metadata(p)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{HOOKS}: {e}")),
    };
    hooks.sort();
    let clock = if r.wall_clock_unix_ns != 0 {
        "stepped"
    } else {
        "frozen"
    };
    for hook in hooks {
        let name = hook.display();
        let mut child = Command::new(&hook)
            .env("SAVESTATE_SANDBOX", &r.sandbox)
            .env("SAVESTATE_CHECKPOINT", &r.checkpoint)
            .env("SAVESTATE_CLOCK", clock)
            .current_dir("/")
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("hook {name}: {e}"))?;
        let deadline = Instant::now() + HOOK_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("hook {name} timed out after {HOOK_TIMEOUT:?}"));
                }
                Err(e) => return Err(format!("hook {name}: {e}")),
            }
        };
        if !status.success() {
            return Err(format!("hook {name} failed: {status}"));
        }
        tracing::info!(hook = %name, "after-restore hook done");
    }
    Ok(())
}
