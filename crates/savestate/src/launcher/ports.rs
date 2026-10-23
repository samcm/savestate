//! `savestate launcher-ports`: keeps the guest's exposed ports in step with the
//! sandbox's `spec.ports` (see "Exposed ports" in `docs/design.md`). The controller
//! writes them to the VM pod's [`ANNOTATION`], which the pod sees as [`FILE`]
//! through a downward API volume; this process, started by the launcher as root
//! before it drops its privileges, reads that file every second and replaces the
//! nftables set of exposed ports whenever it changes. It does nothing else.

use std::{fmt::Write as _, path::PathBuf, process::ExitCode, time::Duration};

use savestate_api::Protocol;

use super::net;

/// The VM pod's annotation listing the exposed ports, one `PORT/tcp` or `PORT/udp`
/// per line.
pub const ANNOTATION: &str = "savestate.dev/ports";
/// Where the VM pod's downward API volume puts [`ANNOTATION`].
pub const FILE: &str = "/etc/savestate/pod/ports";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// File listing the exposed ports (the pod's `savestate.dev/ports` annotation).
    #[arg(long, default_value = FILE)]
    pub file: PathBuf,
}

/// Follows the file until the pod stops.
pub fn run(args: &Args) -> ExitCode {
    crate::telemetry::init_logging();
    let mut applied = None;
    let mut failed = None;
    loop {
        let result = std::fs::read_to_string(&args.file)
            .map_err(anyhow::Error::from)
            .and_then(|text| {
                let ports = parse(&text);
                if applied.as_ref() != Some(&ports) {
                    net::expose(&ports)?;
                    tracing::info!(ports = %text.trim().replace('\n', " "), "exposed ports");
                    applied = Some(ports);
                }
                Ok(())
            });
        // Each failure once, not every second; the next pass tries again.
        let error = result.err().map(|error| format!("{error:#}"));
        if error.is_some() && error != failed {
            tracing::error!(error, file = %args.file.display(), "exposing ports failed");
        }
        failed = error;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The annotation's text as the format the controller writes.
pub fn format(ports: &[savestate_api::Port]) -> String {
    ports.iter().fold(String::new(), |mut out, p| {
        let _ = writeln!(out, "{}/{}", p.port, p.protocol.as_str());
        out
    })
}

/// `PORT/PROTOCOL` lines, sorted; lines that are neither are skipped.
fn parse(text: &str) -> Vec<(Protocol, u16)> {
    let mut ports: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (port, protocol) = line.trim().split_once('/')?;
            let protocol = match protocol {
                "tcp" => Protocol::Tcp,
                "udp" => Protocol::Udp,
                _ => return None,
            };
            Some((protocol, port.parse().ok()?))
        })
        .collect();
    ports.sort_by_key(|&(protocol, port)| (protocol.as_str(), port));
    ports.dedup();
    ports
}
