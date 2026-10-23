//! The user-facing verbs. Each is shorthand over the `Sandbox` and `Checkpoint`
//! objects (see "API" in `docs/design.md`); `exec` and `cp` run `savestate guest` in
//! the sandbox's VM pod through `pods/exec`. They use the current kubeconfig context.

mod show;

use std::{
    collections::BTreeMap,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    process::ExitCode,
    str::FromStr as _,
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand, ValueEnum};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{AttachParams, DeleteParams, ListParams, PostParams},
};
use savestate_api::{
    Checkpoint, CheckpointPhase, Sandbox, SandboxPhase, Transfer, TransferPhase, Ttl,
};
use serde::{Serialize, de::DeserializeOwned};
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
    /// List sandboxes or checkpoints.
    List(List),
    /// Show a sandbox or a checkpoint: phase, node, message, shape, sizes and parent.
    Get(Get),
    /// Delete a sandbox or a checkpoint.
    Delete(Delete),
}

#[derive(Args)]
pub struct Common {
    /// Namespace [default: the kubeconfig context's].
    #[arg(long, env = "SAVESTATE_NAMESPACE")]
    pub namespace: Option<String>,
}

#[derive(Args)]
pub struct Wait {
    /// Return once the objects are created, without waiting for sandboxes to run or
    /// checkpoints to be ready.
    #[arg(long)]
    pub no_wait: bool,
    /// How long to wait in all, e.g. `90s` or `10m`; past it, exit non-zero.
    #[arg(long, default_value = "10m", value_parser = parse_duration)]
    pub timeout: Duration,
}

#[derive(Args)]
pub struct Output {
    /// Output format: text for people, or the objects as JSON.
    #[arg(short = 'o', long = "output", value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Json,
}

#[derive(Args)]
pub struct Create {
    /// Sandbox name.
    pub name: String,
    /// Disk image to boot, e.g. `ghcr.io/example/base@sha256:…`.
    #[arg(long, required_unless_present = "from", conflicts_with = "from")]
    pub image: Option<String>,
    /// Checkpoint to start from; image, vCPUs, memory, disk and labels come from it.
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
    /// A label, `KEY=VALUE`, on top of the checkpoint's (with `--from`); repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub wait: Wait,
    #[command(flatten)]
    pub output: Output,
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
    /// Sandbox to checkpoint; once it runs.
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
    /// A label, `KEY=VALUE`, on top of the sandbox's; repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub wait: Wait,
    #[command(flatten)]
    pub output: Output,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args)]
pub struct Fork {
    /// Sandbox to fork; once it runs. The checkpoint gets its labels.
    pub sandbox: String,
    /// Number of new sandboxes, named `<checkpoint>-1` to `<checkpoint>-N`.
    #[arg(short = 'n', long, default_value_t = 1)]
    pub count: u32,
    /// Guest clock of the new sandboxes [default: frozen].
    #[arg(long, value_enum)]
    pub clock: Option<Clock>,
    /// A label, `KEY=VALUE`, for the new sandboxes on top of the checkpoint's;
    /// repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub wait: Wait,
    #[command(flatten)]
    pub output: Output,
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
pub struct List {
    pub kind: Kind,
    /// Only objects whose labels match this selector, e.g. `app=web,tier!=test`.
    #[arg(short = 'l', long)]
    pub selector: Option<String>,
    #[command(flatten)]
    pub output: Output,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args)]
pub struct Get {
    /// Name of a sandbox or a checkpoint.
    pub name: String,
    /// Which, when a sandbox and a checkpoint have the same name.
    #[arg(long, value_enum)]
    pub kind: Option<Kind>,
    #[command(flatten)]
    pub output: Output,
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    #[value(alias = "sandboxes", alias = "sbx")]
    Sandbox,
    #[value(alias = "checkpoints", alias = "ckpt")]
    Checkpoint,
}

fn parse_label(label: &str) -> Result<(String, String), String> {
    match label.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("expected KEY=VALUE, got {label:?}")),
    }
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    Ttl::from_str(text)
        .map(|ttl| ttl.duration())
        .map_err(|_| format!("expected a duration such as 90s, 10m or 1h30m, got {text:?}"))
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
        Verb::Create(args) => create(&client, args).await?,
        Verb::Checkpoint(args) => take_checkpoint(&client, args).await?,
        Verb::Fork(args) => fork(&client, args).await?,
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
        Verb::List(args) => {
            let ns = namespace(&client, &args.common);
            let mut params = ListParams::default();
            if let Some(selector) = &args.selector {
                params = params.labels(selector);
            }
            match args.kind {
                Kind::Sandbox => {
                    let items = Api::<Sandbox>::namespaced(client, &ns)
                        .list(&params)
                        .await?
                        .items;
                    match args.output.format {
                        Format::Text => show::sandboxes(&items)?,
                        Format::Json => {
                            print_json(&items.into_iter().map(bare).collect::<Vec<_>>())?;
                        }
                    }
                }
                Kind::Checkpoint => {
                    let items = Api::<Checkpoint>::namespaced(client, &ns)
                        .list(&params)
                        .await?
                        .items;
                    match args.output.format {
                        Format::Text => show::checkpoints(&items)?,
                        Format::Json => {
                            print_json(&items.into_iter().map(bare).collect::<Vec<_>>())?;
                        }
                    }
                }
            }
        }
        Verb::Get(args) => get(&client, &args).await?,
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

