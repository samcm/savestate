//! The verbs that reach the guest agent: `exec`, `ps`, `logs` and `cp`. Each runs
//! `savestate guest` (see `launcher::guest`) in the sandbox's VM pod through
//! `pods/exec`, and relays stdin, stdout, stderr and the exit code.

use std::{io::Write as _, os::unix::fs::PermissionsExt as _, path::Path, process::ExitCode};

use anyhow::{Context as _, bail};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    api::{AttachParams, AttachedProcess},
};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio_util::io::SyncIoBridge;

use super::{Exec, Format, Logs, Ps};
use crate::launcher::guest::{FileHeader, ProcessStatus};

/// Buffer between a local tar stream and the exec's stdin or stdout.
const PIPE: usize = 256 * 1024;

pub async fn exec(client: &Client, ns: &str, args: Exec) -> anyhow::Result<ExitCode> {
    let mut command = guest(&["exec"]);
    if let Some(cwd) = args.cwd {
        command.extend(["--cwd".into(), cwd]);
    }
    for (name, value) in args.env {
        command.extend(["--env".into(), format!("{name}={value}")]);
    }
    if args.detach {
        command.push("--detach".into());
    }
    command.push("--".into());
    command.extend(args.command);
    let stdin = args.stdin.then(tokio::io::stdin);
    run(
        client,
        ns,
        &args.sandbox,
        command,
        stdin,
        tokio::io::stdout(),
    )
    .await
}

pub async fn ps(client: &Client, ns: &str, args: &Ps) -> anyhow::Result<()> {
    let mut command = guest(&["ps"]);
    command.extend(args.id.clone());
    let mut out = Vec::new();
    let code = run(
        client,
        ns,
        &args.sandbox,
        command,
        None::<tokio::io::Empty>,
        &mut out,
    )
    .await?;
    if code != ExitCode::SUCCESS {
        bail!("listing the processes in sandbox {} failed", args.sandbox);
    }
    let processes: Vec<ProcessStatus> =
        serde_json::from_slice(&out).context("the guest's process list")?;
    let mut stdout = std::io::stdout().lock();
    match args.output.format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut stdout, &processes)?;
            writeln!(stdout)?;
        }
        Format::Text => {
            let width = processes.iter().map(|p| p.state.len()).max().unwrap_or(0);
            writeln!(
                stdout,
                "{:<16}   {:<width$}   {:<9}   COMMAND",
                "ID", "STATE", "EXIT"
            )?;
            for p in &processes {
                let exit = match (p.exit_code, p.signal) {
                    (Some(code), _) => code.to_string(),
                    (None, Some(signal)) => format!("signal {signal}"),
                    (None, None) => String::new(),
                };
                writeln!(
                    stdout,
                    "{:<16}   {:<width$}   {exit:<9}   {}",
                    p.id, p.state, p.command
                )?;
            }
        }
    }
    Ok(())
}

pub async fn logs(client: &Client, ns: &str, args: &Logs) -> anyhow::Result<ExitCode> {
    let mut command = guest(&["logs", &args.id]);
    if args.follow {
        command.push("--follow".into());
    }
    run(
        client,
        ns,
        &args.sandbox,
        command,
        None::<tokio::io::Empty>,
        tokio::io::stdout(),
    )
    .await
}

/// Copies a file or a directory into or out of a sandbox.
pub async fn cp(client: &Client, ns: &str, args: &super::Cp) -> anyhow::Result<ExitCode> {
    match (remote(&args.source), remote(&args.destination)) {
        (Some((sandbox, path)), None) => {
            download(client, ns, sandbox, path, &args.destination, args.mode).await
        }
        (None, Some((sandbox, path))) => {
            upload(client, ns, &args.source, sandbox, path, args.mode).await
        }
        _ => bail!("name exactly one of the source and destination as SANDBOX:PATH"),
    }
}

async fn upload(
    client: &Client,
    ns: &str,
    source: &str,
    sandbox: &str,
    path: &str,
    mode: Option<u32>,
) -> anyhow::Result<ExitCode> {
    let meta = tokio::fs::metadata(source)
        .await
        .with_context(|| format!("reading {source}"))?;
    if !meta.is_dir() {
        let file = tokio::fs::File::open(source)
            .await
            .with_context(|| format!("opening {source}"))?;
        let mode = mode.unwrap_or(meta.permissions().mode() & 0o7777);
        let command = guest(&["write", path, "--mode", &format!("{mode:o}")]);
        return run(
            client,
            ns,
            sandbox,
            command,
            Some(file),
            tokio::io::stdout(),
        )
        .await;
    }
    if mode.is_some() {
        bail!("--mode applies to a file; {source} is a directory");
    }
    // The directory as a tar stream, packed on a blocking thread into the exec's stdin.
    let (tar_out, stdin) = tokio::io::duplex(PIPE);
    let writer = SyncIoBridge::new(tar_out);
    let source = source.to_owned();
    let packer = tokio::task::spawn_blocking(move || {
        let mut tar = tar::Builder::new(writer);
        tar.follow_symlinks(false);
        tar.append_dir_all(".", &source)?;
        tar.into_inner()?.shutdown()
    });
    let command = guest(&["write", path, "--directory"]);
    let code = run(
        client,
        ns,
        sandbox,
        command,
        Some(stdin),
        tokio::io::stdout(),
    )
    .await?;
    // The exec has dropped its end, so the packer cannot be stuck writing.
    packer.await?.context("packing the directory")?;
    Ok(code)
}

