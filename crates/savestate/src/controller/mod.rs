//! The controller (Deployment, one replica): creates one Pod per Sandbox, places
//! sandboxes started from checkpoints on nodes that hold them or can import them,
//! writes Sandbox status, creates export and import Transfers, enforces TTLs, keeps
//! the finalizer that protects checkpoints still in use, and garbage-collects the
//! object stores. See "Controller" in `docs/design.md`.
//!
//! It runs one sandbox and one checkpoint controller per namespace in the
//! installation config; its permissions are bound per namespace, so it never
//! watches cluster-wide.

mod checkpoint;
mod pod;
mod sandbox;
mod stores;

use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use jiff::{SignedDuration, Timestamp};
use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::apis::meta::v1::ObjectMeta};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{DeleteParams, Preconditions},
    runtime::{
        controller::Action,
        events::{Event, EventType, Recorder, Reporter},
    },
};
use savestate_api::{Config, Ttl, config::TtlLimit};

/// Field manager and event reporter for everything the controller writes.
const MANAGER: &str = "savestate-controller";

/// Longest wait before an object is looked at again, whatever its TTL.
const MAX_REQUEUE: Duration = Duration::from_secs(3600);

struct Context {
    client: Client,
    config: Config,
    /// Image of VM pods for sandboxes from an image, by digest.
    launcher_image: String,
    recorder: Recorder,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("building the VM pod: {0}")]
    PodSpec(String),
    /// A checkpoint's deletion waits for the sandboxes named here.
    #[error("still in use by sandboxes {0}")]
    InUse(String),
    #[error(transparent)]
    Finalizer(#[from] Box<kube::runtime::finalizer::Error<Error>>),
}

/// Run the controller until SIGTERM or SIGINT.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let client = Client::try_default()
        .await
        .context("connecting to the Kubernetes API")?;
    let launcher_image = match &config.launcher_image {
        Some(image) => image.clone(),
        None => own_image(&client).await.context(
            "launcherImage is not set, and the controller's own image could not be read",
        )?,
    };
    tracing::info!(
        %launcher_image,
        namespaces = ?config.namespaces,
        "controller starting"
    );
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: MANAGER.into(),
            instance: std::env::var("POD_NAME").ok(),
        },
    );
    let ctx = Arc::new(Context {
        client,
        config,
        launcher_image,
        recorder,
    });
    let mut controllers: Vec<BoxFuture<'static, ()>> = Vec::new();
    for namespace in &ctx.config.namespaces {
        controllers.push(sandbox::run(ctx.clone(), namespace.clone()).boxed());
        controllers.push(checkpoint::run(ctx.clone(), namespace.clone()).boxed());
    }
    // The controllers stop on a signal; store garbage collection with them.
    tokio::select! {
        _ = futures::future::join_all(controllers) => {}
        () = stores::run(ctx.clone()) => {}
    }
    tracing::info!("controller stopped");
    Ok(())
}

/// The image this controller runs, by digest: the default launcher image. Read
/// from its own pod (`POD_NAME` and `POD_NAMESPACE`).
async fn own_image(client: &Client) -> anyhow::Result<String> {
    let name = std::env::var("POD_NAME").context("POD_NAME")?;
    let namespace = std::env::var("POD_NAMESPACE").context("POD_NAMESPACE")?;
    let pod = Api::<Pod>::namespaced(client.clone(), &namespace)
        .get(&name)
        .await?;
    let container = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.containers.first())
        .context("own pod has no containers")?;
    if container.image.as_deref().is_some_and(is_pinned) {
        return Ok(container.image.clone().unwrap_or_default());
    }
    // A tag: the digest the node resolved it to, as `repository@sha256:…`.
    pod.status
        .iter()
        .flat_map(|status| status.container_statuses.iter().flatten())
        .find(|status| status.name == container.name)
        .map(|status| status.image_id.clone())
        .filter(|id| is_pinned(id))
        .context("own container's imageID has no repository digest")
}

