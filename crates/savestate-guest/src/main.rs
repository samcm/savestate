//! `savestate-guest`: the agent inside every savestate VM, run as
//! `savestate-guest.service`. It serves the host on vsock port
//! [`PORT`](savestate_guest_proto::PORT) with the `savestate.guest.v1` protocol: exec
//! (attached, or detached as systemd units), file and directory copy, the
//! pre-checkpoint hook and the post-restore fixups. The vsock transport
//! and the guest-side building blocks (atomic writes, hostname, bounded tool runs)
//! come from orbd's guest library.
//!
//! * `savestate-guest serve`: serve the host until stopped.
//! * `savestate-guest --version`

mod conn;
mod detached;
mod exec;
mod files;
mod fixups;

use conn::Conn;
use orbd_guest::transport::{Connector, Vsock};
use savestate_guest_proto::proto::{Done, HelloReply, guest_message, host_message};
use std::io::Write;
use std::sync::Arc;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["serve"] => {
            // The journal adds the time and the unit.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .init();
            serve()
        }
        ["--version"] => writeln!(
            std::io::stdout().lock(),
            "savestate-guest {}",
            env!("CARGO_PKG_VERSION")
        ),
        _ => {
            writeln!(
                std::io::stderr().lock(),
                "usage: savestate-guest serve | --version"
            )?;
            std::process::exit(2);
        }
    }
}

/// Accepts host connections forever; each gets its own thread and carries one
/// operation.
fn serve() -> ! {
    let vsock = Vsock::new(savestate_guest_proto::PORT);
    tracing::info!(port = savestate_guest_proto::PORT, "listening on vsock");
    loop {
        match vsock.accept() {
            Ok(session) => {
                std::thread::spawn(move || handle(&Arc::new(Conn::new(session))));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}

fn handle(conn: &Arc<Conn>) {
    let result = match conn.recv() {
        Ok(Some(body)) => dispatch(conn, body),
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(message) = result {
        tracing::warn!("{message}");
        let _ = conn.send(guest_message::Body::Error(
            savestate_guest_proto::proto::Error { message },
        ));
    }
    conn.close();
}

fn dispatch(conn: &Arc<Conn>, body: host_message::Body) -> Result<(), String> {
    match body {
        host_message::Body::Hello(_) => conn.send(guest_message::Body::Hello(HelloReply {
            version: env!("CARGO_PKG_VERSION").into(),
            boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap_or_default()
                .trim()
                .into(),
        })),
        host_message::Body::Exec(e) => exec::run(conn, &e),
        host_message::Body::Processes(p) => detached::processes(conn, &p.id),
        host_message::Body::Logs(l) => detached::logs(conn, &l.id, l.follow),
        host_message::Body::ReadFile(r) => files::read(conn, &r.path),
        host_message::Body::WriteFile(w) => files::write(conn, &w),
        host_message::Body::PrepareCheckpoint(_) => {
            fixups::prepare_checkpoint()?;
            conn.send(guest_message::Body::Done(Done {}))
        }
        host_message::Body::Restore(r) => {
            fixups::restore(&r)?;
            conn.send(guest_message::Body::Done(Done {}))
        }
        host_message::Body::Stdin(_) | host_message::Body::Chunk(_) => {
            Err("stdin or chunk without an exec or write_file".into())
        }
    }
}
