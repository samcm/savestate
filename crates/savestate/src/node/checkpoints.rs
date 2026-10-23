//! Checkpoints of the sandboxes on this node: the node agent takes a `Pending`
//! checkpoint of a sandbox running here, has its launcher save it, publishes it and
//! writes its status. A save that a restart interrupted is finished from what is on
//! disk: published, or complete and published now, is `Ready`; anything else `Failed`.
//!
//! Order of a save: `checkpoints/<UID>.tmp/` is created, then the status becomes
//! `Saving` (only if still `Pending`), so every `Saving` checkpoint of this node has a
//! directory here; the launcher writes the memory file and manifest; the directory is
//! renamed into place; the status becomes `Ready`.

use std::{sync::Arc, time::Duration};

use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::apis::meta::v1::Time};
use kube::{
    Api, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{Controller, controller::Action, events::EventType, watcher},
};
use savestate_api::{Checkpoint, CheckpointPhase, CheckpointStatus, Sandbox, SandboxPhase};
use serde_json::json;

use super::{Context, Error, MANAGER, launchers};
use crate::{launcher::proto::CheckpointRequest, store::Manifest};

pub(super) async fn run(ctx: Arc<Context>, namespace: String) {
    let checkpoints = Api::<Checkpoint>::namespaced(ctx.client.clone(), &namespace);
    super::drain(
        Controller::new(checkpoints, watcher::Config::default())
            .shutdown_on_signal()
            .run(
                reconcile,
                |checkpoint, error, _| super::on_error("checkpoint", &checkpoint.name_any(), error),
                ctx,
            ),
    )
    .await;
}

async fn reconcile(checkpoint: Arc<Checkpoint>, ctx: Arc<Context>) -> Result<Action, Error> {
    if checkpoint.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }
    let uid = checkpoint.uid().unwrap_or_default();
    let phase = checkpoint
        .status
        .as_ref()
        .map(|status| status.phase)
        .unwrap_or_default();
    match phase {
        CheckpointPhase::Ready | CheckpointPhase::Failed => Ok(Action::await_change()),
        CheckpointPhase::Saving => {
            let store = ctx.store.checkpoints();
            let ours = store.published(&uid).exists() || store.tmp(&uid).exists();
            if !ours || saving(&ctx).contains(&uid) {
                return Ok(Action::await_change());
            }
            // Interrupted by a restart of this agent.
            match ctx.store.settle(&uid)? {
                Some(manifest) => ready(&ctx, &checkpoint, &manifest, None).await?,
                None => {
                    failed(
                        &ctx,
                        &checkpoint,
                        "the save was interrupted by a restart of the node agent".into(),
                    )
                    .await?;
                }
            }
            Ok(Action::await_change())
        }
        CheckpointPhase::Pending => take(&ctx, &checkpoint, &uid).await,
    }
}

/// Saves a pending checkpoint of a sandbox running on this node.
async fn take(ctx: &Context, checkpoint: &Checkpoint, uid: &str) -> Result<Action, Error> {
    let namespace = checkpoint.namespace().unwrap_or_default();
    let name = &checkpoint.spec.sandbox;
    let Some(sandbox) = Api::<Sandbox>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(name)
        .await?
    else {
        return Ok(Action::await_change());
    };
    let status = sandbox.status.clone().unwrap_or_default();
    if status.phase != SandboxPhase::Running || status.node.as_deref() != Some(&ctx.node) {
        // Not here, or not running: the controller fails it.
        return Ok(Action::await_change());
    }
    let sandbox_uid = sandbox.uid().unwrap_or_default();
    // The VM pod's image, by digest: what a restore must run.
    let pod = Api::<Pod>::namespaced(ctx.client.clone(), &namespace)
        .get(name)
        .await?;
    let launcher_image = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.containers.first())
        .and_then(|container| container.image.clone())
        .unwrap_or_default();
    let dir = launchers::run_dir(ctx, &namespace, name, &sandbox_uid);
    let mut client = launchers::connect(&dir).await?;

    ctx.store.begin_save(uid)?;
    let patch = json!({
        "apiVersion": "savestate.dev/v1alpha1",
        "kind": "Checkpoint",
        "metadata": {"resourceVersion": checkpoint.metadata.resource_version},
        "status": {"phase": "Saving"},
    });
    let checkpoints = Api::<Checkpoint>::namespaced(ctx.client.clone(), &namespace);
    match checkpoints
        .patch_status(
            &checkpoint.name_any(),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(patch),
        )
        .await
    {
        Ok(_) => {}
        // Changed meanwhile (failed by the controller, say); look again.
        Err(kube::Error::Api(status)) if status.code == 409 => {
            ctx.store.checkpoints().discard_tmp(uid)?;
            return Ok(Action::requeue(Duration::from_secs(1)));
        }
        Err(error) => return Err(error.into()),
    }
    saving(ctx).insert(uid.to_owned());
    tracing::info!(
        namespace,
        checkpoint = checkpoint.name_any(),
        sandbox = name,
        "saving"
    );
    let request = CheckpointRequest {
        id: uid.to_owned(),
        namespace: namespace.clone(),
        name: checkpoint.name_any(),
        parent: sandbox
            .spec
            .from
            .as_ref()
            .map(|from| from.checkpoint.clone())
            .unwrap_or_default(),
        launcher_image,
    };
    let result = client.checkpoint(request).await;
    let outcome = match result {
        Ok(reply) => match ctx.store.settle(uid)? {
            Some(manifest) => ready(ctx, checkpoint, &manifest, Some(reply.into_inner())).await,
            None => {
                failed(
                    ctx,
                    checkpoint,
                    "the save left no complete checkpoint".into(),
                )
                .await
            }
        },
        Err(status) => {
            ctx.store.checkpoints().discard_tmp(uid)?;
            failed(ctx, checkpoint, status.message().to_owned()).await
        }
    };
    saving(ctx).remove(uid);
    outcome?;
    Ok(Action::await_change())
}

