//! Sandbox → VM pod, Sandbox status from that pod, and the import Transfer of a
//! sandbox whose pod lands on a node without its checkpoint.

use std::{sync::Arc, time::Duration};

use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::api::resource::Quantity};
use kube::{
    Api, Resource as _, ResourceExt,
    api::{Patch, PatchParams, PostParams},
    runtime::{Controller, controller::Action, events::EventType, reflector::ObjectRef, watcher},
};
use savestate_api::{
    Checkpoint, CheckpointPhase, Config, Sandbox, SandboxPhase, SandboxStatus, Transfer,
    TransferEnd, TransferPhase, TransferSpec, config::Shape,
};
use serde_json::json;

use super::{Context, Error, MANAGER, pod};

pub(super) async fn run(ctx: Arc<Context>, namespace: String) {
    let client = ctx.client.clone();
    let controller = Controller::new(
        Api::<Sandbox>::namespaced(client.clone(), &namespace),
        watcher::Config::default(),
    );
    let sandboxes = controller.store();
    let controller = controller
        .owns(
            Api::<Pod>::namespaced(client.clone(), &namespace),
            watcher::Config::default().labels(pod::LABELS),
        )
        // Import progress.
        .owns(
            Api::<Transfer>::namespaced(client.clone(), &namespace),
            watcher::Config::default(),
        )
        // A checkpoint becoming Ready (or failing) moves the sandboxes waiting for it.
        .watches(
            Api::<Checkpoint>::namespaced(client, &namespace),
            watcher::Config::default(),
            move |checkpoint| {
                let name = checkpoint.name_any();
                sandboxes
                    .state()
                    .into_iter()
                    .filter(|sandbox| checkpoint_of(sandbox) == Some(name.as_str()))
                    .map(|sandbox| ObjectRef::from_obj(&*sandbox))
                    .collect::<Vec<_>>()
            },
        );
    super::drain(controller.shutdown_on_signal().run(
        |sandbox, ctx| super::timed("sandbox", reconcile(sandbox, ctx)),
        |sandbox, error, _| super::on_error("sandbox", &*sandbox, error),
        ctx,
    ))
    .await;
}

fn checkpoint_of(sandbox: &Sandbox) -> Option<&str> {
    sandbox
        .spec
        .from
        .as_ref()
        .map(|from| from.checkpoint.as_str())
}

async fn reconcile(sandbox: Arc<Sandbox>, ctx: Arc<Context>) -> Result<Action, Error> {
    if sandbox.metadata.deletion_timestamp.is_some() {
        // Its pod goes with it (owner reference).
        return Ok(Action::await_change());
    }
    let namespace = sandbox.namespace().unwrap_or_default();
    let name = sandbox.name_any();
    let sandboxes = Api::<Sandbox>::namespaced(ctx.client.clone(), &namespace);

    let left = super::time_left(
        &sandbox.metadata,
        sandbox.spec.ttl.as_ref(),
        &ctx.config.ttl.sandbox,
    );
    if left == Some(Duration::ZERO) {
        super::expire(&ctx, &sandboxes, &*sandbox, "sandbox").await?;
        return Ok(Action::await_change());
    }

    let old = sandbox.status.clone().unwrap_or_default();
    if old.phase == SandboxPhase::Failed {
        // Terminal: the pod (kept for its logs) is never replaced.
        return Ok(super::requeue(left));
    }

    let pods = Api::<Pod>::namespaced(ctx.client.clone(), &namespace);
    let mut retry = None;
    let status = match pods.get_opt(&name).await? {
        Some(pod) if !super::owned_by(&pod, &*sandbox) => {
            retry = Some(Duration::from_secs(5));
            waiting(
                &sandbox,
                format!("waiting for the VM pod of an earlier sandbox named {name} to go away"),
            )
        }
        Some(pod) => {
            let mut status = observe(&sandbox, &pod, &old);
            if status.phase == SandboxPhase::Restoring
                && let Some(node) = &status.node
                && let Some(waiting) = import(&ctx, &sandbox, node).await?
            {
                status.phase = SandboxPhase::WaitingForTransfer;
                status.message = Some(waiting);
            }
            status
        }
        // Placed once, so the VM ran or started there: its state is gone.
        None if old.node.is_some() => {
            failed(&sandbox, old.node.clone(), "the VM pod was deleted".into())
        }
        None => match plan(&sandbox, &ctx).await? {
            Plan::Wait(message) => waiting(&sandbox, message),
            Plan::Refuse(message) => failed(&sandbox, None, message),
            Plan::Run(vm) => {
                let pod = pod::build(&sandbox, &vm, &ctx.config)?;
                let params = PostParams {
                    field_manager: Some(MANAGER.into()),
                    ..PostParams::default()
                };
                match pods.create(&params, &pod).await {
                    Ok(_) => {
                        super::publish(
                            &ctx,
                            &*sandbox,
                            EventType::Normal,
                            "Created",
                            format!("created VM pod {name} running {}", vm.image),
                        )
                        .await;
                        tracing::info!(namespace, name, image = vm.image, "created VM pod");
                    }
                    // Created by an earlier reconcile the cache has not seen yet.
                    Err(kube::Error::Api(status)) if status.code == 409 => {}
                    Err(error) => return Err(error.into()),
                }
                waiting(&sandbox, None)
            }
        },
    };

    if status != old {
        let patch = json!({
            "apiVersion": "savestate.dev/v1alpha1",
            "kind": "Sandbox",
            "status": status,
        });
        sandboxes
            .patch_status(
                &name,
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(patch),
            )
            .await?;
        transition(&ctx, &sandbox, &old, &status).await;
    }
    Ok(retry.map_or_else(|| super::requeue(left), Action::requeue))
}

