//! Checkpoint TTLs, and the finalizer that keeps a checkpoint while a sandbox's
//! disk still builds on it.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use kube::{
    Api, ResourceExt,
    api::ListParams,
    runtime::{
        Controller,
        controller::Action,
        events::EventType,
        finalizer::{Event, finalizer},
        reflector::ObjectRef,
        watcher,
    },
};
use savestate_api::{Checkpoint, Sandbox};

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
    // A sandbox going away may release checkpoints waiting to be deleted.
    let controller = controller.watches(
        Api::<Sandbox>::namespaced(client, &namespace),
        watcher::Config::default(),
        move |_| {
            checkpoints
                .state()
                .into_iter()
                .filter(|checkpoint| checkpoint.metadata.deletion_timestamp.is_some())
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
    Ok(super::requeue(left))
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