/// Whether `child` belongs to `owner` (and not to an earlier object of the same
/// name).
fn owned_by(child: &impl ResourceExt, owner: &impl Resource) -> bool {
    let uid = owner.meta().uid.as_deref();
    child
        .owner_references()
        .iter()
        .any(|reference| Some(reference.uid.as_str()) == uid)
}

fn is_pinned(image: &str) -> bool {
    image.contains("@sha256:")
}

/// How long until an object's TTL expires (zero once it has); `None` without a
/// TTL. The object's own TTL, else the configured default, capped at the
/// configured maximum.
fn time_left(meta: &ObjectMeta, ttl: Option<&Ttl>, limit: &TtlLimit) -> Option<Duration> {
    let ttl = ttl.or(limit.default.as_ref()).map(Ttl::duration);
    let max = limit.max.as_ref().map(Ttl::duration);
    let ttl = match (ttl, max) {
        (Some(ttl), Some(max)) => ttl.min(max),
        (ttl, max) => ttl.or(max)?,
    };
    let created = meta.creation_timestamp.as_ref()?.0;
    // Too far in the future to represent: practically forever.
    let expires = created
        .checked_add(SignedDuration::try_from(ttl).ok()?)
        .ok()?;
    Some(
        Timestamp::now()
            .duration_until(expires)
            .try_into()
            .unwrap_or(Duration::ZERO),
    )
}

/// Requeue after `left` (the TTL), but at least hourly.
fn requeue(left: Option<Duration>) -> Action {
    Action::requeue(left.map_or(MAX_REQUEUE, |left| left.min(MAX_REQUEUE)))
}

/// Delete an object whose TTL has expired, with an event saying so. The UID
/// precondition keeps a newer object of the same name safe.
async fn expire<K>(ctx: &Context, api: &Api<K>, object: &K, kind: &'static str) -> Result<(), Error>
where
    K: Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    publish(
        ctx,
        object,
        EventType::Normal,
        "Expired",
        "its TTL expired; deleting it".into(),
    )
    .await;
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: object.meta().uid.clone(),
            resource_version: None,
        }),
        ..DeleteParams::default()
    };
    match api.delete(&object.name_any(), &params).await {
        Ok(_) => {}
        Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => {}
        Err(error) => return Err(error.into()),
    }
    tracing::info!(
        kind,
        namespace = object.namespace(),
        name = object.name_any(),
        "TTL expired; deleted"
    );
    metrics::counter!("savestate_ttl_expired_total", "kind" => kind).increment(1);
    Ok(())
}

/// Publish a Kubernetes Event on `object`. Events are best effort: a failure is
/// logged, never retried.
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

/// Count and time one reconcile.
async fn timed(
    kind: &'static str,
    reconcile: impl Future<Output = Result<Action, Error>>,
) -> Result<Action, Error> {
    let started = std::time::Instant::now();
    let result = reconcile.await;
    metrics::counter!("savestate_controller_reconciles_total", "kind" => kind).increment(1);
    metrics::histogram!("savestate_controller_reconcile_seconds", "kind" => kind)
        .record(started.elapsed().as_secs_f64());
    result
}

/// Logs and metrics for a failed reconcile, shared by both controllers; returns
/// when to retry.
fn on_error(kind: &'static str, object: &impl ResourceExt, error: &Error) -> Action {
    let (namespace, name) = (object.namespace(), object.name_any());
    if let Error::Finalizer(inner) = error
        && let kube::runtime::finalizer::Error::CleanupFailed(Error::InUse(sandboxes)) = &**inner
    {
        // Not a failure: deletion waits, and sandbox changes requeue it sooner.
        tracing::info!(
            kind,
            namespace,
            name,
            sandboxes,
            "deletion waits for sandboxes"
        );
        return Action::requeue(Duration::from_secs(60));
    }
    tracing::warn!(kind, namespace, name, %error, "reconcile failed");
    metrics::counter!("savestate_controller_reconcile_errors_total", "kind" => kind).increment(1);
    Action::requeue(Duration::from_secs(10))
}

/// Drain a controller's output stream; reconcile errors are already handled by
/// [`on_error`].
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
