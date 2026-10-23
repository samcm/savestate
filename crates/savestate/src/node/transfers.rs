//! Transfers of this node: exports of checkpoints held here to object stores
//! (`spec.from.node` is this node) and imports from stores to here (`spec.to.node`).
//! At most `transfers.perNode` run at a time. A transfer runs inside its reconcile, so
//! one runs at most once at a time; after a restart of the node agent a `Copying`
//! transfer is simply run again, and what is already at the destination is not
//! copied twice. One whose object is deleted while copying stops at its next
//! progress report and removes its partial files here.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, bail};
use kube::{
    Api, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{Controller, controller::Action, events::EventType, watcher},
};
use orbd_vm::fsutil;
use savestate_api::{Checkpoint, Transfer, TransferPhase};
use savestate_objstore::{Moved, Progress};
use serde_json::json;
use tokio::time::Instant;

use super::{Context, Error, MANAGER, cache, checkpoints};
use crate::store::MEMORY;

/// How often a running transfer writes its progress.
const REPORT: Duration = Duration::from_secs(2);

pub(super) async fn run(ctx: Arc<Context>, namespace: String) {
    let transfers = Api::<Transfer>::namespaced(ctx.client.clone(), &namespace);
    let watch = |field: &str| {
        Controller::new(
            transfers.clone(),
            watcher::Config::default().fields(&format!("{field}={}", ctx.node)),
        )
        .shutdown_on_signal()
        .run(
            reconcile,
            |transfer, error, _| super::on_error("transfer", &transfer.name_any(), error),
            ctx.clone(),
        )
    };
    futures::join!(
        super::drain(watch("spec.from.node")),
        super::drain(watch("spec.to.node")),
    );
}

async fn reconcile(transfer: Arc<Transfer>, ctx: Arc<Context>) -> Result<Action, Error> {
    let phase = transfer
        .status
        .as_ref()
        .map(|status| status.phase)
        .unwrap_or_default();
    if transfer.metadata.deletion_timestamp.is_some()
        || matches!(phase, TransferPhase::Ready | TransferPhase::Failed)
    {
        return Ok(Action::await_change());
    }
    let _slot = ctx
        .transfer_slots
        .acquire()
        .await
        .map_err(|error| anyhow::anyhow!("transfer slots: {error}"))?;
    if !write_status(
        &ctx,
        &transfer,
        json!({"phase": "Copying", "message": null}),
    )
    .await?
    {
        return Ok(Action::await_change());
    }
    let started = Instant::now();
    let progress = Progress::default();
    let mut partial = Vec::new();
    let outcome = tokio::select! {
        result = copy(&ctx, &transfer, &progress, &mut partial) => Some(result),
        () = report(&ctx, &transfer, &progress) => None,
    };
    let name = transfer.name_any();
    let namespace = transfer.namespace().unwrap_or_default();
    let what = if transfer.spec.to.store.is_some() {
        "export"
    } else {
        "import"
    };
    match outcome {
        None => {
            remove(&partial);
            tracing::info!(namespace, transfer = name, "deleted while copying; stopped");
        }
        Some(Ok(moved)) => {
            let seconds = started.elapsed().as_secs_f64();
            let total = moved.copied + moved.present;
            let status =
                json!({"phase": "Ready", "bytesDone": total, "bytesTotal": total, "message": null});
            write_status(&ctx, &transfer, status).await?;
            #[expect(clippy::cast_precision_loss, reason = "bytes as MiB")]
            let note = format!(
                "{what} of checkpoint {} done in {seconds:.1} s: {:.1} MiB of data, {:.1} MiB copied, {:.1} MiB already there",
                transfer.spec.checkpoint,
                total as f64 / 1_048_576.0,
                moved.copied as f64 / 1_048_576.0,
                moved.present as f64 / 1_048_576.0,
            );
            tracing::info!(namespace, transfer = name, note, "transfer ready");
            metrics::counter!("savestate_node_transfers_total", "kind" => what, "result" => "ready")
                .increment(1);
            metrics::histogram!("savestate_node_transfer_seconds", "kind" => what).record(seconds);
            super::publish(&ctx, &*transfer, EventType::Normal, "Copied", note).await;
        }
        Some(Err(error)) => {
            remove(&partial);
            let message = format!("{error:#}");
            tracing::warn!(namespace, transfer = name, message, "transfer failed");
            write_status(
                &ctx,
                &transfer,
                json!({"phase": "Failed", "message": message}),
            )
            .await?;
            metrics::counter!("savestate_node_transfers_total", "kind" => what, "result" => "failed")
                .increment(1);
            super::publish(&ctx, &*transfer, EventType::Warning, "Failed", message).await;
        }
    }
    Ok(Action::await_change())
}