async fn create(client: &Client, args: Create) -> anyhow::Result<()> {
    let ns = namespace(client, &args.common);
    let started = Instant::now();
    let mut labels = BTreeMap::new();
    let spec = if let Some(from) = &args.from {
        // A sandbox from a checkpoint carries the checkpoint's labels, so parking and
        // resuming keeps them. The controller waits for a checkpoint that does not
        // exist yet; it has no labels to give.
        if let Some(checkpoint) = Api::<Checkpoint>::namespaced(client.clone(), &ns)
            .get_opt(from)
            .await?
        {
            labels.clone_from(checkpoint.labels());
        }
        json!({"from": {"checkpoint": from}, "clock": args.clock.map(Clock::as_str), "ttl": args.ttl})
    } else {
        json!({"image": args.image, "vcpus": args.vcpus, "memory": args.memory, "disk": args.disk, "ttl": args.ttl})
    };
    labels.extend(args.labels);
    let sandbox = create_sandbox(client, &ns, &args.name, &labels, spec).await?;
    if args.wait.no_wait {
        return finish(
            args.output.format,
            &sandbox,
            &format!("created sandbox {}", args.name),
        );
    }
    let sandboxes = Api::<Sandbox>::namespaced(client.clone(), &ns);
    let sandbox = wait_running(&sandboxes, &args.name, started + args.wait.timeout).await?;
    finish(
        args.output.format,
        &sandbox,
        &format!(
            "sandbox {} is Running on node {} ({:.1} s)",
            args.name,
            node(&sandbox),
            started.elapsed().as_secs_f64()
        ),
    )
}

/// `savestate checkpoint`.
async fn take_checkpoint(client: &Client, args: CheckpointArgs) -> anyhow::Result<()> {
    let ns = namespace(client, &args.common);
    let started = Instant::now();
    let deadline = started + args.wait.timeout;
    let mut checkpoint = checkpoint(
        client,
        &ns,
        &args.sandbox,
        args.name.as_deref(),
        args.ttl.as_deref(),
        args.export.as_deref(),
        args.labels,
        deadline,
        !args.wait.no_wait,
    )
    .await?;
    let name = checkpoint.name_any();
    if args.wait.no_wait {
        return finish(
            args.output.format,
            &checkpoint,
            &format!("created checkpoint {name}"),
        );
    }
    text(
        args.output.format,
        &format!(
            "checkpoint {name} of {} is Ready ({:.1} s)",
            args.sandbox,
            started.elapsed().as_secs_f64()
        ),
    )?;
    if let Some(store) = &args.export {
        let started = Instant::now();
        checkpoint = wait_exported(client, &ns, &name, store, deadline).await?;
        text(
            args.output.format,
            &format!(
                "checkpoint {name} is in store {store} ({:.1} s)",
                started.elapsed().as_secs_f64()
            ),
        )?;
    }
    json_out(args.output.format, &bare(checkpoint))
}

/// One checkpoint of `args.sandbox`, then `args.count` sandboxes from it.
async fn fork(client: &Client, args: Fork) -> anyhow::Result<()> {
    let ns = namespace(client, &args.common);
    let format = args.output.format;
    let started = Instant::now();
    let deadline = started + args.wait.timeout;
    let wait = !args.wait.no_wait;
    let checkpoint = checkpoint(
        client,
        &ns,
        &args.sandbox,
        None,
        None,
        None,
        Vec::new(),
        deadline,
        wait,
    )
    .await?;
    let name = checkpoint.name_any();
    if wait {
        text(
            format,
            &format!("checkpoint {name} of {} is Ready", args.sandbox),
        )?;
    }
    let mut labels = checkpoint.labels().clone();
    labels.extend(args.labels);
    let names: Vec<String> = (1..=args.count).map(|i| format!("{name}-{i}")).collect();
    let mut sandboxes = Vec::new();
    for sandbox in &names {
        let spec = json!({"from": {"checkpoint": name}, "clock": args.clock.map(Clock::as_str)});
        sandboxes.push(create_sandbox(client, &ns, sandbox, &labels, spec).await?);
    }
    if wait {
        let api = Api::<Sandbox>::namespaced(client.clone(), &ns);
        for sandbox in &mut sandboxes {
            *sandbox = wait_running(&api, &sandbox.name_any(), deadline).await?;
            text(
                format,
                &format!(
                    "sandbox {} is Running on node {}",
                    sandbox.name_any(),
                    node(sandbox)
                ),
            )?;
        }
        text(
            format,
            &format!(
                "forked {} into {} sandboxes ({:.1} s)",
                args.sandbox,
                names.len(),
                started.elapsed().as_secs_f64()
            ),
        )?;
    } else {
        text(
            format,
            &format!(
                "created checkpoint {name} and sandboxes {}",
                names.join(" ")
            ),
        )?;
    }
    json_out(
        format,
        &json!({"checkpoint": bare(checkpoint), "sandboxes": sandboxes.into_iter().map(bare).collect::<Vec<_>>()}),
    )
}

