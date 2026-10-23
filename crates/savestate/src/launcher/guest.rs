//! `savestate guest`: runs inside a VM pod (started there through `pods/exec` by the
//! CLI's `exec`, `ps`, `logs` and `cp`) and relays its stdin, stdout and exit code to
//! the guest agent over vsock. Its output is for the CLI, which runs the same build.

use std::{io::Write as _, path::PathBuf, process::ExitCode};

use anyhow::Context as _;
use clap::Subcommand;
use savestate_guest_proto::{
    Client, ExitStatus,
    proto::{EnvVar, Exec, Process, exited},
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;

#[derive(Subcommand, Debug)]
pub enum Guest {
    /// Run a command in the guest with this process's stdin, stdout and stderr; exit
    /// with its exit code (128 + signal when it was killed). With `--detach`, start it
    /// as a systemd unit and print its id.
    Exec {
        /// Working directory in the guest [default: /].
        #[arg(long)]
        cwd: Option<String>,
        /// Environment variable `NAME=VALUE`; repeatable.
        #[arg(long, value_parser = parse_env)]
        env: Vec<(String, String)>,
        #[arg(long)]
        detach: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Print the detached processes (or the one with ID) as a JSON array of
    /// [`ProcessStatus`].
    Ps { id: Option<String> },
    /// Write the output of detached process ID so far to stdout and stderr; with
    /// `--follow`, until it exits, then exit with its exit code.
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
    },
    /// Write a [`FileHeader`] line, then the guest file PATH (a directory as a tar
    /// stream) to stdout.
    Read { path: String },
    /// Replace the guest file PATH with stdin, or with `--directory` unpack the tar
    /// stream on stdin into the directory PATH.
    Write {
        path: String,
        /// Permission bits, octal.
        #[arg(long, default_value = "644", value_parser = parse_mode)]
        mode: u32,
        #[arg(long)]
        directory: bool,
    },
}

/// What `savestate guest read` writes as its first line, as JSON.
#[derive(Serialize, Deserialize)]
pub struct FileHeader {
    pub directory: bool,
    pub mode: u32,
}

/// A detached process, as `savestate guest ps` prints it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStatus {
    pub id: String,
    pub command: String,
    /// `running` or `exited`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that killed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

impl From<Process> for ProcessStatus {
    fn from(p: Process) -> Self {
        let status = p.exited.and_then(|e| e.status);
        Self {
            id: p.id,
            command: p.command,
            state: if status.is_some() {
                "exited"
            } else {
                "running"
            }
            .into(),
            exit_code: match status {
                Some(exited::Status::Code(code)) => Some(code),
                _ => None,
            },
            signal: match status {
                Some(exited::Status::Signal(signal)) => Some(signal),
                _ => None,
            },
        }
    }
}

fn parse_mode(mode: &str) -> Result<u32, String> {
    u32::from_str_radix(mode, 8).map_err(|error| error.to_string())
}

pub fn parse_env(var: &str) -> Result<(String, String), String> {
    match var.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.to_owned(), value.to_owned())),
        _ => Err(format!("expected NAME=VALUE, got {var:?}")),
    }
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
        Guest::Exec {
            cwd,
            env,
            detach,
            command,
        } => {
            let exec = Exec {
                argv: command,
                env: env
                    .into_iter()
                    .map(|(name, value)| EnvVar { name, value })
                    .collect(),
                cwd: cwd.unwrap_or_default(),
                detach: false,
            };
            if detach {
                let id = client.detach(exec).await?;
                writeln!(std::io::stdout().lock(), "{id}")?;
                return Ok(ExitCode::SUCCESS);
            }
            let status = client
                .exec(
                    exec,
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                    tokio::io::stderr(),
                )
                .await?;
            Ok(exit_code(status))
        }
        Guest::Ps { id } => {
            let processes: Vec<ProcessStatus> = client
                .processes(id.as_deref().unwrap_or_default())
                .await?
                .into_iter()
                .map(ProcessStatus::from)
                .collect();
            let mut out = std::io::stdout().lock();
            serde_json::to_writer(&mut out, &processes)?;
            writeln!(out)?;
            Ok(ExitCode::SUCCESS)
        }
        Guest::Logs { id, follow } => {
            let status = client
                .logs(&id, follow, tokio::io::stdout(), tokio::io::stderr())
                .await?;
            Ok(match status {
                Some(status) if follow => exit_code(status),
                _ => ExitCode::SUCCESS,
            })
        }
        Guest::Read { path } => {
            let download = client
                .read_file(&path)
                .await
                .with_context(|| format!("reading {path}"))?;
            let header = FileHeader {
                directory: download.info.directory,
                mode: download.info.mode,
            };
            let mut line = serde_json::to_vec(&header)?;
            line.push(b'\n');
            let mut stdout = tokio::io::stdout();
            stdout.write_all(&line).await?;
            download
                .copy_to(stdout)
                .await
                .with_context(|| format!("reading {path}"))?;
            Ok(ExitCode::SUCCESS)
        }
        Guest::Write {
            path,
            mode,
            directory,
        } => {
            if directory {
                client.write_directory(&path, tokio::io::stdin()).await
            } else {
                client.write_file(&path, mode, tokio::io::stdin()).await
            }
            .with_context(|| format!("writing {path}"))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// A guest program's exit status as this process's: its code, or 128 + the signal.
fn exit_code(status: ExitStatus) -> ExitCode {
    match status {
        ExitStatus::Code(code) => ExitCode::from(u8::try_from(code).unwrap_or(255)),
        ExitStatus::Signal(signal) => ExitCode::from(u8::try_from(128 + signal).unwrap_or(255)),
    }
}
