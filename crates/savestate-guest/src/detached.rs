//! Detached processes (`Exec` with `detach`): transient systemd units
//! `savestate-exec-<id>.service`, outside the agent's cgroup, so they outlive the
//! connection and the agent. Each keeps its stdout and stderr in [`DIR`]; systemd keeps
//! its exit status (`RemainAfterExit`), which `Processes` and `Logs` read.

use crate::conn::Conn;
use orbd_guest::cmd;
use savestate_guest_proto::CHUNK;
use savestate_guest_proto::proto::{
    Detached, Done, Exec, Exited, Output, Process, ProcessList, Stream, exited, guest_message,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::PathBuf;
use std::time::Duration;

/// Where detached processes' output goes: `<id>.stdout` and `<id>.stderr`.
const DIR: &str = "/var/log/savestate/exec";
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
/// How often `Logs` with `follow` looks for more output.
const FOLLOW: Duration = Duration::from_millis(200);
/// Units per `systemctl show`, whose output `cmd` caps.
const SHOW_BATCH: usize = 16;

pub fn start(conn: &Conn, exec: &Exec) -> Result<(), String> {
    if exec.argv.is_empty() {
        return Err("exec: empty argv".into());
    }
    let id = new_id()?;
    std::fs::create_dir_all(DIR).map_err(|e| format!("{DIR}: {e}"))?;
    let (stdout, stderr) = (output(&id, Stream::Stdout), output(&id, Stream::Stderr));
    // There from the start, so `Logs` finds them however soon it asks.
    for path in [&stdout, &stderr] {
        File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let mut run = vec![
        "systemd-run".to_owned(),
        "--quiet".into(),
        format!("--unit={}", unit(&id)),
        // One line, so `systemctl show` output stays one property per line.
        format!(
            "--description={}",
            exec.argv.join(" ").replace(char::is_control, " ")
        ),
        // The arguments as given: no `$VAR` expansion by the service manager.
        "--expand-environment=no".into(),
        // Started means exec'd: a program that cannot start fails the call.
        "--service-type=exec".into(),
        // The unit and its exit status stay after it exits.
        "--property=RemainAfterExit=yes".into(),
        format!("--property=StandardOutput=append:{}", stdout.display()),
        format!("--property=StandardError=append:{}", stderr.display()),
        format!(
            "--working-directory={}",
            if exec.cwd.is_empty() { "/" } else { &exec.cwd }
        ),
        "--setenv=HOME=/root".into(),
    ];
    run.extend(
        exec.env
            .iter()
            .map(|v| format!("--setenv={}={}", v.name, v.value)),
    );
    run.push("--".into());
    run.extend(exec.argv.iter().cloned());
    let argv: Vec<&str> = run.iter().map(String::as_str).collect();
    if let Err(e) = cmd::check(&argv, TOOL_TIMEOUT) {
        for path in [&stdout, &stderr] {
            let _ = std::fs::remove_file(path);
        }
        return Err(format!("exec {}: {e}", exec.argv[0]));
    }
    tracing::info!(id, program = exec.argv[0], "detached");
    conn.send(guest_message::Body::Detached(Detached { id }))
}

/// `Processes`: the one with `id`, or every one, oldest first.
pub fn processes(conn: &Conn, id: &str) -> Result<(), String> {
    let ids = if id.is_empty() {
        all_ids()?
    } else {
        known(id)?;
        vec![id.to_owned()]
    };
    let mut found = Vec::new();
    for batch in ids.chunks(SHOW_BATCH) {
        found.extend(show(batch)?);
    }
    found.sort_by_key(|(started, _)| *started);
    conn.send(guest_message::Body::ProcessList(ProcessList {
        processes: found.into_iter().map(|(_, p)| p).collect(),
    }))
}

/// `Logs`: the output so far, or with `follow` until the process has exited.
pub fn logs(conn: &Conn, id: &str, follow: bool) -> Result<(), String> {
    known(id)?;
    let open = |stream| {
        let path = output(id, stream);
        File::open(&path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let mut files = [
        (open(Stream::Stdout)?, Stream::Stdout),
        (open(Stream::Stderr)?, Stream::Stderr),
    ];
    let mut buf = vec![0; CHUNK];
    loop {
        // Read the status first: once it says exited, everything the process wrote is
        // in the files, and draining them below gets all of it.
        let exited = status(id)?;
        for (file, stream) in &mut files {
            loop {
                let n = match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(format!("output of {id}: {e}")),
                };
                conn.send(guest_message::Body::Output(Output {
                    stream: (*stream).into(),
                    data: buf[..n].to_vec(),
                }))?;
            }
        }
        match exited {
            Some(status) => {
                return conn.send(guest_message::Body::Exited(Exited {
                    status: Some(status),
                }));
            }
            None if !follow => return conn.send(guest_message::Body::Done(Done {})),
            None => std::thread::sleep(FOLLOW),
        }
    }
}

fn unit(id: &str) -> String {
    format!("savestate-exec-{id}.service")
}

fn output(id: &str, stream: Stream) -> PathBuf {
    let suffix = match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    };
    PathBuf::from(DIR).join(format!("{id}.{suffix}"))
}

/// 16 random lowercase hex digits.
fn new_id() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(format!("{:016x}", u64::from_be_bytes(bytes)))
}

/// Fails unless `id` names a detached process (and so is safe in a path).
fn known(id: &str) -> Result<(), String> {
    let valid = id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid || !output(id, Stream::Stdout).exists() {
        return Err(format!("no detached process {id}"));
    }
    Ok(())
}

fn all_ids() -> Result<Vec<String>, String> {
    let dir = match std::fs::read_dir(DIR) {
        Ok(dir) => dir,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{DIR}: {e}")),
    };
    Ok(dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some(name.strip_suffix(".stdout")?.to_owned())
        })
        .filter(|id| known(id).is_ok())
        .collect())
}

