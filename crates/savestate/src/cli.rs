//! The user-facing verbs. Each is shorthand over the `Sandbox` and `Checkpoint`
//! objects (see "API" in `docs/design.md`); `exec` and `cp` run `savestate guest` in
//! the sandbox's VM pod through `pods/exec`. They use the current kubeconfig context.

use std::{
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand, ValueEnum};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client, ResourceExt,
    api::{AttachParams, DeleteParams, ListParams, PostParams},
};
use savestate_api::{Checkpoint, CheckpointPhase, Sandbox, SandboxPhase, Transfer, TransferPhase};
use serde_json::json;

#[derive(Subcommand)]
pub enum Verb {
    /// Create a sandbox from a disk image or a checkpoint, and wait until it runs.
    Create(Create),
    /// Take a checkpoint of a running sandbox, and wait until it is ready; the
    /// sandbox pauses for a few seconds, then carries on.
    Checkpoint(CheckpointArgs),
    /// Checkpoint a sandbox and start sandboxes from that checkpoint.
    Fork(Fork),
    /// Run a command in a sandbox; exits with its exit code.
    Exec(Exec),
    /// Copy a file into or out of a sandbox (`NAME:PATH` names a path in a sandbox).
    Cp(Cp),
    /// Delete a sandbox or a checkpoint.
    Delete(Delete),
}

#[derive(Args)]
pub struct Common {
    /// Namespace [default: the kubeconfig context's].
    #[arg(long, env = "SAVESTATE_NAMESPACE")]
    pub namespace: Option<String>,
    /// How long to wait for sandboxes to run and checkpoints to be ready, in seconds.
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
}

#[derive(Args)]
pub struct Create {
    /// Sandbox name.
    pub name: String,
    /// Disk image to boot, e.g. `ghcr.io/example/base@sha256:…`.
    #[arg(long, required_unless_present = "from", conflicts_with = "from")]
    pub image: Option<String>,
    /// Checkpoint to start from; image, vCPUs, memory and disk come from it.
    #[arg(long)]
    pub from: Option<String>,
    /// Virtual CPUs (with `--image`).
    #[arg(long, required_unless_present = "from", conflicts_with = "from")]
    pub vcpus: Option<u32>,
    /// Guest memory, e.g. `8Gi` (with `--image`).
    #[arg(long, required_unless_present = "from", conflicts_with = "from")]
    pub memory: Option<String>,
    /// Disk size, e.g. `60Gi` (with `--image`).
    #[arg(long, required_unless_present = "from", conflicts_with = "from")]
    pub disk: Option<String>,
    /// Guest clock after starting from a checkpoint (with `--from`) [default: frozen].
    #[arg(long, value_enum, requires = "from")]
    pub clock: Option<Clock>,
    /// Delete the sandbox after this long, e.g. `7d`.
    #[arg(long)]
    pub ttl: Option<String>,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Clock {
    /// The guest believes no time passed while it was saved.
    Frozen,
    /// The guest's wall clock is set to the host's current time after the restore.
    Stepped,
}

impl Clock {
    fn as_str(self) -> &'static str {
        match self {
            Self::Frozen => "frozen",
            Self::Stepped => "stepped",
        }
    }
}

#[derive(Args)]
pub struct CheckpointArgs {
    /// Sandbox to checkpoint.
    pub sandbox: String,
    /// Checkpoint name [default: generated from the sandbox name].
    #[arg(long)]
    pub name: Option<String>,
    /// Delete the checkpoint after this long, e.g. `48h`.
    #[arg(long)]
    pub ttl: Option<String>,
    /// Export it to this object store (from the installation config), and wait
    /// until it is there too.
    #[arg(long)]
    pub export: Option<String>,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args)]
pub struct Fork {
    /// Sandbox to fork.
    pub sandbox: String,
    /// Number of new sandboxes, named `<checkpoint>-1` to `<checkpoint>-N`.
    #[arg(short = 'n', long, default_value_t = 1)]
    pub count: u32,
    /// Guest clock of the new sandboxes [default: frozen].
    #[arg(long, value_enum)]
    pub clock: Option<Clock>,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args)]
