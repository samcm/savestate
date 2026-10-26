//! The controller against a real cluster, with a dummy launcher image standing in
//! for QEMU (`tests/launcher/Dockerfile`): it marks the VM ready, then runs until
//! told to crash. The test plays the node agent by writing Checkpoint status, and
//! the export of a checkpoint by adding a store to it.
//!
//! Needs a disposable cluster in `KUBECONFIG` (CI uses kind) and the dummy image,
//! pinned by digest and present on the nodes, in `SAVESTATE_TEST_LAUNCHER_IMAGE`.
//! With no node agent running, the first node must advertise `savestate.dev/vm` and
//! hold the VM pods' seccomp profile (CI patches the node's capacity and copies the
//! profile into the kubelet's seccomp directory).

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{future::Future, time::Duration};

use k8s_openapi::{
    api::{
        core::v1::{Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod, Service},
        networking::v1::NetworkPolicy,
        storage::v1::StorageClass,
    },
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
};
use kube::{
    Api, Client, ResourceExt,
    api::{AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams},
};
use savestate_api::{Checkpoint, CheckpointPhase, Config, Sandbox, SandboxPhase};
use serde_json::json;

const IMAGE: &str =
    "ghcr.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[tokio::test]
#[ignore = "needs a disposable Kubernetes cluster in KUBECONFIG and SAVESTATE_TEST_LAUNCHER_IMAGE"]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
async fn controller_runs_pins_fails_expires_and_protects() {
    let launcher = std::env::var("SAVESTATE_TEST_LAUNCHER_IMAGE")
        .expect("SAVESTATE_TEST_LAUNCHER_IMAGE: the dummy launcher image, by digest");
    savestate::telemetry::init_logging();
    let client = Client::try_default().await.expect("KUBECONFIG");
    install_crds(&client).await;
    let ns = format!("savestate-controller-{}", std::process::id());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": ns}})).unwrap(),
        )
        .await
        .unwrap();
    let node = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await
        .unwrap()
        .items[0]
        .name_any();
    let storage_class = format!("savestate-test-{}", std::process::id());
    create(
        &Api::<StorageClass>::all(client.clone()),
        json!({
            "metadata": {"name": storage_class},
            "provisioner": "kubernetes.io/no-provisioner",
            "volumeBindingMode": "Immediate",
        }),
    )
    .await;
    let persistent_volumes = Api::<PersistentVolume>::all(client.clone());
    for index in 0..4 {
        create(
            &persistent_volumes,
            json!({
                "metadata": {"name": format!("{storage_class}-{index}")},
                "spec": {
                    "capacity": {"storage": "2Gi"},
                    "accessModes": ["ReadWriteOnce"],
                    "persistentVolumeReclaimPolicy": "Retain",
                    "storageClassName": storage_class,
                    "volumeMode": "Filesystem",
                    "hostPath": {"path": format!("/tmp/{storage_class}-{index}"), "type": "DirectoryOrCreate"},
                },
            }),
        )
        .await;
    }

    let config = Config::from_yaml(&format!(
        "namespaces: [{ns}]\nlauncherImage: {launcher}\nvm:\n  cpuModel: EPYC-Rome-v4\n  memoryOverhead: 32Mi\n  activeDiskStorageClass: {storage_class}\n  shapes:\n    - {{vcpus: 1, memory: 64Mi, disk: 1Gi, activeDiskSize: 2Gi}}\n"
    ))
    .unwrap();
    tokio::spawn(savestate::controller::run(config));

    let sandboxes: Api<Sandbox> = Api::namespaced(client.clone(), &ns);
    let checkpoints: Api<Checkpoint> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);

    // Playing the node agent: `base` is Ready on this node, and `child` was taken
    // from a sandbox started from `base`, so its disk builds on `base`.
    for (name, parent) in [("base", None), ("child", Some("base"))] {
        create(
            &checkpoints,
            json!({"metadata": {"name": name}, "spec": {"sandbox": "web-0"}}),
        )
        .await;
        checkpoints
            .patch_status(
                name,
                &PatchParams::default(),
                &Patch::Merge(json!({"status": {
                    "phase": "Ready",
                    "nodes": [node],
                    "vcpus": 1,
                    "memory": "64Mi",
                    "disk": "1Gi",
                    "parent": parent,
                    "fingerprint": {
                        "launcherImage": launcher,
                        "qemu": "11.1.2",
                        "machineType": "pc-q35-11.1",
                        "cpuModel": "EPYC-Rome-v4",
                        "devices": "sha256:0",
                    },
                }})),
            )
            .await
            .unwrap();
    }

    // A sandbox from a checkpoint runs in a Guaranteed pod pinned to a node holding
    // the checkpoint, with no service-account token.
    create(
        &sandboxes,
        json!({"metadata": {"name": "web-1"}, "spec": {"from": {"checkpoint": "child"}}}),
    )
    .await;
    let sandbox = wait_for("web-1 to run", || async {
        let sandbox = sandboxes.get("web-1").await.unwrap();
        let running = sandbox
            .status
            .as_ref()
            .is_some_and(|s| s.phase == SandboxPhase::Running);
        running.then_some(sandbox)
    })
    .await;
    let status = sandbox.status.as_ref().unwrap();
    assert_eq!(status.node.as_deref(), Some(node.as_str()));
    assert_eq!(status.from.as_ref().unwrap().checkpoint, "child");
    let pod = pods.get("web-1").await.unwrap();
    let first_pod_uid = pod.uid().unwrap();
    let sandbox_uid = sandbox.uid().unwrap();
    assert_eq!(pod.owner_references()[0].uid, sandbox_uid);
    assert_eq!(
        pod.status.as_ref().unwrap().qos_class.as_deref(),
        Some("Guaranteed")
    );
    let spec = pod.spec.unwrap();
    assert_eq!(spec.automount_service_account_token, Some(false));
    let volumes = serde_json::to_value(&spec.volumes).unwrap();
    assert!(
        !volumes.to_string().contains("serviceAccountToken"),
        "the VM pod mounts a token: {volumes}"
    );
    let container = &spec.containers[0];
    assert_eq!(container.image.as_deref(), Some(launcher.as_str()));
    let resources = serde_json::to_value(container.resources.as_ref().unwrap()).unwrap();
    // PVC mode reserves disk through the claim, not ephemeral storage on the node.
    assert_eq!(
        resources["requests"],
        json!({"cpu": "1", "memory": "96Mi", "savestate.dev/vm": "1"})
    );
    assert_eq!(
        resources["limits"],
        json!({"cpu": "1", "memory": "96Mi", "savestate.dev/vm": "1"})
    );
    let affinity = serde_json::to_value(spec.affinity.unwrap()).unwrap();
    assert_eq!(
        affinity["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
            [0]["matchFields"],
        json!([{"key": "metadata.name", "operator": "In", "values": [node]}])
    );
    // VM pods prefer nodes running VM pods, so nodes empty out for scale-down.
    assert_eq!(
        affinity["podAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"][0]["podAffinityTerm"]
            ["topologyKey"],
        "kubernetes.io/hostname",
        "{affinity}"
    );
    let claim = claims.get("web-1").await.unwrap();
    let claim_uid = claim.uid().unwrap();
    assert_eq!(claim.owner_references().len(), 1);
    let owner = &claim.owner_references()[0];
    assert_eq!(
        (owner.kind.as_str(), owner.name.as_str(), owner.uid.as_str()),
        ("Sandbox", "web-1", sandbox_uid.as_str())
    );
    let claim_spec = claim.spec.as_ref().unwrap();
    assert_eq!(
        claim_spec.storage_class_name.as_deref(),
        Some(storage_class.as_str())
    );
    assert_eq!(
        claim_spec.access_modes.as_deref(),
        Some(&["ReadWriteOnce".to_owned()][..])
    );
    assert_eq!(claim_spec.volume_mode.as_deref(), Some("Filesystem"));
    assert_eq!(
        claim_spec
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()["storage"]
            .0,
        "2Gi"
    );
    assert!(volumes.as_array().unwrap().iter().any(|volume| {
        volume["name"] == "active-disk" && volume["persistentVolumeClaim"]["claimName"] == "web-1"
    }));
    let mounts = serde_json::to_value(&container.volume_mounts).unwrap();
    assert!(mounts.as_array().unwrap().iter().any(|mount| {
        mount["name"] == "active-disk" && mount["mountPath"] == "/var/lib/savestate-active"
    }));

    // Infrastructure loss replaces the pod while the exact same Sandbox-owned
    // claim survives. This is a cold boot; a launcher failure below stays terminal.
    pods.delete("web-1", &DeleteParams::default())
        .await
        .unwrap();
    let replacement = wait_for("web-1's VM pod to be replaced", || async {
        let pod = pods.get_opt("web-1").await.unwrap()?;
        (pod.uid().as_deref() != Some(first_pod_uid.as_str())
            && pod
                .status
                .as_ref()?
                .conditions
                .as_ref()?
                .iter()
                .any(|condition| condition.type_ == "Ready" && condition.status == "True"))
        .then_some(pod)
    })
    .await;
    let pod_uid = replacement.uid().unwrap();
    assert_eq!(
        claims.get("web-1").await.unwrap().uid().as_deref(),
        Some(claim_uid.as_str())
    );
    let volumes = serde_json::to_value(replacement.spec.unwrap().volumes).unwrap();
    assert!(
        volumes
            .as_array()
            .unwrap()
            .iter()
            .any(|volume| { volume["persistentVolumeClaim"]["claimName"] == "web-1" })
    );

    // A fork pinned to holders that cannot take it (here a node that is gone) moves
    // once its checkpoint is in a store: its pod is replaced by one that may run on
    // any node with the checkpoint's CPU, e.g. one the cluster autoscaler adds.
    create(
        &checkpoints,
        json!({"metadata": {"name": "full"}, "spec": {"sandbox": "web-0"}}),
    )
    .await;
    let ready = json!({
        "phase": "Ready",
        "nodes": ["node-gone"],
        "vcpus": 1,
        "memory": "64Mi",
        "disk": "1Gi",
        "fingerprint": {
            "launcherImage": launcher,
            "qemu": "11.1.2",
            "machineType": "pc-q35-11.1",
            "cpuModel": "EPYC-Rome-v4",
            "devices": "sha256:0",
        },
    });
    checkpoints
        .patch_status(
            "full",
            &PatchParams::default(),
            &Patch::Merge(json!({"status": ready})),
        )
        .await
        .unwrap();
    create(
        &sandboxes,
        json!({"metadata": {"name": "spill"}, "spec": {"from": {"checkpoint": "full"}}}),
    )
    .await;
    let pinned =
        wait_for("spill's pod to be unschedulable", || async {
            let pod = pods.get_opt("spill").await.unwrap()?;
            let unschedulable =
                pod.status.as_ref()?.conditions.as_ref()?.iter().any(|c| {
                    c.type_ == "PodScheduled" && c.reason.as_deref() == Some("Unschedulable")
                });
            unschedulable.then(|| pod.uid())
        })
        .await;
    checkpoints
        .patch_status(
            "full",
            &PatchParams::default(),
            &Patch::Merge(json!({"status": {"stores": ["archive"]}})),
        )
        .await
        .unwrap();
    let affinity = wait_for("spill's pod to be replaced", || async {
        let pod = pods.get_opt("spill").await.unwrap()?;
        (pod.uid() != pinned).then(|| serde_json::to_value(pod.spec.unwrap().affinity).unwrap())
    })
    .await;
    let required = &affinity["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
        [0];
    assert_eq!(required.get("matchFields"), None, "{affinity}");
    assert_eq!(
        required["matchExpressions"][0]["key"], "savestate.dev/cpu",
        "{affinity}"
    );
    sandboxes
        .delete("spill", &DeleteParams::default())
        .await
        .unwrap();

    // Exposure follows the spec both ways: the pod's ports annotation, a Service
    // selecting this pod and a NetworkPolicy opening the port appear, and withdrawing
    // the ports closes the port and removes both.
    let services: Api<Service> = Api::namespaced(client.clone(), &ns);
    let policies: Api<NetworkPolicy> = Api::namespaced(client.clone(), &ns);
    let expose = |spec: serde_json::Value| {
        let sandboxes = sandboxes.clone();
        async move {
            sandboxes
                .patch(
                    "web-1",
                    &PatchParams::default(),
                    &Patch::Merge(json!({"spec": spec})),
                )
                .await
                .unwrap();
        }
    };
    expose(json!({"ports": [{"port": 8080}], "service": true})).await;
    let (service, policy) = wait_for("web-1's Service and NetworkPolicy", || async {
        let annotated =
            pods.get("web-1").await.unwrap().annotations()["savestate.dev/ports"] == "8080/tcp\n";
        let service = services.get_opt("web-1").await.unwrap()?;
        let policy = policies.get_opt("web-1").await.unwrap()?;
        annotated.then_some((service, policy))
    })
    .await;
    let labels = pods.get("web-1").await.unwrap().metadata.labels.unwrap();
    let selector = service.spec.unwrap().selector.unwrap();
    assert!(
        selector.iter().all(|(k, v)| labels.get(k) == Some(v)),
        "the Service does not select web-1's pod: {selector:?}"
    );
    let policy = serde_json::to_value(policy.spec.unwrap()).unwrap();
    assert_eq!(
        policy["ingress"],
        json!([{"from": [{"podSelector": {}}], "ports": [{"port": 8080, "protocol": "TCP"}]}])
    );
    expose(json!({"ports": null, "service": null})).await;
    wait_for("web-1's exposure to be withdrawn", || async {
        let closed =
            pods.get("web-1").await.unwrap().annotations()["savestate.dev/ports"].is_empty();
        let service = services.get_opt("web-1").await.unwrap();
        let policy = policies.get_opt("web-1").await.unwrap();
        (closed && service.is_none() && policy.is_none()).then_some(())
    })
    .await;

    // An in-use checkpoint, here the ancestor of web-1's, is not deleted.
    checkpoints
        .delete("base", &DeleteParams::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let base = checkpoints.get("base").await.unwrap();
    assert!(base.metadata.deletion_timestamp.is_some());

    // A VM that crashes fails its sandbox for good: the pod is not replaced.
    let mut crash = pods
        .exec(
            "web-1",
            ["touch", "/run/savestate/crash"],
            &AttachParams::default(),
        )
        .await
        .unwrap();
    let exec = crash.take_status().unwrap().await.unwrap();
    assert_eq!(exec.status.as_deref(), Some("Success"), "{exec:?}");
    let status = wait_for("web-1 to fail", || async {
        let status = sandboxes.get("web-1").await.unwrap().status?;
        (status.phase == SandboxPhase::Failed).then_some(status)
    })
    .await;
    let message = status.message.unwrap();
    assert!(message.contains("exited with code 1"), "{message}");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let pod = pods.get("web-1").await.unwrap();
    assert_eq!(
        pod.uid().as_deref(),
        Some(pod_uid.as_str()),
        "the failed VM pod was replaced"
    );

    // A checkpoint of a sandbox that is not running, or does not exist, fails rather
    // than waiting forever for a node agent.
    for (name, sandbox) in [("of-failed", "web-1"), ("of-missing", "web-9")] {
        create(
            &checkpoints,
            json!({"metadata": {"name": name}, "spec": {"sandbox": sandbox}}),
        )
        .await;
        let status = wait_for("the checkpoint to fail", || async {
            let status = checkpoints.get(name).await.unwrap().status?;
            (status.phase == CheckpointPhase::Failed).then_some(status)
        })
        .await;
        let message = status.message.unwrap();
        assert!(message.contains(sandbox), "{message}");
    }
    assert_eq!(pod.status.unwrap().phase.as_deref(), Some("Failed"));
    let sandbox = sandboxes.get("web-1").await.unwrap();
    assert_eq!(sandbox.status.unwrap().phase, SandboxPhase::Failed);
    assert!(checkpoints.get_opt("base").await.unwrap().is_some());

    // Once the sandbox is gone, so is the checkpoint.
    sandboxes
        .delete("web-1", &DeleteParams::default())
        .await
        .unwrap();
    wait_for("base to be deleted", || async {
        checkpoints
            .get_opt("base")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;

    // Expired TTLs delete sandboxes and checkpoints.
    create(
        &sandboxes,
        json!({"metadata": {"name": "brief"}, "spec": {
            "image": IMAGE, "vcpus": 1, "memory": "64Mi", "disk": "1Gi", "ttl": "3s",
        }}),
    )
    .await;
    checkpoints
        .patch(
            "child",
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"ttl": "3s"}})),
        )
        .await
        .unwrap();
    wait_for("TTLs to expire", || async {
        let sandbox = sandboxes.get_opt("brief").await.unwrap();
        let checkpoint = checkpoints.get_opt("child").await.unwrap();
        (sandbox.is_none() && checkpoint.is_none()).then_some(())
    })
    .await;

    namespaces
        .delete(&ns, &DeleteParams::default())
        .await
        .unwrap();
}

async fn create<K>(api: &Api<K>, object: serde_json::Value)
where
    K: kube::Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let object: K = serde_json::from_value(object).unwrap();
    api.create(&PostParams::default(), &object).await.unwrap();
}

/// Poll `check` until it returns a value; panic after two minutes.
async fn wait_for<T, F: Future<Output = Option<T>>>(what: &str, check: impl Fn() -> F) -> T {
    for _ in 0..240 {
        if let Some(value) = check().await {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn install_crds(client: &Client) {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let apply = PatchParams::apply("savestate-test").force();
    for crd in savestate_api::crds() {
        let name = crd.metadata.name.clone().unwrap();
        crds.patch(&name, &apply, &Patch::Apply(&crd))
            .await
            .unwrap();
        wait_for("the CRDs to be established", || async {
            let crd = crds.get(&name).await.unwrap();
            let conditions = crd.status?.conditions?;
            conditions
                .iter()
                .any(|c| c.type_ == "Established" && c.status == "True")
                .then_some(())
        })
        .await;
    }
}
