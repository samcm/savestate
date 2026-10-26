//! Garbage collection and disk use: every minute, and as soon as a VM pod on this
//! node stops, the node agent removes checkpoints whose objects are gone, layers and
//! images nothing uses any more (a sandbox whose VM has stopped for good, `Failed`
//! or with a stopped pod, uses none), and run directories of sandboxes that are gone,
//! then reports what its state directory holds. Files are listed before the objects
//! that keep them, so a file created meanwhile is never mistaken for garbage.

use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use k8s_openapi::api::core::v1::Pod;
use kube::{Api, ResourceExt, api::ListParams};
use savestate_api::{Checkpoint, Sandbox, SandboxPhase};

use super::{Context, launchers};
use crate::SANDBOX_UID;
use crate::store::Live;

const INTERVAL: Duration = Duration::from_secs(60);

pub(super) async fn run(ctx: Arc<Context>) {
    loop {
        if let Err(error) = collect(&ctx).await {
            tracing::warn!(error = format!("{error:#}"), "garbage collection failed");
        }
        if let Err(error) = super::cache::evict(&ctx).await {
            tracing::warn!(error = format!("{error:#}"), "cache eviction failed");
        }
        if let Err(error) = super::cache::pin(&ctx).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "updating the scale-down annotation failed"
            );
        }
        if let Err(error) = super::cache::guard(&ctx).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "updating the disk taint failed"
            );
        }
        report(&ctx);
        tokio::select! {
            () = tokio::time::sleep(INTERVAL) => {}
            () = ctx.collect_now.notified() => {}
        }
    }
}

async fn collect(ctx: &Context) -> anyhow::Result<()> {
    let inventory = ctx.store.inventory();
    // Only the namespaces savestate acts in.
    let run_dirs: Vec<_> = launchers::run_dirs(&ctx.run_root)
        .into_iter()
        .filter(|dir| {
            dir.strip_prefix(&ctx.run_root)
                .ok()
                .and_then(|rest| rest.components().next())
                .is_some_and(|ns| {
                    ctx.config
                        .namespaces
                        .iter()
                        .any(|n| ns.as_os_str() == n.as_str())
                })
        })
        .collect();

    let mut live = Live::default();
    // Prefetched images stay.
    live.images.extend(
        ctx.config
            .images
            .prefetch
            .iter()
            .filter_map(|image| Some(image.rsplit_once("@sha256:")?.1.to_owned())),
    );
    // After the listing: what imports are writing now.
    live.files.extend(
        ctx.writing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    for namespace in &ctx.config.namespaces {
        // Sandboxes whose VM pod here has stopped: their VM never runs again, and
        // the controller may not have marked them `Failed` yet.
        let stopped: BTreeSet<String> = Api::<Pod>::namespaced(ctx.client.clone(), namespace)
            .list(
                &ListParams::default()
                    .labels(launchers::LABELS)
                    .fields(&format!("spec.nodeName={}", ctx.node)),
            )
            .await?
            .into_iter()
            .filter(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|status| status.phase.as_deref())
                    .is_some_and(|phase| matches!(phase, "Failed" | "Succeeded"))
            })
            .filter_map(|pod| pod.labels().get(SANDBOX_UID).cloned())
            .collect();
        let sandboxes = Api::<Sandbox>::namespaced(ctx.client.clone(), namespace)
            .list(&ListParams::default())
            .await?;
        for sandbox in sandboxes {
            if let Some(hex) = sandbox
                .spec
                .image
                .as_deref()
                .and_then(|image| image.rsplit_once("@sha256:"))
            {
                live.images.insert(hex.1.to_owned());
            }
            let failed = sandbox
                .status
                .as_ref()
                .is_some_and(|status| status.phase == SandboxPhase::Failed);
            // A stopped VM's layer goes: nothing can run, checkpoint or read it
            // again, and on a node short of disk it is what the kubelet's evictions
            // must free, or they go on until the node is empty.
            if let Some(uid) = sandbox.uid()
                && !failed
                && !stopped.contains(&uid)
            {
                live.sandboxes.insert(uid);
            }
        }
        let checkpoints = Api::<Checkpoint>::namespaced(ctx.client.clone(), namespace)
            .list(&ListParams::default())
            .await?;
        live.checkpoints
            .extend(checkpoints.iter().filter_map(ResourceExt::uid));
    }

    let store = &ctx.store;
    let removed = tokio::task::block_in_place(|| store.collect(&inventory, &live))?;
    for path in &removed {
        tracing::info!(path = %path.display(), "removed: nothing uses it");
    }
    metrics::counter!("savestate_node_gc_removed_total").increment(removed.len() as u64);

    for dir in run_dirs {
        let uid = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A launcher still listening has a VM (and its vsock context id) to stop.
        if live.sandboxes.contains(&uid)
            || std::os::unix::net::UnixStream::connect(dir.join(crate::launcher::SOCKET)).is_ok()
        {
            continue;
        }
        tracing::info!(dir = %dir.display(), "removing the run directory of a sandbox that is gone");
        std::fs::remove_dir_all(&dir)?;
        remove_empty_parents(&dir, &ctx.run_root);
    }
    Ok(())
}

/// Removes the now empty `<sandbox>` and `<namespace>` directories above `dir`.
fn remove_empty_parents(dir: &Path, root: &Path) {
    let mut parent = dir.parent();
    while let Some(dir) = parent {
        if dir == root || std::fs::remove_dir(dir).is_err() {
            return;
        }
        parent = dir.parent();
    }
}

fn report(ctx: &Context) {
    let usage = ctx.store.usage();
    #[expect(clippy::cast_precision_loss, reason = "byte counts as gauge values")]
    for (kind, bytes) in [
        ("images", usage.images),
        ("layers", usage.layers),
        ("checkpoints", usage.checkpoints),
    ] {
        metrics::gauge!("savestate_node_disk_bytes", "kind" => kind).set(bytes as f64);
    }
    #[expect(clippy::cast_precision_loss, reason = "byte counts as gauge values")]
    metrics::gauge!("savestate_node_disk_free_bytes").set(usage.free as f64);
    tracing::debug!(?usage, "disk use");
}
