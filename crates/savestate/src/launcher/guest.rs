//! `savestate guest`: runs inside a VM pod (started there through `pods/exec` by the
//! CLI's `exec` and `cp`) and relays its stdin, stdout and exit code to the guest
//! agent over vsock.

use std::{path::PathBuf, process::ExitCode};

use anyhow::Context as _;
use clap::Subcommand;
use savestate_guest_proto::{Client, ExitStatus, proto::Exec};

#[derive(Subcommand, Debug)]
pub enum Guest {
    /// Run a command in the guest with this process's stdin, stdout and stderr; exit
    /// with its exit code (128 + signal when it was killed).
    Exec {
        /// Working directory in the guest [default: /].
        #[arg(long)]
        cwd: Option<String>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Write the guest file PATH to stdout.
    Read { path: String },
    /// Replace the guest file PATH with stdin.
    Write {
        path: String,
        /// Permission bits, octal.
        #[arg(long, default_value = "644", value_parser = parse_mode)]
        mode: u32,
    },
}

fn parse_mode(mode: &str) -> Result<u32, String> {
    u32::from_str_radix(mode, 8).map_err(|error| error.to_string())
}

pub fn run(command: Guest) -> ExitCode {
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(relay(command)));
    match result {
        Ok(code) => code,
        Err(error) => {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr().lock(), "savestate guest: {error:#}");
            ExitCode::from(255)
        }
    }
}

async fn relay(command: Guest) -> anyhow::Result<ExitCode> {
    let run_dir = PathBuf::from(super::RUN_DIR);
    if !run_dir.join(super::READY).exists() {
        anyhow::bail!("the VM is not running");
    }
    let client = Client::vsock(super::read_cid(&run_dir)?);
    match command {
        Guest::Exec { cwd, command } => {
            let exec = Exec {
                argv: command,
                cwd: cwd.unwrap_or_default(),
                ..Exec::default()
            };
            let status = client
                .exec(
                    exec,
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                    tokio::io::stderr(),
                )
                .await?;
            Ok(match status {
                ExitStatus::Code(code) => ExitCode::from(u8::try_from(code).unwrap_or(255)),
                ExitStatus::Signal(signal) => {
                    ExitCode::from(u8::try_from(128 + signal).unwrap_or(255))
                }
            })
        }
        Guest::Read { path } => {
            client
                .read_file(&path, tokio::io::stdout())
                .await
                .with_context(|| format!("reading {path}"))?;
            Ok(ExitCode::SUCCESS)
        }
        Guest::Write { path, mode } => {
            client
                .write_file(&path, mode, tokio::io::stdin())
                .await
                .with_context(|| format!("writing {path}"))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
