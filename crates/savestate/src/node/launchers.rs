//! Starting the VM in each VM pod on this node: once a pod's launcher listens, the
//! node agent allocates the VM's vsock context id, makes sure its base image or
//! checkpoint is here, and tells the launcher to boot or restore.

use std::{
    collections::BTreeSet,
    os::fd::AsRawFd as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context as _;
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, ResourceExt,
    runtime::{Controller, controller::Action, watcher},
};
use orbd_vm::fsutil;
use savestate_api::{Checkpoint, Clock, Sandbox};
use tonic::transport::{Channel, Endpoint};

use super::{Context, Error};
use crate::launcher::{
    self, CID, SOCKET,
    proto::{
        Boot, FailRequest, Restore, StartRequest, StatusRequest, launcher_client::LauncherClient,
        start_request::Source, status_reply::State,
    },
};

/// Labels on every VM pod.
const LABELS: &str = "app.kubernetes.io/name=savestate,app.kubernetes.io/component=vm";

/// First vsock context id handed out; low ids are often taken by other VMs on a host.
const FIRST_CID: u32 = 100_000;

pub(super) async fn run(ctx: Arc<Context>, namespace: String) {
    let pods = Api::<Pod>::namespaced(ctx.client.clone(), &namespace);
    let config = watcher::Config::default()
        .labels(LABELS)
        .fields(&format!("spec.nodeName={}", ctx.node));
    super::drain(Controller::new(pods, config).shutdown_on_signal().run(
        reconcile,
        |pod, error, _| super::on_error("pod", &pod.name_any(), error),
        ctx,
    ))
    .await;
}

/// A VM pod's run directory on this node, from its owning sandbox.
pub(super) fn run_dir(ctx: &Context, namespace: &str, sandbox: &str, uid: &str) -> PathBuf {
    ctx.run_root.join(namespace).join(sandbox).join(uid)
}

