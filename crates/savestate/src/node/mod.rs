//! The node agent (`DaemonSet`): owns the state directory on its node (base images,
//! disk layers, checkpoints; see [`crate::store`]), starts the VM in each VM pod on
//! its node, checkpoints sandboxes on its node and writes Checkpoint status, exports
//! checkpoints to object stores and imports them ([`transfers`]), keeps its node from
//! scale-down while it holds the only copy of a checkpoint and drops exported copies
//! under disk pressure ([`cache`]), garbage-collects files and run directories whose
//! objects are gone, and reports disk use. As a kubelet device plugin it hands VM pods their devices and installs
//! their seccomp profile ([`devices`]). It is the only component on a node that holds
//! API credentials. See "Contract with the node agent and launcher" in
//! `docs/design.md`.

mod cache;
mod checkpoints;
mod devices;
mod gc;
mod images;
mod launchers;
mod transfers;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context as _;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use kube::{
    Client, Resource,
    runtime::events::{Event, EventType, Recorder, Reporter},
};
use savestate_api::Config;

use crate::store::Store;

pub use devices::{RESOURCE, SECCOMP_PROFILE};

/// Field manager and event reporter for everything the node agent writes.
const MANAGER: &str = "savestate-node";

/// The host directory holding one run directory per sandbox,
/// `<namespace>/<sandbox>/<sandbox UID>/`.
pub const RUN_ROOT: &str = "/run/savestate";

struct Context {
    client: Client,
    config: Config,
    node: String,
    store: Store,
    run_root: PathBuf,
    images: images::Images,
    cids: launchers::Cids,
    /// Checkpoints (UIDs) this process is saving now.
    saving: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Checkpoints (UIDs) this process is importing now.
    importing: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Paths imports are writing, kept from garbage collection (once per import).
    writing: std::sync::Mutex<Vec<String>>,
    /// Held while a restore looks for its checkpoint and while copies are evicted.
    restores: tokio::sync::Mutex<()>,
    /// `transfers.perNode`.
    transfer_slots: tokio::sync::Semaphore,
    recorder: Recorder,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

/// Run the node agent until SIGTERM or SIGINT.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let node = std::env::var("NODE_NAME").context("NODE_NAME: the node this agent runs on")?;
    let client = Client::try_default()
        .await
        .context("connecting to the Kubernetes API")?;
    let store = Store::new(config.state_dir.clone());
    store
        .prepare()
        .with_context(|| format!("preparing {}", config.state_dir.display()))?;
    let images = images::Images::new(&config, &store)?;
    // Before any VM slot is advertised, so no VM pod starts here without it.
    devices::install_seccomp_profile(&config.kubelet_dir)?;
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: MANAGER.into(),
            instance: Some(node.clone()),
        },
    );
    tracing::info!(node, namespaces = ?config.namespaces, state_dir = %config.state_dir.display(), "node agent starting");
    let ctx = Arc::new(Context {
        client,
        node,
        store,
        run_root: PathBuf::from(RUN_ROOT),
        images,
        cids: launchers::Cids::default(),
        saving: std::sync::Mutex::default(),
        importing: std::sync::Mutex::default(),
        writing: std::sync::Mutex::default(),
        restores: tokio::sync::Mutex::default(),
        transfer_slots: tokio::sync::Semaphore::new(
            usize::try_from(config.transfers.per_node).unwrap_or(1),
        ),
        recorder,
        config,
    });
    cache::label(&ctx).await?;
    let mut tasks: Vec<BoxFuture<'static, ()>> = vec![
        gc::run(ctx.clone()).boxed(),
        devices::run(ctx.clone()).boxed(),
    ];
    for namespace in &ctx.config.namespaces {
        tasks.push(launchers::run(ctx.clone(), namespace.clone()).boxed());
        tasks.push(checkpoints::run(ctx.clone(), namespace.clone()).boxed());
        tasks.push(transfers::run(ctx.clone(), namespace.clone()).boxed());
    }
    // The controllers stop on a signal; the collector and the device plugin with them.
    futures::future::select_all(tasks).await;
    tracing::info!("node agent stopped");
    Ok(())
}

