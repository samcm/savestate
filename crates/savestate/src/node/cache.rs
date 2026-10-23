//! What this node's local copies mean for the cluster: the node is kept from
//! cluster-autoscaler scale-down while it holds a `Ready` checkpoint no store has,
//! and local copies of checkpoints a store holds are a cache dropped, oldest first,
//! while the state directory's filesystem is short of free space. Its CPU label
//! says which checkpoints from stores can be restored here.

use std::collections::BTreeSet;

use anyhow::Context as _;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    Api, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::events::EventType,
};
use savestate_api::{Checkpoint, CheckpointPhase, Sandbox};
use serde_json::json;

use super::{Context, MANAGER, checkpoints};

/// The cluster autoscaler leaves a node with this annotation (`"true"`) alone.
const SCALE_DOWN_DISABLED: &str = "cluster-autoscaler.kubernetes.io/scale-down-disabled";

/// Labels this node with the CPU its guests get (`crate::CPU_LABEL`).
pub(super) async fn label(ctx: &Context) -> anyhow::Result<()> {
    let model = &ctx.config.vm.cpu_model;
    let cpu = if model == "host" {
        crate::host_cpu()
    } else {
        model.clone()
    };
    let value = crate::cpu_label(&cpu);
    patch_node(
        ctx,
        json!({"metadata": {"labels": {crate::CPU_LABEL: value}}}),
    )
    .await?;
    tracing::info!(
        node = ctx.node,
        cpu,
        label = value,
        "labelled the node with its guests' CPU"
    );
    Ok(())
}

/// Sets or removes the scale-down annotation on this node, if it differs.
pub(super) async fn set_scale_down(ctx: &Context, disabled: bool) -> anyhow::Result<()> {
    let node = Api::<Node>::all(ctx.client.clone()).get(&ctx.node).await?;
    let current = node
        .annotations()
        .get(SCALE_DOWN_DISABLED)
        .map(String::as_str)
        == Some("true");
    if current == disabled {
        return Ok(());
    }
    let value = disabled.then_some("true");
    patch_node(
        ctx,
        json!({"metadata": {"annotations": {SCALE_DOWN_DISABLED: value}}}),
    )
    .await?;
    tracing::info!(node = ctx.node, disabled, "scale-down of this node");
    Ok(())
}

/// Disables scale-down while a local copy is the only one of a `Ready` checkpoint,
/// and enables it otherwise.
pub(super) async fn pin(ctx: &Context) -> anyhow::Result<()> {
    let local: BTreeSet<String> = ctx.store.inventory().checkpoints.into_iter().collect();
    let mut only_here = false;
    for checkpoint in all_checkpoints(ctx).await? {
        let status = checkpoint.status.unwrap_or_default();
        if status.phase == CheckpointPhase::Ready
            && status.stores.is_empty()
            && checkpoint
                .metadata
                .uid
                .is_some_and(|uid| local.contains(&uid))
        {
            only_here = true;
            break;
        }
    }
    set_scale_down(ctx, only_here).await
}