fn saving(ctx: &Context) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<String>> {
    ctx.saving
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn ready(
    ctx: &Context,
    checkpoint: &Checkpoint,
    manifest: &Manifest,
    reply: Option<crate::launcher::proto::CheckpointReply>,
) -> Result<(), Error> {
    let guest_time = manifest
        .guest_time_unix_ns
        .and_then(|ns| jiff::Timestamp::from_nanosecond(i128::from(ns)).ok())
        .map(Time);
    let status = CheckpointStatus {
        phase: CheckpointPhase::Ready,
        nodes: vec![ctx.node.clone()],
        vcpus: Some(manifest.vcpus),
        memory: Some(super::quantity(manifest.memory_mib << 20)),
        disk: Some(super::quantity(manifest.disk_bytes)),
        memory_bytes: Some(manifest.memory_bytes),
        disk_bytes: Some(manifest.layer_bytes),
        guest_time,
        parent: manifest.parent.clone(),
        fingerprint: Some(manifest.fingerprint.clone()),
        message: None,
    };
    write_status(ctx, checkpoint, &status).await?;
    let note = match &reply {
        Some(reply) => {
            #[expect(clippy::cast_precision_loss, reason = "milliseconds as seconds")]
            let (pause, save) = (reply.pause_ms as f64 / 1e3, reply.save_ms as f64 / 1e3);
            metrics::histogram!("savestate_node_checkpoint_pause_seconds").record(pause);
            format!(
                "saved on {}: the sandbox paused {pause:.2} s (memory file {save:.2} s); memory file {} MiB, disk layer {} MiB",
                ctx.node,
                manifest.memory_bytes >> 20,
                manifest.layer_bytes >> 20,
            )
        }
        None => format!(
            "saved on {} (finished after a node agent restart)",
            ctx.node
        ),
    };
    tracing::info!(
        namespace = checkpoint.namespace(),
        checkpoint = checkpoint.name_any(),
        note,
        "checkpoint ready"
    );
    metrics::counter!("savestate_node_checkpoints_total", "result" => "ready").increment(1);
    super::publish(ctx, checkpoint, EventType::Normal, "Saved", note).await;
    Ok(())
}

async fn failed(ctx: &Context, checkpoint: &Checkpoint, message: String) -> Result<(), Error> {
    tracing::warn!(
        namespace = checkpoint.namespace(),
        checkpoint = checkpoint.name_any(),
        message,
        "checkpoint failed"
    );
    let status = CheckpointStatus {
        phase: CheckpointPhase::Failed,
        message: Some(message.clone()),
        ..CheckpointStatus::default()
    };
    write_status(ctx, checkpoint, &status).await?;
    metrics::counter!("savestate_node_checkpoints_total", "result" => "failed").increment(1);
    super::publish(ctx, checkpoint, EventType::Warning, "Failed", message).await;
    Ok(())
}

async fn write_status(
    ctx: &Context,
    checkpoint: &Checkpoint,
    status: &CheckpointStatus,
) -> Result<(), Error> {
    let patch = json!({
        "apiVersion": "savestate.dev/v1alpha1",
        "kind": "Checkpoint",
        "status": status,
    });
    Api::<Checkpoint>::namespaced(
        ctx.client.clone(),
        &checkpoint.namespace().unwrap_or_default(),
    )
    .patch_status(
        &checkpoint.name_any(),
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(patch),
    )
    .await?;
    Ok(())
}
