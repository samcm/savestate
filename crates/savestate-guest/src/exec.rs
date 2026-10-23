//! `Exec`: runs a program with its stdin fed from the host and its output streamed
//! back. The program outlives the connection: if the host goes away (including when
//! this VM is a restored copy and the connection was reset), its stdin is closed and
//! its output discarded.

use crate::conn::Conn;
use savestate_guest_proto::CHUNK;
use savestate_guest_proto::proto::{
    Exec, Exited, Output, Stream, exited, guest_message, host_message,
};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;

pub fn run(conn: &Arc<Conn>, exec: &Exec) -> Result<(), String> {
    let (program, args) = exec.argv.split_first().ok_or("exec: empty argv")?;
    let mut child = Command::new(program)
        .args(args)
        .envs(exec.env.iter().map(|v| (&v.name, &v.value)))
        .current_dir(if exec.cwd.is_empty() { "/" } else { &exec.cwd })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("exec {program}: {e}"))?;
    tracing::info!(program, pid = child.id(), "exec");

    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(format!("exec {program}: no pipes"));
    };
    let feeder = conn.clone();
    // Ends when the host sends eof or goes away, or when the connection is closed below.
    std::thread::spawn(move || feed(&feeder, stdin));
    let pumps = [
        (Box::new(stdout) as Box<dyn Read + Send>, Stream::Stdout),
        (Box::new(stderr), Stream::Stderr),
    ]
    .map(|(pipe, stream)| {
        let conn = conn.clone();
        std::thread::spawn(move || pump(&conn, pipe, stream))
    });

    let status = child
        .wait()
        .map_err(|e| format!("exec {program}: wait: {e}"))?;
    // Output is complete once every holder of the pipes (the program and anything it
    // left running with them) has closed them.
    for p in pumps {
        let _ = p.join();
    }
    let status = match (status.code(), status.signal()) {
        (Some(code), _) => exited::Status::Code(code),
        (None, Some(signal)) => exited::Status::Signal(signal),
        (None, None) => return Err(format!("exec {program}: unknown exit status {status}")),
    };
    conn.send(guest_message::Body::Exited(Exited {
        status: Some(status),
    }))
}

fn feed(conn: &Conn, stdin: ChildStdin) {
    let mut stdin = Some(stdin);
    while let Ok(Some(host_message::Body::Stdin(s))) = conn.recv() {
        // A program that closed its stdin gets nothing more; the host may still send.
        if let Some(pipe) = &mut stdin
            && pipe.write_all(&s.data).is_err()
        {
            stdin = None;
        }
        if s.eof {
            return;
        }
    }
}

fn pump(conn: &Conn, mut pipe: Box<dyn Read + Send>, stream: Stream) {
    let mut buf = vec![0; CHUNK];
    let mut connected = true;
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        // Keep draining after the host is gone so the program never blocks on output.
        connected = connected
            && conn
                .send(guest_message::Body::Output(Output {
                    stream: stream.into(),
                    data: buf[..n].to_vec(),
                }))
                .is_ok();
    }
}