/// Events, logs and metrics for a status change.
async fn transition(ctx: &Context, sandbox: &Sandbox, old: &SandboxStatus, new: &SandboxStatus) {
    let phase = format!("{:?}", new.phase);
    tracing::info!(
        namespace = sandbox.namespace(),
        name = sandbox.name_any(),
        from = ?old.phase,
        to = phase,
        node = new.node,
        detail = new.message,
        "sandbox status"
    );
    if new.phase != old.phase {
        metrics::counter!("savestate_sandbox_transitions_total", "phase" => phase.clone())
            .increment(1);
    }
    // An event for what a user would want to see: a new phase, or a new reason
    // for being stuck in one.
    let note = match (&new.message, &new.node) {
        (Some(message), _) if new.message != old.message => message.clone(),
        (_, Some(node)) if new.phase != old.phase => format!("{phase} on node {node}"),
        _ if new.phase != old.phase && new.phase != SandboxPhase::Pending => phase.clone(),
        _ => return,
    };
    let type_ = if new.phase == SandboxPhase::Failed {
        EventType::Warning
    } else {
        EventType::Normal
    };
    super::publish(ctx, sandbox, type_, &phase, note).await;
}

enum Plan {
    Run(pod::Vm),
    /// Not yet: the sandbox stays Pending with this message.
    Wait(String),
    /// Never: the sandbox fails with this message.
    Refuse(String),
}

/// What the VM pod for a sandbox without one runs, and where.
async fn plan(sandbox: &Sandbox, ctx: &Context) -> Result<Plan, Error> {
    let spec = &sandbox.spec;
    let Some(from) = &spec.from else {
        // The CRD guarantees image, vcpus, memory and disk without `from`.
        let (Some(image), Some(vcpus), Some(memory), Some(disk)) =
            (&spec.image, spec.vcpus, &spec.memory, &spec.disk)
        else {
            return Ok(Plan::Refuse(
                "set image, vcpus, memory and disk, or from".into(),
            ));
        };
        if let Some(refusal) = refuse_by_config(&ctx.config, image, vcpus, memory, disk)? {
            return Ok(Plan::Refuse(refusal));
        }
        return Ok(Plan::Run(pod::Vm {
            image: ctx.launcher_image.clone(),
            vcpus,
            cpu: cpu_request(&ctx.config, vcpus, memory, Some(disk))?,
            memory: memory.clone(),
            placement: pod::Placement::Any,
        }));
    };

    let name = &from.checkpoint;
    let checkpoints =
        Api::<Checkpoint>::namespaced(ctx.client.clone(), &sandbox.namespace().unwrap_or_default());
    let Some(checkpoint) = checkpoints.get_opt(name).await? else {
        return Ok(Plan::Wait(format!("checkpoint {name} does not exist")));
    };
    if checkpoint.metadata.deletion_timestamp.is_some() {
        return Ok(Plan::Refuse(format!("checkpoint {name} is being deleted")));
    }
    let status = checkpoint.status.unwrap_or_default();
    match status.phase {
        CheckpointPhase::Ready => {}
        CheckpointPhase::Failed => {
            return Ok(Plan::Refuse(format!("checkpoint {name} failed")));
        }
        phase => {
            return Ok(Plan::Wait(format!(
                "checkpoint {name} is {phase:?}, not yet Ready"
            )));
        }
    }
    if status.nodes.is_empty() && status.stores.is_empty() {
        return Ok(Plan::Wait(format!(
            "no node holds checkpoint {name} and no store has it"
        )));
    }
    let (Some(fingerprint), Some(vcpus), Some(memory)) =
        (status.fingerprint, status.vcpus, status.memory)
    else {
        return Ok(Plan::Refuse(format!(
            "checkpoint {name} is Ready but its status lacks fingerprint, vcpus or memory"
        )));
    };
    // In a store, it can start on any node that gives guests the CPU it was taken
    // with, which imports it unless it holds it already.
    let placement = if status.stores.is_empty() {
        pod::Placement::Nodes(status.nodes)
    } else {
        let cpu = fingerprint
            .host_cpu
            .as_deref()
            .unwrap_or(&fingerprint.cpu_model);
        pod::Placement::Cpu {
            label: crate::cpu_label(cpu),
            prefer: status.nodes,
        }
    };
    Ok(Plan::Run(pod::Vm {
        image: fingerprint.launcher_image,
        vcpus,
        cpu: cpu_request(&ctx.config, vcpus, &memory, status.disk.as_ref())?,
        memory,
        placement,
    }))
}