/// `savestate get`: the sandbox or checkpoint named `args.name`.
async fn get(client: &Client, args: &Get) -> anyhow::Result<()> {
    let ns = namespace(client, &args.common);
    let sandboxes = Api::<Sandbox>::namespaced(client.clone(), &ns);
    let checkpoints = Api::<Checkpoint>::namespaced(client.clone(), &ns);
    let sandbox = if args.kind == Some(Kind::Checkpoint) {
        None
    } else {
        sandboxes.get_opt(&args.name).await?
    };
    let checkpoint = if args.kind == Some(Kind::Sandbox) {
        None
    } else {
        checkpoints.get_opt(&args.name).await?
    };
    match (sandbox, checkpoint) {
        (Some(_), Some(_)) => bail!(
            "a sandbox and a checkpoint are both named {}; say which with --kind",
            args.name
        ),
        (Some(sandbox), None) => match args.output.format {
            Format::Json => print_json(&bare(sandbox)),
            Format::Text => {
                // A sandbox from a checkpoint has the checkpoint's shape.
                let from = match sandbox.spec.from.as_ref() {
                    Some(from) => checkpoints.get_opt(&from.checkpoint).await?,
                    None => None,
                };
                show::sandbox(&sandbox, from.as_ref())
            }
        },
        (None, Some(checkpoint)) => match args.output.format {
            Format::Json => print_json(&bare(checkpoint)),
            Format::Text => show::checkpoint(&checkpoint),
        },
        (None, None) => {
            let what = match args.kind {
                Some(Kind::Sandbox) => "sandbox",
                Some(Kind::Checkpoint) => "checkpoint",
                None => "sandbox or checkpoint",
            };
            bail!("no {what} named {} in namespace {ns}", args.name)
        }
    }
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

/// `line` with text output.
fn text(format: Format, line: &str) -> anyhow::Result<()> {
    match format {
        Format::Text => say(line),
        Format::Json => Ok(()),
    }
}

/// `value` with JSON output.
fn json_out(format: Format, value: &impl Serialize) -> anyhow::Result<()> {
    match format {
        Format::Text => Ok(()),
        Format::Json => print_json(value),
    }
}

/// `line` with text output, `object` with JSON output.
fn finish<K: Resource + Serialize + Clone>(
    format: Format,
    object: &K,
    line: &str,
) -> anyhow::Result<()> {
    text(format, line)?;
    json_out(format, &bare(object.clone()))
}

/// `object` without its managed fields, which only the API server's bookkeeping needs.
fn bare<K: Resource>(mut object: K) -> K {
    object.meta_mut().managed_fields = None;
    object
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn node(sandbox: &Sandbox) -> &str {
    sandbox
        .status
        .as_ref()
        .and_then(|status| status.node.as_deref())
        .unwrap_or_default()
}

async fn create_sandbox(
    client: &Client,
    ns: &str,
    name: &str,
    labels: &BTreeMap<String, String>,
    mut spec: serde_json::Value,
) -> anyhow::Result<Sandbox> {
    if let Some(spec) = spec.as_object_mut() {
        spec.retain(|_, value| !value.is_null());
    }
    let sandbox: Sandbox = serde_json::from_value(json!({
        "apiVersion": "savestate.dev/v1alpha1",
        "kind": "Sandbox",
        "metadata": {"name": name, "labels": labels},
        "spec": spec,
    }))?;
    Api::<Sandbox>::namespaced(client.clone(), ns)
        .create(&PostParams::default(), &sandbox)
        .await
        .with_context(|| format!("creating sandbox {name}"))
}

/// Waits until the sandbox runs; fails with its message if it fails.
async fn wait_running(
    sandboxes: &Api<Sandbox>,
    name: &str,
    deadline: Instant,
) -> anyhow::Result<Sandbox> {
    wait_for(sandboxes, name, deadline, "to run", |sandbox: &Sandbox| {
        let status = sandbox.status.clone().unwrap_or_default();
        match status.phase {
            SandboxPhase::Running => Progress::Done,
            SandboxPhase::Failed => Progress::Failed(status.message),
            phase => Progress::Waiting(format!("{phase:?}"), status.message),
        }
    })
    .await
}

/// Creates a checkpoint of `sandbox` (named, or generated from the sandbox's name)
/// once the sandbox runs, with the sandbox's labels and `labels` on top; with
/// `ready`, waits until the checkpoint is ready.
#[expect(clippy::too_many_arguments, reason = "the checkpoint verb's options")]
async fn checkpoint(
    client: &Client,
    ns: &str,
    sandbox: &str,
    name: Option<&str>,
    ttl: Option<&str>,
    export: Option<&str>,
    labels: Vec<(String, String)>,
    deadline: Instant,
    ready: bool,
) -> anyhow::Result<Checkpoint> {
    // Only a running sandbox can be checkpointed: wait for it, also without `ready`.
    let sandboxes = Api::<Sandbox>::namespaced(client.clone(), ns);
    let running = wait_running(&sandboxes, sandbox, deadline).await?;
    let mut all = running.labels().clone();
    all.extend(labels);
    let mut metadata = match name {
        Some(name) => json!({"name": name}),
        None => json!({"generateName": format!("{sandbox}-")}),
    };
    metadata["labels"] = json!(all);
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
    let checkpoint = checkpoints
        .create(&PostParams::default(), &checkpoint)
        .await
        .context("creating the checkpoint")?;
    if !ready {
        return Ok(checkpoint);
    }
    wait_for(
        &checkpoints,
        &checkpoint.name_any(),
        deadline,
        "to be ready",
        |checkpoint: &Checkpoint| {
            let status = checkpoint.status.clone().unwrap_or_default();
            match status.phase {
                CheckpointPhase::Ready => Progress::Done,
                CheckpointPhase::Failed => Progress::Failed(status.message),
                phase => Progress::Waiting(format!("{phase:?}"), status.message),
            }
        },
    )
    .await
}

/// Waits until checkpoint `name` is in `store`; fails with its export's message if
/// the export fails.
async fn wait_exported(
    client: &Client,
    ns: &str,
    name: &str,
    store: &str,
    deadline: Instant,
) -> anyhow::Result<Checkpoint> {
    let checkpoints = Api::<Checkpoint>::namespaced(client.clone(), ns);
    let transfers = Api::<Transfer>::namespaced(client.clone(), ns);
    loop {
        let checkpoint = checkpoints.get(name).await?;
        let status = checkpoint.status.clone().unwrap_or_default();
        if status.stores.iter().any(|held| held == store) {
            return Ok(checkpoint);
        }
        let exports = ListParams::default().fields(&format!("spec.checkpoint={name}"));
        let export = transfers
            .list(&exports)
            .await?
            .items
            .into_iter()
            .find(|transfer| transfer.spec.to.store.as_deref() == Some(store));
        if let Some(status) = export.and_then(|transfer| transfer.status)
            && status.phase == TransferPhase::Failed
        {
            bail!(
                "exporting checkpoint {name} to store {store} failed: {}",
                status.message.unwrap_or_default()
            );
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for checkpoint {name} to be in store {store}");
        }
        tokio::time::sleep(POLL).await;
    }
}

const POLL: Duration = Duration::from_millis(500);

/// Where an object stands, for [`wait_for`].
enum Progress {
    Done,
    /// Failed, with its status message.
    Failed(Option<String>),
    /// Still on its way: its phase and status message.
    Waiting(String, Option<String>),
}

/// Polls the object `name` until `progress` says it is done, failing when it has
/// failed or the deadline passes; the object once done. `goal` completes "waiting for
/// sandbox web …".
async fn wait_for<K>(
    api: &Api<K>,
    name: &str,
    deadline: Instant,
    goal: &str,
    progress: impl Fn(&K) -> Progress,
) -> anyhow::Result<K>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    let kind = K::kind(&()).to_lowercase();
    loop {
        let object = api
            .get(name)
            .await
            .with_context(|| format!("reading {kind} {name}"))?;
        let (phase, message) = match progress(&object) {
            Progress::Done => return Ok(object),
            Progress::Failed(message) => {
                bail!("{kind} {name} failed: {}", message.unwrap_or_default())
            }
            Progress::Waiting(phase, message) => (phase, message),
        };
        if Instant::now() >= deadline {
            let message = message.map(|m| format!(": {m}")).unwrap_or_default();
            bail!("timed out waiting for {kind} {name} {goal}; it is {phase}{message}");
        }
        tokio::time::sleep(POLL).await;
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