/// Runs the copy; `partial` collects what to remove here if it does not finish.
async fn copy(
    ctx: &Context,
    transfer: &Transfer,
    progress: &Progress,
    partial: &mut Vec<PathBuf>,
) -> anyhow::Result<Moved> {
    let spec = &transfer.spec;
    let namespace = transfer.namespace().unwrap_or_default();
    let checkpoint = Api::<Checkpoint>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(&spec.checkpoint)
        .await?
        .with_context(|| format!("checkpoint {} does not exist", spec.checkpoint))?;
    let uid = checkpoint.uid().unwrap_or_default();
    match (&spec.from.store, &spec.to.store) {
        (None, Some(store)) => export(ctx, &checkpoint, &uid, store, progress).await,
        (Some(store), None) => import(ctx, &checkpoint, &uid, store, progress, partial).await,
        _ => bail!("a transfer goes from a node to a store or from a store to a node"),
    }
}

async fn export(
    ctx: &Context,
    checkpoint: &Checkpoint,
    uid: &str,
    store: &str,
    progress: &Progress,
) -> anyhow::Result<Moved> {
    let dir = ctx.store.checkpoints().published(uid);
    let manifest = crate::store::read_manifest(&dir).with_context(|| {
        format!(
            "checkpoint {} is not on node {}",
            checkpoint.name_any(),
            ctx.node
        )
    })?;
    let raw: serde_json::Value =
        fsutil::read_json(&dir.join(orbd_vm::checkpoint::MANIFEST))?.context("no manifest")?;
    // Its own files first: the memory file, then the chain, newest layer first.
    let mut files = vec![format!("checkpoints/{uid}/{MEMORY}")];
    files.extend(manifest.chain.iter().rev().cloned());
    let remote = crate::stores::open(&ctx.client, &ctx.config, store).await?;
    let moved = remote
        .export(uid, ctx.store.root(), &files, &raw, progress)
        .await
        .with_context(|| format!("exporting to store {store}"))?;
    checkpoints::update(ctx, checkpoint, |status| {
        if status.stores.iter().any(|held| held == store) {
            return false;
        }
        status.stores.push(store.to_owned());
        true
    })
    .await?;
    if let Err(error) = cache::pin(ctx).await {
        tracing::warn!(
            error = format!("{error:#}"),
            "updating the scale-down annotation"
        );
    }
    Ok(moved)
}