async fn reconcile(pod: Arc<Pod>, ctx: Arc<Context>) -> Result<Action, Error> {
    let terminal = pod
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .is_some_and(|phase| matches!(phase, "Failed" | "Succeeded"));
    if pod.metadata.deletion_timestamp.is_some() || terminal {
        return Ok(Action::await_change());
    }
    let Some(owner) = pod
        .owner_references()
        .iter()
        .find(|owner| owner.kind == "Sandbox")
        .cloned()
    else {
        return Ok(Action::await_change());
    };
    let namespace = pod.namespace().unwrap_or_default();
    let dir = run_dir(&ctx, &namespace, &owner.name, &owner.uid);
    if !dir.join(SOCKET).exists() {
        // The launcher is not listening yet.
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let mut client = connect(&dir).await?;
    let state = client.status(StatusRequest {}).await.map_err(status)?;
    if state.into_inner().state() != State::Waiting {
        return Ok(Action::await_change());
    }
    let sandboxes = Api::<Sandbox>::namespaced(ctx.client.clone(), &namespace);
    let Some(sandbox) = sandboxes.get_opt(&owner.name).await? else {
        return Ok(Action::await_change());
    };
    if sandbox.uid().as_deref() != Some(owner.uid.as_str()) {
        return Ok(Action::await_change());
    }
    let request = match plan(&ctx, &namespace, &sandbox, &dir).await {
        Ok(request) => request,
        Err(error) => {
            let message = format!("{error:#}");
            tracing::warn!(
                namespace,
                sandbox = sandbox.name_any(),
                message,
                "cannot start the VM"
            );
            client.fail(FailRequest { message }).await.map_err(status)?;
            return Ok(Action::await_change());
        }
    };
    tracing::info!(
        namespace,
        sandbox = sandbox.name_any(),
        ?request,
        "starting the VM"
    );
    client.start(request).await.map_err(status)?;
    Ok(Action::await_change())
}

/// What the launcher of `sandbox` starts.
async fn plan(
    ctx: &Context,
    namespace: &str,
    sandbox: &Sandbox,
    dir: &Path,
) -> anyhow::Result<StartRequest> {
    let uid = sandbox.uid().unwrap_or_default();
    let spec = &sandbox.spec;
    let source = if let Some(from) = &spec.from {
        let checkpoint = Api::<Checkpoint>::namespaced(ctx.client.clone(), namespace)
            .get(&from.checkpoint)
            .await
            .with_context(|| format!("checkpoint {}", from.checkpoint))?;
        let id = checkpoint.uid().unwrap_or_default();
        let checkpoint_dir = ctx.store.checkpoints().published(&id);
        anyhow::ensure!(
            checkpoint_dir.is_dir(),
            "checkpoint {} is not on node {}",
            from.checkpoint,
            ctx.node
        );
        Source::Restore(Restore {
            checkpoint_dir: checkpoint_dir.display().to_string(),
            checkpoint: from.checkpoint.clone(),
            stepped: spec.clock == Some(Clock::Stepped),
        })
    } else {
        let (Some(image), Some(vcpus), Some(memory), Some(disk)) =
            (&spec.image, spec.vcpus, &spec.memory, &spec.disk)
        else {
            anyhow::bail!("the sandbox has neither an image with a shape nor a checkpoint");
        };
        let memory = super::bytes(memory)?;
        anyhow::ensure!(
            memory.is_multiple_of(1 << 20),
            "memory must be a whole number of MiB"
        );
        let hex = ctx.images.ensure(image).await?;
        Source::Boot(Boot {
            image: crate::store::image_path(&hex),
            vcpus,
            memory_mib: memory >> 20,
            disk_bytes: super::bytes(disk)?,
            cpu_model: ctx.config.vm.cpu_model.clone(),
        })
    };
    Ok(StartRequest {
        sandbox: sandbox.name_any(),
        state_dir: ctx.store.root().display().to_string(),
        layer_prefix: uid,
        vsock_cid: ctx.cids.allocate(&ctx.run_root, dir)?,
        source: Some(source),
    })
}

/// Connects to the launcher listening in `run_dir`. The socket is reached through
/// the directory's file descriptor, as its full path may exceed what a unix socket
/// address holds.
pub(super) async fn connect(run_dir: &Path) -> Result<LauncherClient<Channel>, Error> {
    let dir = Arc::new(std::fs::File::open(run_dir)?);
    let channel = Endpoint::from_static("http://launcher")
        .connect_with_connector(tower::service_fn(move |_| {
            let path = format!("/proc/self/fd/{}/{SOCKET}", dir.as_raw_fd());
            async move {
                Ok::<_, std::io::Error>(TokioIo::new(tokio::net::UnixStream::connect(path).await?))
            }
        }))
        .await
        .map_err(|error| anyhow::anyhow!("connecting to the launcher: {error}"))?;
    Ok(LauncherClient::new(channel))
}

#[expect(clippy::needless_pass_by_value, reason = "used as map_err(status)")]
pub(super) fn status(status: tonic::Status) -> Error {
    Error::Other(anyhow::anyhow!("launcher: {}", status.message()))
}

/// vsock context ids, which are global on a host: each running VM's is in its run
/// directory ([`CID`]), which lives as long as its sandbox.
#[derive(Default)]
pub(super) struct Cids {
    lock: Mutex<()>,
}

impl Cids {
    /// The context id of the VM in `run_dir`, allocated now if it has none: the
    /// lowest free one from [`FIRST_CID`].
    fn allocate(&self, run_root: &Path, run_dir: &Path) -> anyhow::Result<u32> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(cid) = launcher::read_cid(run_dir) {
            return Ok(cid);
        }
        let used: BTreeSet<u32> = run_dirs(run_root)
            .iter()
            .filter_map(|dir| launcher::read_cid(dir).ok())
            .collect();
        let cid = (FIRST_CID..u32::MAX)
            .find(|cid| !used.contains(cid))
            .context("no free vsock context id")?;
        fsutil::write_atomic(&run_dir.join(CID), cid.to_string().as_bytes())?;
        Ok(cid)
    }
}

/// Every run directory, `<run root>/<namespace>/<sandbox>/<uid>`.
pub(super) fn run_dirs(run_root: &Path) -> Vec<PathBuf> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .map(|entry| entry.path())
            .collect()
    };
    children(run_root)
        .iter()
        .flat_map(|namespace| children(namespace))
        .flat_map(|sandbox| children(&sandbox))
        .collect()
}