pub struct Exec {
    /// Sandbox to run the command in.
    pub sandbox: String,
    /// Pass this process's stdin to the command.
    #[arg(short = 'i', long)]
    pub stdin: bool,
    /// Command and arguments.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args)]
pub struct Cp {
    /// Source: a local file or `NAME:PATH`.
    pub source: String,
    /// Destination: a local file or `NAME:PATH`.
    pub destination: String,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args)]
pub struct Delete {
    pub kind: Kind,
    pub name: String,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    Sandbox,
    Checkpoint,
}

/// Run one verb against the cluster in the current kubeconfig context; the process's
/// exit code.
pub fn main(verb: Verb) -> ExitCode {
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run(verb)));
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

pub async fn run(verb: Verb) -> anyhow::Result<ExitCode> {
    let client = Client::try_default()
        .await
        .context("connecting to the cluster in the current kubeconfig context")?;
    match verb {
        Verb::Create(args) => {
            let ns = namespace(&client, &args.common);
            let spec = if let Some(from) = &args.from {
                json!({"from": {"checkpoint": from}, "clock": args.clock.map(Clock::as_str), "ttl": args.ttl})
            } else {
                json!({"image": args.image, "vcpus": args.vcpus, "memory": args.memory, "disk": args.disk, "ttl": args.ttl})
            };
            create_sandbox(&client, &ns, &args.name, spec).await?;
            let started = Instant::now();
            let node = wait_running(&client, &ns, &args.name, args.common.timeout).await?;
            say(&format!(
                "sandbox {} is Running on node {node} ({:.1} s)",
                args.name,
                started.elapsed().as_secs_f64()
            ))?;
        }
        Verb::Checkpoint(args) => {
            let ns = namespace(&client, &args.common);
            let started = Instant::now();
            let name = checkpoint(
                &client,
                &ns,
                &args.sandbox,
                args.name.as_deref(),
                args.ttl.as_deref(),
                args.export.as_deref(),
                args.common.timeout,
            )
            .await?;
            say(&format!(
                "checkpoint {name} of {} is Ready ({:.1} s)",
                args.sandbox,
                started.elapsed().as_secs_f64()
            ))?;
            if let Some(store) = &args.export {
                let started = Instant::now();
                wait_exported(&client, &ns, &name, store, args.common.timeout).await?;
                say(&format!(
                    "checkpoint {name} is in store {store} ({:.1} s)",
                    started.elapsed().as_secs_f64()
                ))?;
            }
        }
        Verb::Fork(args) => fork(&client, &args).await?,
        Verb::Exec(args) => {
            let ns = namespace(&client, &args.common);
            let mut command = vec![
                "savestate".to_owned(),
                "guest".into(),
                "exec".into(),
                "--".into(),
            ];
            command.extend(args.command);
            let stdin = args.stdin.then(tokio::io::stdin);
            return exec(
                &client,
                &ns,
                &args.sandbox,
                command,
                stdin,
                tokio::io::stdout(),
            )
            .await;
        }
        Verb::Cp(args) => {
            let ns = namespace(&client, &args.common);
            return cp(&client, &ns, &args.source, &args.destination).await;
        }
        Verb::Delete(args) => {
            let ns = namespace(&client, &args.common);
            match args.kind {
                Kind::Sandbox => {
                    Api::<Sandbox>::namespaced(client, &ns)
                        .delete(&args.name, &DeleteParams::default())
                        .await?;
                }
                Kind::Checkpoint => {
                    Api::<Checkpoint>::namespaced(client, &ns)
                        .delete(&args.name, &DeleteParams::default())
                        .await?;
                }
            }
            say(&format!("deleted {}", args.name))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// One checkpoint of `args.sandbox`, then `args.count` sandboxes from it.
async fn fork(client: &Client, args: &Fork) -> anyhow::Result<()> {
    let ns = namespace(client, &args.common);
    let started = Instant::now();
    let name = checkpoint(
        client,
        &ns,
        &args.sandbox,
        None,
        None,
        None,
        args.common.timeout,
    )
    .await?;
    say(&format!("checkpoint {name} of {} is Ready", args.sandbox))?;
    let names: Vec<String> = (1..=args.count).map(|i| format!("{name}-{i}")).collect();
    for sandbox in &names {
        let spec = json!({"from": {"checkpoint": name}, "clock": args.clock.map(Clock::as_str)});
        create_sandbox(client, &ns, sandbox, spec).await?;
    }
    for sandbox in &names {
        let node = wait_running(client, &ns, sandbox, args.common.timeout).await?;
        say(&format!("sandbox {sandbox} is Running on node {node}"))?;
    }
    say(&format!(
        "forked {} into {} sandboxes ({:.1} s)",
        args.sandbox,
        names.len(),
        started.elapsed().as_secs_f64()
    ))?;
    Ok(())
}

fn namespace(client: &Client, common: &Common) -> String {
    common
        .namespace
        .clone()
        .unwrap_or_else(|| client.default_namespace().to_owned())
}

fn say(line: &str) -> anyhow::Result<()> {
    writeln!(std::io::stdout().lock(), "{line}")?;
    Ok(())
}

async fn create_sandbox(
    client: &Client,
    ns: &str,
    name: &str,
    mut spec: serde_json::Value,
) -> anyhow::Result<()> {
    if let Some(spec) = spec.as_object_mut() {
        spec.retain(|_, value| !value.is_null());
    }
    let sandbox: Sandbox = serde_json::from_value(json!({
        "apiVersion": "savestate.dev/v1alpha1",
        "kind": "Sandbox",
        "metadata": {"name": name},
        "spec": spec,
    }))?;
    Api::<Sandbox>::namespaced(client.clone(), ns)
        .create(&PostParams::default(), &sandbox)
        .await
        .with_context(|| format!("creating sandbox {name}"))?;
    Ok(())
}

/// Waits until the sandbox runs; returns its node.
async fn wait_running(
    client: &Client,
    ns: &str,
    name: &str,
    timeout: u64,
) -> anyhow::Result<String> {
    let sandboxes = Api::<Sandbox>::namespaced(client.clone(), ns);
    poll(timeout, &format!("sandbox {name} to run"), || async {
        let status = sandboxes.get(name).await?.status.unwrap_or_default();
        match status.phase {
            SandboxPhase::Running => Ok(Some(status.node.unwrap_or_default())),
            SandboxPhase::Failed => bail!(
                "sandbox {name} failed: {}",
                status.message.unwrap_or_default()
            ),
            _ => Ok(None),
        }
    })
    .await
}

/// Creates a checkpoint of `sandbox` (named, or generated from the sandbox's name)
/// once the sandbox runs, and waits until it is ready; returns its name.
async fn checkpoint(
    client: &Client,
    ns: &str,
    sandbox: &str,
    name: Option<&str>,
    ttl: Option<&str>,
    export: Option<&str>,
    timeout: u64,
) -> anyhow::Result<String> {
    wait_running(client, ns, sandbox, timeout).await?;
    let metadata = match name {
        Some(name) => json!({"name": name}),
        None => json!({"generateName": format!("{sandbox}-")}),
    };
    let mut spec = json!({"sandbox": sandbox});
    if let Some(ttl) = ttl {
        spec["ttl"] = json!(ttl);
    }
    if let Some(store) = export {
        spec["export"] = json!(store);
    }
    let checkpoint: Checkpoint = serde_json::from_value(json!({
        "apiVersion": "savestate.dev/v1alpha1",
        "kind": "Checkpoint",
        "metadata": metadata,
        "spec": spec,
    }))?;
    let checkpoints = Api::<Checkpoint>::namespaced(client.clone(), ns);
    let name = checkpoints
        .create(&PostParams::default(), &checkpoint)
        .await
        .context("creating the checkpoint")?
        .name_any();
    poll(
        timeout,
        &format!("checkpoint {name} to be ready"),
        || async {
            let status = checkpoints.get(&name).await?.status.unwrap_or_default();
            match status.phase {
                CheckpointPhase::Ready => Ok(Some(())),
                CheckpointPhase::Failed => bail!(
                    "checkpoint {name} failed: {}",
                    status.message.unwrap_or_default()
                ),
                _ => Ok(None),
            }
        },
    )
    .await?;
    Ok(name)
}

/// Waits until checkpoint `name` is in `store`; fails with its export's message if
/// the export fails.
async fn wait_exported(
    client: &Client,
    ns: &str,
    name: &str,
    store: &str,
    timeout: u64,
) -> anyhow::Result<()> {
    let checkpoints = Api::<Checkpoint>::namespaced(client.clone(), ns);
    let transfers = Api::<Transfer>::namespaced(client.clone(), ns);
    poll(
        timeout,
        &format!("checkpoint {name} to be in store {store}"),
        || async {
            let status = checkpoints.get(name).await?.status.unwrap_or_default();
            if status.stores.iter().any(|held| held == store) {
                return Ok(Some(()));
            }
            let exports = ListParams::default().fields(&format!("spec.checkpoint={name}"));
            let export = transfers
                .list(&exports)
                .await?
                .items
                .into_iter()
                .find(|transfer| transfer.spec.to.store.as_deref() == Some(store));
            match export.and_then(|transfer| transfer.status) {
                Some(status) if status.phase == TransferPhase::Failed => bail!(
                    "exporting checkpoint {name} to store {store} failed: {}",
                    status.message.unwrap_or_default()
                ),
                _ => Ok(None),
            }
        },
    )
    .await
}

async fn poll<T, F>(timeout: u64, what: &str, check: impl Fn() -> F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        if let Some(value) = check().await? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Runs `command` in the sandbox's VM pod, copying `stdin` in and its output to
/// `stdout` and stderr; its exit code.
async fn exec(
    client: &Client,
    ns: &str,
    sandbox: &str,
    command: Vec<String>,
    stdin: Option<impl tokio::io::AsyncRead + Unpin>,
    mut stdout: impl tokio::io::AsyncWrite + Unpin,
) -> anyhow::Result<ExitCode> {
    let pods = Api::<Pod>::namespaced(client.clone(), ns);
    let params = AttachParams::default()
        .container("launcher")
        .stdin(stdin.is_some())
        .stdout(true)
        .stderr(true);
    let mut process = pods
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
        tokio::io::AsyncWriteExt::flush(&mut stdout).await
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

/// Copies one file into or out of a sandbox.
async fn cp(
    client: &Client,
    ns: &str,
    source: &str,
    destination: &str,
) -> anyhow::Result<ExitCode> {
    match (remote(source), remote(destination)) {
        (Some((sandbox, path)), None) => {
            let file = tokio::fs::File::create(destination)
                .await
                .with_context(|| format!("creating {destination}"))?;
            let command = ["savestate", "guest", "read", path]
                .map(String::from)
                .into();
            exec(client, ns, sandbox, command, None::<tokio::io::Empty>, file).await
        }
        (None, Some((sandbox, path))) => {
            let file = tokio::fs::File::open(source)
                .await
                .with_context(|| format!("opening {source}"))?;
            let mode = file.metadata().await?.permissions().mode() & 0o7777;
            let command = [
                "savestate",
                "guest",
                "write",
                path,
                "--mode",
                &format!("{mode:o}"),
            ]
            .map(String::from)
            .into();
            exec(
                client,
                ns,
                sandbox,
                command,
                Some(file),
                tokio::io::stdout(),
            )
            .await
        }
        _ => bail!("name exactly one of the source and destination as SANDBOX:PATH"),
    }
}

/// `NAME:PATH` as (`NAME`, `PATH`); a local path otherwise (`./a:b` is local).
fn remote(arg: &str) -> Option<(&str, &str)> {
    let (name, path) = arg.split_once(':')?;
    (!name.is_empty() && !name.contains('/') && path.starts_with('/')).then_some((name, path))
}
