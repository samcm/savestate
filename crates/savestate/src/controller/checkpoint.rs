//! Checkpoint TTLs, checkpoints that cannot start, and the finalizer that keeps a
//! checkpoint while a sandbox's disk still builds on it.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use kube::{
    Api, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::{
        Controller,
        controller::Action,
        events::EventType,
        finalizer::{Event, finalizer},
        reflector::ObjectRef,
        watcher,
    },
};
use savestate_api::{Checkpoint, CheckpointPhase, Sandbox, SandboxPhase};
use serde_json::json;

use super::{Context, Error};

/// Present on every checkpoint until no sandbox builds on it. The node agent
/// removes a checkpoint's files once the object is gone.
pub(super) const FINALIZER: &str = "savestate.dev/in-use";

pub(super) async fn run(ctx: Arc<Context>, namespace: String) {
    let client = ctx.client.clone();
    let controller = Controller::new(
        Api::<Checkpoint>::namespaced(client.clone(), &namespace),
        watcher::Config::default(),
    );
    let checkpoints = controller.store();
    // A sandbox going away may release checkpoints waiting to be deleted, and a
    // sandbox that is gone or stops running fails the checkpoints still waiting for it.
    let controller = controller.watches(
        Api::<Sandbox>::namespaced(client, &namespace),
        watcher::Config::default(),
        move |sandbox| {
            let name = sandbox.name_any();
            checkpoints
                .state()
                .into_iter()
                .filter(|checkpoint| {
                    checkpoint.metadata.deletion_timestamp.is_some()
                        || (pending(checkpoint) && checkpoint.spec.sandbox == name)
                })
                .map(|checkpoint| ObjectRef::from_obj(&*checkpoint))
                .collect::<Vec<_>>()
        },
    );
    super::drain(controller.shutdown_on_signal().run(
        |checkpoint, ctx| super::timed("checkpoint", reconcile(checkpoint, ctx)),
        |checkpoint, error, _| super::on_error("checkpoint", &*checkpoint, error),
        ctx,
    ))
    .await;
}

async fn reconcile(checkpoint: Arc<Checkpoint>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = checkpoint.namespace().unwrap_or_default();
    let checkpoints = Api::<Checkpoint>::namespaced(ctx.client.clone(), &namespace);
    finalizer(&checkpoints, FINALIZER, checkpoint, |event| async {
        match event {
            Event::Apply(checkpoint) => expire(&ctx, &checkpoints, &checkpoint).await,
            Event::Cleanup(checkpoint) => release(&ctx, &namespace, &checkpoint).await,
        }
    })
    .await
    .map_err(|error| Error::Finalizer(Box::new(error)))
}

async fn expire(
    ctx: &Context,
    checkpoints: &Api<Checkpoint>,
    checkpoint: &Checkpoint,
) -> Result<Action, Error> {
    let left = super::time_left(
        &checkpoint.metadata,
        checkpoint.spec.ttl.as_ref(),
        &ctx.config.ttl.checkpoint,
    );
    if left == Some(Duration::ZERO) {
        super::expire(ctx, checkpoints, checkpoint, "checkpoint").await?;
        return Ok(Action::await_change());
    }
    if pending(checkpoint) {
        refuse(ctx, checkpoints, checkpoint).await?;
    }
    Ok(super::requeue(left))
}

/// Not yet taken up by a node agent.
fn pending(checkpoint: &Checkpoint) -> bool {
    checkpoint
        .status
        .as_ref()
        .is_none_or(|status| status.phase == CheckpointPhase::Pending)
}

/// Fails a pending checkpoint whose sandbox is missing, being deleted or not
/// `Running`: no node agent will ever take it. The node agent of a running
/// sandbox's node takes the others.
async fn refuse(
    ctx: &Context,
    checkpoints: &Api<Checkpoint>,
    checkpoint: &Checkpoint,
) -> Result<(), Error> {
    let name = &checkpoint.spec.sandbox;
    let namespace = checkpoint.namespace().unwrap_or_default();
    // Fresh from the API: a stale cache could fail a checkpoint of a sandbox that has
    // just started running.
    let sandbox = Api::<Sandbox>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(name)
        .await?;
    let message = match sandbox {
        None => format!("sandbox {name} does not exist"),
        Some(sandbox) if sandbox.metadata.deletion_timestamp.is_some() => {
            format!("sandbox {name} is being deleted")
        }
        Some(sandbox) => match sandbox.status.map(|status| status.phase) {
            Some(SandboxPhase::Running) => return Ok(()),
            phase => format!(
                "sandbox {name} is {:?}, not Running",
                phase.unwrap_or_default()
            ),
        },
    };
    // Only while still Pending: the resource version fails the patch if a node agent
    // has taken the checkpoint meanwhile.
    let patch = json!({
        "apiVersion": "savestate.dev/v1alpha1",
        "kind": "Checkpoint",
        "metadata": {"resourceVersion": checkpoint.metadata.resource_version},
        "status": {"phase": "Failed", "message": message},
    });
    match checkpoints
        .patch_status(
            &checkpoint.name_any(),
            &PatchParams::apply(super::MANAGER).force(),
            &Patch::Apply(patch),
        )
        .await
    {
        Ok(_) => {}
        // Changed meanwhile; the change is reconciled next.
        Err(kube::Error::Api(status)) if status.code == 409 => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    tracing::info!(
        namespace,
        name = checkpoint.name_any(),
        message,
        "checkpoint failed"
    );
    super::publish(ctx, checkpoint, EventType::Warning, "Failed", message).await;
    Ok(())
}

/// Succeeds, letting the deletion finish, once no sandbox starts from this
/// checkpoint or from a checkpoint descended from it.
async fn release(ctx: &Context, namespace: &str, checkpoint: &Checkpoint) -> Result<Action, Error> {
    // Read fresh from the API rather than a cache: a stale view here deletes
    // files a sandbox still needs.
    let all = Api::<Checkpoint>::namespaced(ctx.client.clone(), namespace)
        .list(&ListParams::default())
        .await?;
    let name = checkpoint.name_any();
    let lineage = descendants(&name, &all.items);
    let users: Vec<String> = Api::<Sandbox>::namespaced(ctx.client.clone(), namespace)
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .filter(|sandbox| {
            sandbox
                .spec
                .from
                .as_ref()
                .is_some_and(|from| lineage.contains(from.checkpoint.as_str()))
        })
        .map(ResourceExt::name_any)
        .collect();
    if users.is_empty() {
        return Ok(Action::await_change());
    }
    let users = users.join(", ");
    super::publish(
        ctx,
        checkpoint,
        EventType::Normal,
        "InUse",
        format!("deletion waits until these sandboxes are deleted: {users}"),
    )
    .await;
    Err(Error::InUse(users))
}

/// `root` and every checkpoint whose `status.parent` chain leads to it.
fn descendants<'a>(root: &'a str, checkpoints: &'a [Checkpoint]) -> BTreeSet<&'a str> {
    let mut lineage = BTreeSet::from([root]);
    loop {
        let before = lineage.len();
        for checkpoint in checkpoints {
            let parent = checkpoint.status.as_ref().and_then(|s| s.parent.as_deref());
            if parent.is_some_and(|parent| lineage.contains(parent)) {
                lineage.insert(checkpoint.metadata.name.as_deref().unwrap_or_default());
            }
        }
        if lineage.len() == before {
            return lineage;
        }
    }
}
