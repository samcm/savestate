//! The node agent as a kubelet device plugin (device plugin API v1beta1): it
//! advertises `vm.slotsPerNode` units of [`RESOURCE`], each VM pod requests one, and
//! allocating one hands the container `/dev/kvm`, `/dev/net/tun` and
//! `/dev/vhost-vsock`, so VM pods reach their devices without being privileged.
//!
//! Before it advertises anything the node agent installs the VM pods' seccomp profile
//! ([`SECCOMP_PROFILE`]) under the kubelet's seccomp directory, so no VM pod starts
//! on a node that lacks it.

use std::{path::Path, pin::Pin, sync::Arc, time::Duration};

use anyhow::Context as _;
use futures::{Stream, StreamExt as _};
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Node;
use kube::{
    Api,
    api::{Patch, PatchParams},
};
use orbd_vm::fsutil;
use tonic::{
    Request, Response, Status,
    transport::{Endpoint, Server},
};

use super::Context;

#[expect(
    unused_qualifications,
    clippy::doc_markdown,
    clippy::default_trait_access,
    clippy::too_many_lines,
    reason = "generated code"
)]
mod proto {
    tonic::include_proto!("v1beta1");
}

use proto::{
    AllocateRequest, AllocateResponse, ContainerAllocateResponse, Device, DevicePluginOptions,
    DeviceSpec, Empty, ListAndWatchResponse, PreStartContainerRequest, PreStartContainerResponse,
    PreferredAllocationRequest, PreferredAllocationResponse, RegisterRequest,
    device_plugin_server::{DevicePlugin, DevicePluginServer},
    registration_client::RegistrationClient,
};

/// The extended resource every VM pod requests one unit of.
pub const RESOURCE: &str = "savestate.dev/vm";

/// The VM pods' seccomp profile, relative to the kubelet's seccomp directory (the
/// pod's `localhostProfile`).
pub const SECCOMP_PROFILE: &str = "savestate/vm.json";

/// The container runtime's default seccomp profile with one rule added: `socket()`
/// may create `AF_VSOCK` (40) sockets, the guest agent's channel, which the default
/// refuses. Nothing else differs. Generated from containerd v2.4.2's
/// `contrib/seccomp.DefaultProfile` for amd64 (the VM pod's capabilities add no
/// rules to it), marshalled to JSON, with the `socket` rule for family 40 inserted
/// after the one for 39.
const SECCOMP_JSON: &str = include_str!("vm-seccomp.json");

/// The devices one VM needs.
const DEVICES: &[&str] = &["/dev/kvm", "/dev/net/tun", "/dev/vhost-vsock"];

/// The plugin's socket in the kubelet's `device-plugins/` directory.
const SOCKET: &str = "savestate.sock";
/// The kubelet's registration socket there.
const KUBELET_SOCKET: &str = "kubelet.sock";
const VERSION: &str = "v1beta1";

/// A taint an installation may give new VM nodes: the cluster autoscaler counts a
/// node with it as still starting, not as a node the pending VM pods cannot use, so
/// it adds no further node for them meanwhile. The node agent removes it once its
/// node advertises VM slots.
pub const STARTUP_TAINT: &str = "startup-taint.cluster-autoscaler.kubernetes.io/savestate";