/// While `node`, where the sandbox's pod is, does not hold its checkpoint: the
/// import `Transfer` from a store holding it (named after the sandbox and owned by
/// it; created now if missing), and a message saying how it goes. `None` once the
/// node holds the checkpoint, or when no store has it (the node agent fails the
/// start then).
async fn import(ctx: &Context, sandbox: &Sandbox, node: &str) -> Result<Option<String>, Error> {
    let Some(name) = checkpoint_of(sandbox) else {
        return Ok(None);
    };
    let namespace = sandbox.namespace().unwrap_or_default();
    let Some(checkpoint) = Api::<Checkpoint>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(name)
        .await?
    else {
        return Ok(None);
    };
    let status = checkpoint.status.clone().unwrap_or_default();
    if status.nodes.iter().any(|holder| holder == node) {
        return Ok(None);
    }
    let Some(store) = status.stores.first() else {
        return Ok(None);
    };
    let transfers = Api::<Transfer>::namespaced(ctx.client.clone(), &namespace);
    let sandbox_name = sandbox.name_any();
    let transfer = match transfers.get_opt(&sandbox_name).await? {
        Some(transfer) if !super::owned_by(&transfer, sandbox) => {
            return Ok(Some(format!(
                "waiting for the transfer of an earlier sandbox named {sandbox_name} to go away"
            )));
        }
        Some(transfer) => transfer,
        None => {
            let owner = sandbox
                .controller_owner_ref(&())
                .ok_or_else(|| Error::PodSpec("the sandbox has no UID yet".into()))?;
            let mut transfer = Transfer::new(
                &sandbox_name,
                TransferSpec {
                    checkpoint: name.to_owned(),
                    from: TransferEnd::store(store.as_str()),
                    to: TransferEnd::node(node),
                },
            );
            transfer.metadata.owner_references = Some(vec![owner]);
            let params = PostParams {
                field_manager: Some(MANAGER.into()),
                ..PostParams::default()
            };
            match transfers.create(&params, &transfer).await {
                Ok(created) => {
                    tracing::info!(
                        namespace,
                        sandbox = sandbox_name,
                        checkpoint = name,
                        store,
                        node,
                        "importing"
                    );
                    super::publish(
                        ctx,
                        sandbox,
                        EventType::Normal,
                        "Importing",
                        format!("importing checkpoint {name} from store {store} to node {node}"),
                    )
                    .await;
                    created
                }
                // Created by an earlier reconcile the cache has not seen yet.
                Err(kube::Error::Api(status)) if status.code == 409 => {
                    return Ok(Some(format!("importing checkpoint {name} to node {node}")));
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let progress = transfer.status.unwrap_or_default();
    let mib = |bytes: Option<u64>| bytes.unwrap_or(0) >> 20;
    Ok(Some(match progress.phase {
        TransferPhase::Pending => {
            format!(
                "importing checkpoint {name} from store {store} to node {node}: waiting for a transfer slot"
            )
        }
        TransferPhase::Copying => format!(
            "importing checkpoint {name} from store {store} to node {node}: {} of {} MiB",
            mib(progress.bytes_done),
            mib(progress.bytes_total)
        ),
        TransferPhase::Ready => format!("imported checkpoint {name} to node {node}"),
        TransferPhase::Failed => format!(
            "importing checkpoint {name} to node {node} failed: {}",
            progress.message.unwrap_or_default()
        ),
    }))
}

/// Why the installation config does not allow this image or shape, if it does not.
fn refuse_by_config(
    config: &Config,
    image: &str,
    vcpus: u32,
    memory: &Quantity,
    disk: &Quantity,
) -> Result<Option<String>, Error> {
    let registries = &config.images.registries;
    let allowed = |prefix: &String| {
        image
            .strip_prefix(prefix.trim_end_matches('/'))
            .is_some_and(|rest| rest.starts_with(['/', '@']))
    };
    if !registries.is_empty() && !registries.iter().any(allowed) {
        return Ok(Some(format!(
            "image {image} is not from an allowed registry ({})",
            registries.join(", ")
        )));
    }
    let shapes = &config.vm.shapes;
    if shapes.is_empty() || offered(config, vcpus, memory, disk)?.is_some() {
        return Ok(None);
    }
    let offered: Vec<String> = shapes
        .iter()
        .map(|s| format!("{} vCPUs/{}/{}", s.vcpus, s.memory.0, s.disk.0))
        .collect();
    Ok(Some(format!(
        "shape {vcpus} vCPUs/{}/{} is not offered; use one of: {}",
        memory.0,
        disk.0,
        offered.join(", ")
    )))
}

/// The configured shape a sandbox of this size has, if any.
fn offered<'a>(
    config: &'a Config,
    vcpus: u32,
    memory: &Quantity,
    disk: &Quantity,
) -> Result<Option<&'a Shape>, Error> {
    let (memory, disk) = (pod::parse(memory, "memory")?, pod::parse(disk, "disk")?);
    for shape in &config.vm.shapes {
        if shape.vcpus == vcpus
            && pod::parse(&shape.memory, "vm.shapes memory")? == memory
            && pod::parse(&shape.disk, "vm.shapes disk")? == disk
        {
            return Ok(Some(shape));
        }
    }
    Ok(None)
}

/// The CPU a VM pod requests: its shape's `cpuRequest`, else
/// `vm.cpuRequestPerVcpu` for each vCPU.
fn cpu_request(
    config: &Config,
    vcpus: u32,
    memory: &Quantity,
    disk: Option<&Quantity>,
) -> Result<Quantity, Error> {
    if let Some(disk) = disk
        && let Some(request) =
            offered(config, vcpus, memory, disk)?.and_then(|s| s.cpu_request.as_ref())
    {
        return Ok(request.clone());
    }
    let per_vcpu = pod::parse(&config.vm.cpu_request_per_vcpu, "vm.cpuRequestPerVcpu")?;
    Ok((per_vcpu * vcpus).into())
}

/// Sandbox status from its VM pod.
fn observe(sandbox: &Sandbox, pod: &Pod, old: &SandboxStatus) -> SandboxStatus {
    let from = sandbox.spec.from.clone();
    let node = pod.spec.as_ref().and_then(|spec| spec.node_name.clone());
    if pod.metadata.deletion_timestamp.is_some() {
        return failed(sandbox, node, "the VM pod is being deleted".into());
    }
    let status = pod.status.clone().unwrap_or_default();
    let container = status
        .container_statuses
        .iter()
        .flatten()
        .find(|c| c.name == "launcher");
    if matches!(status.phase.as_deref(), Some("Failed" | "Succeeded")) {
        let exit = container
            .and_then(|c| c.state.as_ref()?.terminated.as_ref())
            .map(|t| {
                format!(
                    "the launcher exited with code {} ({})",
                    t.exit_code,
                    join(&[t.reason.as_deref(), t.message.as_deref()])
                )
            });
        let reason = join(&[status.reason.as_deref(), status.message.as_deref()]);
        let message = exit.unwrap_or(format!("the VM pod stopped ({reason})"));
        return failed(sandbox, node, message);
    }

    let condition = |type_: &str| {
        status
            .conditions
            .iter()
            .flatten()
            .find(|c| c.type_ == type_)
    };
    let Some(node) = node else {
        let message = condition("PodScheduled")
            .filter(|c| c.status == "False")
            .and_then(|c| c.message.clone())
            .map(|reason| match checkpoint_of(sandbox) {
                Some(checkpoint) => {
                    format!("no node that can restore checkpoint {checkpoint} can run the VM pod: {reason}")
                }
                None => format!("no node can run the VM pod: {reason}"),
            });
        return waiting(sandbox, message);
    };

    let ready = condition("Ready").is_some_and(|c| c.status == "True");
    let phase = if ready || old.phase == SandboxPhase::Running {
        SandboxPhase::Running
    } else if from.is_some() {
        SandboxPhase::Restoring
    } else {
        SandboxPhase::Booting
    };
    // Why the launcher is not running yet, e.g. an image that cannot be pulled.
    let message = container
        .and_then(|c| c.state.as_ref()?.waiting.as_ref())
        .filter(|w| w.reason.as_deref() != Some("ContainerCreating"))
        .map(|w| join(&[w.reason.as_deref(), w.message.as_deref()]));
    SandboxStatus {
        phase,
        node: Some(node),
        from,
        message,
    }
}

fn waiting(sandbox: &Sandbox, message: impl Into<Option<String>>) -> SandboxStatus {
    SandboxStatus {
        phase: SandboxPhase::Pending,
        node: None,
        from: sandbox.spec.from.clone(),
        message: message.into(),
    }
}

fn failed(sandbox: &Sandbox, node: Option<String>, message: String) -> SandboxStatus {
    SandboxStatus {
        phase: SandboxPhase::Failed,
        node,
        from: sandbox.spec.from.clone(),
        message: Some(message),
    }
}

/// Reason and message as one line.
fn join(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(": ")
}