async fn download(
    client: &Client,
    ns: &str,
    sandbox: &str,
    path: &str,
    destination: &str,
    mode: Option<u32>,
) -> anyhow::Result<ExitCode> {
    let (stdout, received) = tokio::io::duplex(PIPE);
    let command = guest(&["read", path]);
    let fetch = run(
        client,
        ns,
        sandbox,
        command,
        None::<tokio::io::Empty>,
        stdout,
    );
    let (code, ()) = tokio::try_join!(fetch, save(received, Path::new(destination), mode))?;
    Ok(code)
}

/// Writes what `savestate guest read` sends to `destination`: a file, or a directory
/// unpacked from its tar stream. Nothing arrives when the guest could not read it.
async fn save(
    received: tokio::io::DuplexStream,
    destination: &Path,
    mode: Option<u32>,
) -> anyhow::Result<()> {
    let mut received = BufReader::new(received);
    let mut line = String::new();
    if received.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let header: FileHeader = serde_json::from_str(&line).context("the guest's file header")?;
    let shown = destination.display();
    if header.directory {
        if mode.is_some() {
            bail!("--mode applies to a file; the source is a directory");
        }
        tokio::fs::create_dir_all(destination)
            .await
            .with_context(|| format!("creating {shown}"))?;
        let mut reader = SyncIoBridge::new(received);
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut tar = tar::Archive::new(&mut reader);
            tar.set_preserve_permissions(true);
            tar.set_preserve_mtime(true);
            tar.unpack(&destination)?;
            // The padding after the archive's end, so the exec is never stuck.
            std::io::copy(&mut reader, &mut std::io::sink()).map(drop)
        })
        .await?
        .with_context(|| format!("unpacking into {shown}"))?;
        return Ok(());
    }
    if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = tokio::fs::File::create(destination)
        .await
        .with_context(|| format!("creating {shown}"))?;
    tokio::io::copy(&mut received, &mut file)
        .await
        .with_context(|| format!("writing {shown}"))?;
    let mode = mode.unwrap_or(header.mode);
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .await
        .with_context(|| format!("setting the mode of {shown}"))?;
    Ok(())
}

/// `savestate guest` and `args`.
fn guest(args: &[&str]) -> Vec<String> {
    ["savestate", "guest"]
        .iter()
        .chain(args)
        .map(ToString::to_string)
        .collect()
}

/// Runs `command` in the sandbox's VM pod, copying `stdin` in and its output to
/// `stdout` and stderr; its exit code.
async fn run(
    client: &Client,
    ns: &str,
    sandbox: &str,
    command: Vec<String>,
    stdin: Option<impl AsyncRead + Unpin>,
    mut stdout: impl AsyncWrite + Unpin,
) -> anyhow::Result<ExitCode> {
    let pods = Api::<Pod>::namespaced(client.clone(), ns);
    let params = AttachParams::default()
        .container("launcher")
        .stdin(stdin.is_some())
        .stdout(true)
        .stderr(true);
    let mut process: AttachedProcess = pods
        .exec(sandbox, command, &params)
        .await
        .with_context(|| format!("exec in sandbox {sandbox}"))?;
    let status = process.take_status().context("exec status")?;
    let mut remote_out = process.stdout().context("exec stdout")?;
    let mut remote_err = process.stderr().context("exec stderr")?;
    let input = async {
        if let (Some(mut local), Some(mut remote)) = (stdin, process.stdin()) {
            tokio::io::copy(&mut local, &mut remote).await?;
            // Dropping `remote` closes the command's stdin.
        }
        Ok::<_, std::io::Error>(())
    };
    let output = async {
        tokio::io::copy(&mut remote_out, &mut stdout).await?;
        stdout.flush().await?;
        // Ends a pipe such as `cp`'s, so its reader sees the end.
        stdout.shutdown().await
    };
    let mut local_err = tokio::io::stderr();
    let errors = tokio::io::copy(&mut remote_err, &mut local_err);
    let (input, output, errors) = tokio::join!(input, output, errors);
    input?;
    output?;
    errors?;
    let status = status.await.context("the exec ended without a status")?;
    if status.status.as_deref() == Some("Success") {
        return Ok(ExitCode::SUCCESS);
    }
    let code = status
        .details
        .as_ref()
        .and_then(|details| details.causes.as_ref())
        .and_then(|causes| {
            causes
                .iter()
                .find(|c| c.reason.as_deref() == Some("ExitCode"))
        })
        .and_then(|cause| cause.message.as_ref()?.parse::<u8>().ok());
    match code {
        Some(code) => Ok(ExitCode::from(code)),
        None => bail!(
            "exec in sandbox {sandbox}: {}",
            status.message.unwrap_or_default()
        ),
    }
}

/// `NAME:PATH` as (`NAME`, `PATH`); a local path otherwise (`./a:b` is local).
fn remote(arg: &str) -> Option<(&str, &str)> {
    let (name, path) = arg.split_once(':')?;
    (!name.is_empty() && !name.contains('/') && path.starts_with('/')).then_some((name, path))
}