/// Removes [`STARTUP_TAINT`] from this node once the node advertises VM slots, and
/// again whenever it comes back (a platform may copy a pool's taints onto its
/// existing nodes when they are edited).
pub(super) async fn untaint(ctx: Arc<Context>) {
    let nodes = Api::<Node>::all(ctx.client.clone());
    loop {
        let wait = match untaint_once(&nodes, &ctx.node).await {
            Ok(true) => Duration::from_secs(30),
            Ok(false) => Duration::from_secs(2),
            Err(error) => {
                tracing::warn!(%error, "removing the startup taint");
                Duration::from_secs(2)
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// Whether the node is free of [`STARTUP_TAINT`] (removing it now if it has VM slots).
async fn untaint_once(nodes: &Api<Node>, name: &str) -> Result<bool, kube::Error> {
    let node = nodes.get(name).await?;
    let taints = node.spec.and_then(|spec| spec.taints).unwrap_or_default();
    if !taints.iter().any(|taint| taint.key == STARTUP_TAINT) {
        return Ok(true);
    }
    let slots = node
        .status
        .and_then(|status| status.allocatable)
        .and_then(|allocatable| allocatable.get(RESOURCE).cloned())
        .is_some_and(|quantity| quantity.0 != "0");
    if !slots {
        return Ok(false);
    }
    let kept: Vec<_> = taints
        .into_iter()
        .filter(|taint| taint.key != STARTUP_TAINT)
        .collect();
    // The resource version makes it a replace of the taints read above, not of
    // whatever another writer has set since.
    let patch = serde_json::json!({
        "metadata": {"resourceVersion": node.metadata.resource_version},
        "spec": {"taints": kept},
    });
    nodes
        .patch(name, &PatchParams::default(), &Patch::Merge(patch))
        .await?;
    tracing::info!(
        taint = STARTUP_TAINT,
        "VM slots are advertised: removed the startup taint"
    );
    Ok(true)
}

/// Installs the VM pods' seccomp profile under `kubelet_dir`, atomically.
pub(super) fn install_seccomp_profile(kubelet_dir: &Path) -> anyhow::Result<()> {
    let path = kubelet_dir.join("seccomp").join(SECCOMP_PROFILE);
    let dir = path.parent().unwrap_or(kubelet_dir);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fsutil::write_atomic(&path, SECCOMP_JSON.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(path = %path.display(), "installed the VM pods' seccomp profile");
    Ok(())
}

/// Serves the device plugin and registers it with the kubelet, again whenever the
/// kubelet restarts (it removes every socket in `device-plugins/` when it starts).
pub(super) async fn run(ctx: Arc<Context>) {
    let dir = ctx.config.kubelet_dir.join("device-plugins");
    let slots = ctx.config.vm.slots_per_node;
    loop {
        match serve(&dir, slots).await {
            Ok(()) => tracing::info!("the kubelet restarted; registering the device plugin again"),
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "device plugin");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Serves the plugin on a fresh socket and registers it; returns once the socket is
/// removed.
async fn serve(dir: &Path, slots: u32) -> anyhow::Result<()> {
    let socket = dir.join(SOCKET);
    match std::fs::remove_file(&socket) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("removing {}", socket.display()));
        }
        _ => {}
    }
    let listener = tokio::net::UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    let mut server = tokio::spawn(
        Server::builder()
            .add_service(DevicePluginServer::new(Plugin { slots }))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    if let Err(error) = register(&dir.join(KUBELET_SOCKET)).await {
        server.abort();
        return Err(error);
    }
    tracing::info!(
        resource = RESOURCE,
        slots,
        "registered the device plugin with the kubelet"
    );
    let removed = async {
        while socket.exists() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    };
    // Not a graceful shutdown: ListAndWatch streams never end on their own.
    tokio::select! {
        result = &mut server => result?.context("serving the device plugin"),
        () = removed => {
            server.abort();
            Ok(())
        }
    }
}

async fn register(kubelet: &Path) -> anyhow::Result<()> {
    let kubelet = kubelet.to_owned();
    let channel = Endpoint::from_static("http://kubelet")
        .connect_with_connector(tower::service_fn(move |_| {
            let kubelet = kubelet.clone();
            async move {
                Ok::<_, std::io::Error>(TokioIo::new(
                    tokio::net::UnixStream::connect(kubelet).await?,
                ))
            }
        }))
        .await
        .context("connecting to the kubelet")?;
    RegistrationClient::new(channel)
        .register(RegisterRequest {
            version: VERSION.into(),
            endpoint: SOCKET.into(),
            resource_name: RESOURCE.into(),
            options: Some(DevicePluginOptions::default()),
        })
        .await
        .map_err(|status| {
            anyhow::anyhow!(
                "the kubelet refused the device plugin: {}",
                status.message()
            )
        })?;
    Ok(())
}

struct Plugin {
    slots: u32,
}

type DeviceStream = Pin<Box<dyn Stream<Item = Result<ListAndWatchResponse, Status>> + Send>>;

#[tonic::async_trait]
impl DevicePlugin for Plugin {
    async fn get_device_plugin_options(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(DevicePluginOptions::default()))
    }

    type ListAndWatchStream = DeviceStream;

    /// Every slot, healthy, for as long as the kubelet listens; the list never
    /// changes.
    async fn list_and_watch(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<Self::ListAndWatchStream>, Status> {
        let devices = (0..self.slots)
            .map(|slot| Device {
                id: format!("vm-{slot}"),
                health: "Healthy".into(),
                topology: None,
            })
            .collect();
        let stream = futures::stream::once(async { Ok(ListAndWatchResponse { devices }) })
            .chain(futures::stream::pending());
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_preferred_allocation(
        &self,
        _: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        Err(Status::unimplemented("no preferred allocation"))
    }

    /// Each slot is the same three device nodes, shared by every VM on the node.
    async fn allocate(
        &self,
        request: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        let container_responses = request
            .into_inner()
            .container_requests
            .iter()
            .map(|_| ContainerAllocateResponse {
                devices: DEVICES
                    .iter()
                    .map(|path| DeviceSpec {
                        container_path: (*path).into(),
                        host_path: (*path).into(),
                        permissions: "rw".into(),
                    })
                    .collect(),
                ..ContainerAllocateResponse::default()
            })
            .collect();
        Ok(Response::new(AllocateResponse {
            container_responses,
        }))
    }

    async fn pre_start_container(
        &self,
        _: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}
