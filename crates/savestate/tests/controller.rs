//! The controller against a real cluster, with a dummy launcher image standing in
//! for QEMU (`tests/launcher/Dockerfile`): it marks the VM ready, then runs until
//! told to crash. The test plays the node agent by writing Checkpoint status.
//!
//! Needs a disposable cluster in `KUBECONFIG` (CI uses kind) and the dummy image,
//! pinned by digest and present on the nodes, in `SAVESTATE_TEST_LAUNCHER_IMAGE`.

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{future::Future, time::Duration};

use k8s_openapi::{
    api::core::v1::{Namespace, Node, Pod},
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

    let config = Config::from_yaml(&format!(
        "namespaces: [{ns}]\nlauncherImage: {launcher}\nvm:\n  cpuModel: EPYC-Rome-v4\n  memoryOverhead: 32Mi\n"
    ))
    .unwrap();
    tokio::spawn(savestate::controller::run(config));

    let sandboxes: Api<Sandbox> = Api::namespaced(client.clone(), &ns);
    let checkpoints: Api<Checkpoint> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);

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
    let status = sandbox.status.unwrap();
    assert_eq!(status.node.as_deref(), Some(node.as_str()));
    assert_eq!(status.from.unwrap().checkpoint, "child");
    let pod = pods.get("web-1").await.unwrap();
    let pod_uid = pod.uid();
    assert_eq!(pod.owner_references()[0].uid, sandbox.metadata.uid.unwrap());
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
    assert_eq!(resources["requests"], json!({"cpu": "1", "memory": "96Mi"}));
    assert_eq!(resources["limits"], resources["requests"]);
    let affinity = serde_json::to_value(spec.affinity.unwrap()).unwrap();
    assert_eq!(
        affinity["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
            [0]["matchFields"],
        json!([{"key": "metadata.name", "operator": "In", "values": [node]}])
    );

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
    assert_eq!(pod.uid(), pod_uid, "the failed VM pod was replaced");

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