/// Checks that this node runs the configured guest CPU model under `enforce`, as every
/// VM does; the error names the features the node lacks. Needs `/dev/kvm`, so the
/// node agent's pod runs it in a privileged init container.
pub fn probe_cpu(config: &Config) -> anyhow::Result<()> {
    use std::io::Write as _;
    const QEMU: &str = "qemu-system-x86_64";
    let node = std::env::var("NODE_NAME").unwrap_or_else(|_| "this node".into());
    let machine =
        orbd_qemu::probe::q35_machine(std::path::Path::new(QEMU)).map_err(anyhow::Error::msg)?;
    let model = &config.vm.cpu_model;
    if model == "host" {
        tracing::info!(node, "guests get this node's CPU (vm.cpuModel host)");
        return Ok(());
    }
    let mut qemu = std::process::Command::new(QEMU)
        .args(["-machine", &format!("{machine},accel=kvm")])
        .args(["-cpu", &format!("{model},enforce")])
        .args(["-nodefaults", "-display", "none", "-S", "-qmp", "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("starting QEMU")?;
    if let Some(mut stdin) = qemu.stdin.take() {
        // QEMU may already have exited; its status says so.
        let _ = stdin.write_all(b"{\"execute\":\"qmp_capabilities\"}\n{\"execute\":\"quit\"}\n");
    }
    let out = qemu.wait_with_output()?;
    if out.status.success() {
        tracing::info!(node, model, "the guest CPU model runs here");
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // "warning: host doesn't support requested feature: CPUID[eax=80000008h].EBX.ibpb [bit 12]"
    let lacking: std::collections::BTreeSet<&str> = stderr
        .lines()
        .filter_map(|line| line.split_once("requested feature: ")?.1.split(' ').next())
        .filter_map(|feature| feature.rsplit('.').next())
        .collect();
    if lacking.is_empty() {
        anyhow::bail!(
            "vm.cpuModel {model} does not run on {node}: {}",
            stderr.trim()
        );
    }
    let lacking: Vec<&str> = lacking.into_iter().collect();
    anyhow::bail!(
        "vm.cpuModel {model} does not run on {node} under enforce: the host lacks {}; choose a model every VM node runs",
        lacking.join(", ")
    )
}

/// Publish a Kubernetes Event on `object`; best effort.
async fn publish<K: Resource<DynamicType = ()>>(
    ctx: &Context,
    object: &K,
    type_: EventType,
    reason: &str,
    note: String,
) {
    let event = Event {
        type_,
        reason: reason.into(),
        note: Some(note),
        action: reason.into(),
        secondary: None,
    };
    if let Err(error) = ctx.recorder.publish(&event, &object.object_ref(&())).await {
        tracing::warn!(%error, reason, "publishing an event failed");
    }
}

/// Logs a failed reconcile; returns when to retry.
fn on_error(kind: &'static str, name: &str, error: &Error) -> kube::runtime::controller::Action {
    tracing::warn!(kind, name, %error, "reconcile failed");
    metrics::counter!("savestate_node_reconcile_errors_total", "kind" => kind).increment(1);
    kube::runtime::controller::Action::requeue(Duration::from_secs(5))
}

async fn drain<S, T, E>(stream: S)
where
    S: futures::Stream<Item = Result<T, E>>,
    E: std::fmt::Display,
{
    stream
        .for_each(|result| async move {
            if let Err(error) = result {
                tracing::debug!(%error, "controller");
            }
        })
        .await;
}

/// `<quantity>` for a size in bytes: whole Gi or Mi where exact.
fn quantity(bytes: u64) -> k8s_openapi::apimachinery::pkg::api::resource::Quantity {
    const MI: u64 = 1 << 20;
    const GI: u64 = 1 << 30;
    let text = if bytes.is_multiple_of(GI) {
        format!("{}Gi", bytes / GI)
    } else if bytes.is_multiple_of(MI) {
        format!("{}Mi", bytes / MI)
    } else {
        bytes.to_string()
    };
    k8s_openapi::apimachinery::pkg::api::resource::Quantity(text)
}

/// Bytes in a quantity, if it is a whole number of bytes.
fn bytes(
    quantity: &k8s_openapi::apimachinery::pkg::api::resource::Quantity,
) -> anyhow::Result<u64> {
    let parsed = kube_quantity::ParsedQuantity::try_from(quantity)
        .map_err(|error| anyhow::anyhow!("{:?}: {error}", quantity.0))?;
    let bytes = parsed
        .to_bytes_i64()
        .with_context(|| format!("{:?} is not a whole number of bytes", quantity.0))?;
    u64::try_from(bytes).with_context(|| format!("{:?} is negative", quantity.0))
}