/// While less than `cache.minFreePercent` of the state directory's filesystem is
/// free, drops local copies of checkpoints a store holds, oldest first, except those
/// a VM pod on this node starts or runs from. Each leaves `status.nodes` before its
/// files go. Holds the restore lock, so no restore starts from a copy being dropped.
pub(super) async fn evict(ctx: &Context) -> anyhow::Result<()> {
    let min = f64::from(ctx.config.cache.min_free_percent);
    if min == 0.0 || free_percent(ctx)? >= min {
        return Ok(());
    }
    let _restores = ctx.restores.lock().await;
    let in_use = in_use(ctx).await?;
    let local: BTreeSet<String> = ctx.store.inventory().checkpoints.into_iter().collect();
    let mut candidates: Vec<Checkpoint> = all_checkpoints(ctx)
        .await?
        .into_iter()
        .filter(|checkpoint| {
            let status = checkpoint.status.clone().unwrap_or_default();
            status.phase == CheckpointPhase::Ready
                && !status.stores.is_empty()
                && checkpoint.uid().is_some_and(|uid| local.contains(&uid))
                && !in_use.contains(&(
                    checkpoint.namespace().unwrap_or_default(),
                    checkpoint.name_any(),
                ))
        })
        .collect();
    candidates.sort_by_key(|checkpoint| checkpoint.creation_timestamp().map(|time| time.0));
    for checkpoint in candidates {
        if free_percent(ctx)? >= min {
            break;
        }
        checkpoints::update(ctx, &checkpoint, |status| {
            let before = status.nodes.len();
            status.nodes.retain(|node| *node != ctx.node);
            status.nodes.len() != before
        })
        .await?;
        let uid = checkpoint.uid().unwrap_or_default();
        ctx.store.checkpoints().discard(&uid)?;
        let note = format!(
            "dropped the copy on node {} for disk space; stores hold it ({})",
            ctx.node,
            checkpoint
                .status
                .as_ref()
                .map(|status| status.stores.join(", "))
                .unwrap_or_default()
        );
        tracing::info!(
            namespace = checkpoint.namespace(),
            checkpoint = checkpoint.name_any(),
            note,
            "evicted"
        );
        metrics::counter!("savestate_node_evictions_total").increment(1);
        super::publish(ctx, &checkpoint, EventType::Normal, "Evicted", note).await;
    }
    Ok(())
}

/// `(namespace, checkpoint)` that the sandboxes of VM pods on this node start from.
async fn in_use(ctx: &Context) -> anyhow::Result<BTreeSet<(String, String)>> {
    let mut in_use = BTreeSet::new();
    for namespace in &ctx.config.namespaces {
        let pods = Api::<Pod>::namespaced(ctx.client.clone(), namespace)
            .list(
                &ListParams::default()
                    .labels(super::launchers::LABELS)
                    .fields(&format!("spec.nodeName={}", ctx.node)),
            )
            .await?;
        let owners: BTreeSet<String> = pods
            .iter()
            .flat_map(|pod| pod.owner_references().iter())
            .filter(|owner| owner.kind == "Sandbox")
            .map(|owner| owner.name.clone())
            .collect();
        let sandboxes = Api::<Sandbox>::namespaced(ctx.client.clone(), namespace)
            .list(&ListParams::default())
            .await?;
        for sandbox in sandboxes {
            if owners.contains(&sandbox.name_any())
                && let Some(from) = sandbox.spec.from
            {
                in_use.insert((namespace.clone(), from.checkpoint));
            }
        }
    }
    Ok(in_use)
}

async fn all_checkpoints(ctx: &Context) -> anyhow::Result<Vec<Checkpoint>> {
    let mut all = Vec::new();
    for namespace in &ctx.config.namespaces {
        all.extend(
            Api::<Checkpoint>::namespaced(ctx.client.clone(), namespace)
                .list(&ListParams::default())
                .await?
                .items,
        );
    }
    Ok(all)
}

fn free_percent(ctx: &Context) -> anyhow::Result<f64> {
    let stat = nix::sys::statvfs::statvfs(ctx.store.root())
        .with_context(|| format!("statvfs {}", ctx.store.root().display()))?;
    #[expect(clippy::cast_precision_loss, reason = "block counts as a percentage")]
    let percent = if stat.blocks() == 0 {
        100.0
    } else {
        stat.blocks_available() as f64 * 100.0 / stat.blocks() as f64
    };
    Ok(percent)
}

async fn patch_node(ctx: &Context, patch: serde_json::Value) -> anyhow::Result<()> {
    let params = PatchParams {
        field_manager: Some(MANAGER.into()),
        ..PatchParams::default()
    };
    Api::<Node>::all(ctx.client.clone())
        .patch(&ctx.node, &params, &Patch::Merge(patch))
        .await
        .with_context(|| format!("patching node {}", ctx.node))?;
    Ok(())
}