async fn import(
    ctx: &Context,
    checkpoint: &Checkpoint,
    uid: &str,
    store: &str,
    progress: &Progress,
    partial: &mut Vec<PathBuf>,
) -> anyhow::Result<Moved> {
    // One import of a checkpoint at a time here; a second finds it published.
    let _busy = Busy::wait(ctx, uid).await;
    let checkpoints = ctx.store.checkpoints();
    let mut moved = Moved::default();
    if !checkpoints.published(uid).is_dir() {
        let remote = crate::stores::open(&ctx.client, &ctx.config, store).await?;
        let manifest = remote.manifest(uid).await?.with_context(|| {
            format!(
                "checkpoint {} is not in store {store}",
                checkpoint.name_any()
            )
        })?;
        let tmp = ctx.store.begin_save(uid)?;
        partial.push(tmp.clone());
        let memory = format!("checkpoints/{uid}/{MEMORY}");
        let (mut wanted, mut present) = (Vec::new(), 0);
        for path in &manifest.files {
            let target = if *path == memory {
                tmp.join(MEMORY)
            } else {
                ctx.store.root().join(path)
            };
            if target.exists() {
                present += crate::store::allocated(&target);
            } else {
                let mut part = target.clone().into_os_string();
                part.push(".part");
                partial.push(part.into());
                wanted.push((path.clone(), target));
            }
        }
        let _writing = Writing::new(ctx, &manifest.files);
        moved = remote
            .download(&wanted, progress)
            .await
            .with_context(|| format!("importing from store {store}"))?;
        // Files already on the node (an image, a parent's layers), by their size here.
        moved.present = present;
        // Last: the manifest completes the copy, which is then published.
        fsutil::write_json_atomic(
            &tmp.join(orbd_vm::checkpoint::MANIFEST),
            &manifest.checkpoint,
        )?;
        ctx.store
            .settle(uid)?
            .context("the imported checkpoint is incomplete")?;
        partial.clear();
    }
    checkpoints::update(ctx, checkpoint, |status| {
        if status.nodes.contains(&ctx.node) {
            return false;
        }
        status.nodes.push(ctx.node.clone());
        true
    })
    .await?;
    Ok(moved)
}

/// Writes the transfer's progress every [`REPORT`]; returns once the transfer is gone.
async fn report(ctx: &Context, transfer: &Transfer, progress: &Progress) {
    loop {
        tokio::time::sleep(REPORT).await;
        let status = json!({"bytesDone": progress.done(), "bytesTotal": progress.total()});
        match write_status(ctx, transfer, status).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => tracing::debug!(%error, "writing transfer progress"),
        }
    }
}

/// Merges `status` into the transfer's; false if the transfer is gone.
async fn write_status(
    ctx: &Context,
    transfer: &Transfer,
    status: serde_json::Value,
) -> Result<bool, Error> {
    let api = Api::<Transfer>::namespaced(
        ctx.client.clone(),
        &transfer.namespace().unwrap_or_default(),
    );
    let params = PatchParams {
        field_manager: Some(MANAGER.into()),
        ..PatchParams::default()
    };
    let patch = json!({
        "metadata": {"uid": transfer.uid()},
        "status": status,
    });
    match api
        .patch_status(&transfer.name_any(), &params, &Patch::Merge(patch))
        .await
    {
        Ok(_) => Ok(true),
        // Gone, or replaced by a transfer of the same name.
        Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Removes partial files and directories; best effort.
fn remove(paths: &[PathBuf]) {
    for path in paths {
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %error, "removing a partial file");
        }
    }
}

/// A checkpoint being imported here, while this lives.
struct Busy<'a> {
    ctx: &'a Context,
    uid: String,
}

impl<'a> Busy<'a> {
    async fn wait(ctx: &'a Context, uid: &str) -> Self {
        loop {
            if lock(&ctx.importing).insert(uid.to_owned()) {
                return Self {
                    ctx,
                    uid: uid.to_owned(),
                };
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        lock(&self.ctx.importing).remove(&self.uid);
    }
}

/// Paths an import is writing, kept from garbage collection while this lives.
struct Writing<'a> {
    ctx: &'a Context,
    paths: Vec<String>,
}

impl<'a> Writing<'a> {
    fn new(ctx: &'a Context, paths: &[String]) -> Self {
        lock(&ctx.writing).extend(paths.iter().cloned());
        Self {
            ctx,
            paths: paths.to_vec(),
        }
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        let mut writing = lock(&self.ctx.writing);
        for path in &self.paths {
            if let Some(i) = writing.iter().position(|other| other == path) {
                writing.swap_remove(i);
            }
        }
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