/// How the process ended, or `None` while it runs.
fn status(id: &str) -> Result<Option<exited::Status>, String> {
    let (_, process) = show(&[id.to_owned()])?
        .pop()
        .ok_or_else(|| format!("no detached process {id}"))?;
    Ok(process.exited.and_then(|e| e.status))
}

/// The processes of `ids` that systemd knows, each with its start time (monotonic µs).
fn show(ids: &[String]) -> Result<Vec<(u64, Process)>, String> {
    let units: Vec<String> = ids.iter().map(|id| unit(id)).collect();
    let mut argv = vec![
        "systemctl",
        "show",
        "--property=LoadState,Description,ExecMainCode,ExecMainStatus,ExecMainStartTimestampMonotonic",
    ];
    argv.extend(units.iter().map(String::as_str));
    let out = cmd::check(&argv, TOOL_TIMEOUT)?;
    // One block of `Key=value` lines per unit, in the order asked, blank-line separated.
    let mut found = Vec::new();
    for (id, block) in ids.iter().zip(out.stdout.split("\n\n")) {
        let props: HashMap<&str, &str> = block.lines().filter_map(|l| l.split_once('=')).collect();
        if props.get("LoadState") != Some(&"loaded") {
            continue;
        }
        let number = |key: &str| props.get(key).and_then(|v| v.parse::<i64>().ok());
        let status = number("ExecMainStatus").and_then(|v| i32::try_from(v).ok());
        // CLD_EXITED, CLD_KILLED or CLD_DUMPED once the main process has ended.
        let exited = match (number("ExecMainCode"), status) {
            (Some(1), Some(code)) => Some(exited::Status::Code(code)),
            (Some(2 | 3), Some(signal)) => Some(exited::Status::Signal(signal)),
            _ => None,
        };
        let started = number("ExecMainStartTimestampMonotonic")
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0);
        found.push((
            started,
            Process {
                id: id.clone(),
                command: props.get("Description").copied().unwrap_or("").to_owned(),
                exited: exited.map(|status| Exited {
                    status: Some(status),
                }),
            },
        ));
    }
    Ok(found)
}
