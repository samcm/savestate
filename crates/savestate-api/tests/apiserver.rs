//! The generated CRDs against a real API server: valid objects are accepted and read
//! back as written, and specs that cannot work are refused with a message saying why.
//!
//! Needs a disposable cluster in `KUBECONFIG` (CI uses kind). It installs the CRDs
//! and works in a namespace of its own.

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use k8s_openapi::{
    api::core::v1::Namespace,
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
    apimachinery::pkg::apis::meta::v1::Time,
};
use kube::{
    Api, Client, Resource,
    api::{DeleteParams, DynamicObject, Patch, PatchParams, PostParams},
    core::ApiResource,
};
use savestate_api::{
    Checkpoint, CheckpointPhase, CheckpointSpec, CheckpointStatus, Clock, Fingerprint, Sandbox,
    SandboxPhase, SandboxSource, SandboxSpec, SandboxStatus, Transfer, TransferSpec,
    TransferTarget,
};
use serde_json::{Value, json};

const IMAGE: &str =
    "ghcr.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[tokio::test]
#[ignore = "needs a disposable Kubernetes cluster in KUBECONFIG (CI uses kind)"]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
async fn crds_accept_what_works_and_refuse_what_cannot() {
    let client = Client::try_default().await.expect("KUBECONFIG");
    install_crds(&client).await;
    let ns = format!("savestate-test-{}", std::process::id());
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let mut namespace = Namespace::default();
    namespace.metadata.name = Some(ns.clone());
    namespaces
        .create(&PostParams::default(), &namespace)
        .await
        .unwrap();

    let sandboxes: Api<Sandbox> = Api::namespaced(client.clone(), &ns);
    let checkpoints: Api<Checkpoint> = Api::namespaced(client.clone(), &ns);
    let transfers: Api<Transfer> = Api::namespaced(client.clone(), &ns);

    // Valid objects are accepted and come back exactly as written.
    let from_image = Sandbox::new(
        "web-1",
        SandboxSpec {
            image: Some(IMAGE.into()),
            from: None,
            vcpus: Some(4),
            memory: Some(quantity("8Gi")),
            disk: Some(quantity("60Gi")),
            clock: None,
            ttl: Some("1d12h".parse().unwrap()),
        },
    );
    let from_checkpoint = Sandbox::new(
        "web-2",
        SandboxSpec {
            image: None,
            from: Some(SandboxSource {
                checkpoint: "web-1-parked".into(),
            }),
            vcpus: None,
            memory: None,
            disk: None,
            clock: Some(Clock::Stepped),
            ttl: None,
        },
    );
    let checkpoint = Checkpoint::new(
        "web-1-parked",
        CheckpointSpec {
            sandbox: "web-1".into(),
            ttl: Some("48h".parse().unwrap()),
        },
    );
    let transfer = Transfer::new(
        "parked-to-node-b",
        TransferSpec {
            checkpoint: "web-1-parked".into(),
            to: TransferTarget {
                node: "node-b".into(),
            },
        },
    );
    for sandbox in [&from_image, &from_checkpoint] {
        let created = create_when_served(&sandboxes, sandbox).await;
        assert_eq!(created.spec, sandbox.spec);
    }
    assert_eq!(
        create_when_served(&checkpoints, &checkpoint).await.spec,
        checkpoint.spec
    );
    assert_eq!(
        create_when_served(&transfers, &transfer).await.spec,
        transfer.spec
    );

    // Status the controller and node agents write survives the schema's pruning.
    let status = CheckpointStatus {
        phase: CheckpointPhase::Ready,
        nodes: vec!["node-a".into(), "node-b".into()],
        vcpus: Some(4),
        memory: Some(quantity("8Gi")),
        disk: Some(quantity("60Gi")),
        memory_bytes: Some(8 << 30),
        disk_bytes: Some(1 << 30),
        guest_time: Some(Time("2026-01-02T03:04:05Z".parse().unwrap())),
        parent: Some("web-0-parked".into()),
        fingerprint: Some(Fingerprint {
            launcher_image: IMAGE.into(),
            qemu: "11.1.2".into(),
            machine_type: "pc-q35-11.1".into(),
            cpu_model: "EPYC-Rome-v4".into(),
            devices: "sha256:0123".into(),
        }),
        message: None,
    };
    let patched = checkpoints
        .patch_status(
            "web-1-parked",
            &PatchParams::default(),
            &Patch::Merge(json!({"status": status})),
        )
        .await
        .unwrap();
    assert_eq!(patched.status, Some(status));
    let status = SandboxStatus {
        phase: SandboxPhase::Failed,
        node: Some("node-a".into()),
        from: None,
        message: Some("QEMU exited".into()),
    };
    let patched = sandboxes
        .patch_status(
            "web-1",
            &PatchParams::default(),
            &Patch::Merge(json!({"status": status})),
        )
        .await
        .unwrap();
    assert_eq!(patched.status, Some(status));

    // Specs that cannot work are refused on create, each with its reason.
    let raw = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        &ns,
        &ApiResource::erase::<Sandbox>(&()),
    );
    let image_spec = json!({"image": IMAGE, "vcpus": 4, "memory": "8Gi", "disk": "60Gi"});
    let refused = [
        (
            json!({"vcpus": 4, "memory": "8Gi", "disk": "60Gi"}),
            "set exactly one of image and from",
        ),
        (
            merged(&image_spec, &json!({"from": {"checkpoint": "c"}})),
            "set exactly one of image and from",
        ),
        (
            json!({"image": IMAGE, "vcpus": 4, "memory": "8Gi"}),
            "must set vcpus, memory and disk",
        ),
        (
            json!({"from": {"checkpoint": "c"}, "memory": "8Gi"}),
            "inherits vcpus, memory and disk",
        ),
        (
            merged(&image_spec, &json!({"clock": "frozen"})),
            "clock applies only to a sandbox started from a checkpoint",
        ),
        (
            merged(
                &image_spec,
                &json!({"image": "ghcr.io/example/base:latest"}),
            ),
            "spec.image",
        ),
        (
            merged(&image_spec, &json!({"memory": "0"})),
            "memory must be a positive quantity",
        ),
        (
            merged(&image_spec, &json!({"disk": -1})),
            "disk must be a positive quantity",
        ),
        (merged(&image_spec, &json!({"ttl": "7 days"})), "spec.ttl"),
        (
            json!({"from": {"checkpoint": "c"}, "clock": "sometimes"}),
            "spec.clock",
        ),
    ];
    for (i, (spec, reason)) in refused.into_iter().enumerate() {
        let mut object =
            DynamicObject::new(&format!("bad-{i}"), &ApiResource::erase::<Sandbox>(&()));
        object.data = json!({ "spec": spec });
        let error = raw.create(&PostParams::default(), &object).await.err();
        assert_refused(error, reason, &spec);
    }

    // Only TTLs change after creation.
    let ttl = patch_spec(&sandboxes, "web-1", json!({"ttl": "30d"}))
        .await
        .unwrap();
    assert_eq!(ttl.spec.ttl, Some("30d".parse().unwrap()));
    let changes = [
        (json!({"vcpus": 8}), "vcpus cannot change"),
        (json!({"memory": "16Gi"}), "memory cannot change"),
        (json!({"disk": "120Gi"}), "disk cannot change"),
        (
            json!({"image": IMAGE.replace("0123", "4567")}),
            "image cannot change",
        ),
        (
            json!({"image": null, "vcpus": null, "memory": null, "disk": null, "from": {"checkpoint": "c"}}),
            "only ttl can change on a sandbox",
        ),
    ];
    for (patch, reason) in changes {
        let error = patch_spec(&sandboxes, "web-1", patch.clone()).await.err();
        assert_refused(error, reason, &patch);
    }
    let changes = [
        (
            json!({"from": {"checkpoint": "other"}}),
            "from cannot change",
        ),
        (json!({"clock": "frozen"}), "clock cannot change"),
        (json!({"clock": null}), "only ttl can change on a sandbox"),
    ];
    for (patch, reason) in changes {
        let error = patch_spec(&sandboxes, "web-2", patch.clone()).await.err();
        assert_refused(error, reason, &patch);
    }
    patch_spec(&checkpoints, "web-1-parked", json!({"ttl": "7d"}))
        .await
        .unwrap();
    let error = patch_spec(&checkpoints, "web-1-parked", json!({"sandbox": "web-2"}))
        .await
        .err();
    assert_refused(error, "sandbox cannot change", &json!("checkpoint sandbox"));
    let error = patch_spec(
        &transfers,
        "parked-to-node-b",
        json!({"to": {"node": "node-c"}}),
    )
    .await
    .err();
    assert_refused(error, "a transfer cannot change", &json!("transfer target"));

    namespaces
        .delete(&ns, &DeleteParams::default())
        .await
        .unwrap();
}

async fn install_crds(client: &Client) {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let apply = PatchParams::apply("savestate-test").force();
    for crd in savestate_api::crds() {
        let name = crd.metadata.name.clone().unwrap();
        crds.patch(&name, &apply, &Patch::Apply(&crd))
            .await
            .unwrap();
        for _ in 0..100 {
            let established = crds.get(&name).await.unwrap().status.is_some_and(|status| {
                status
                    .conditions
                    .unwrap_or_default()
                    .iter()
                    .any(|c| c.type_ == "Established" && c.status == "True")
            });
            if established {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Create `object`, retrying while the API server starts serving a new CRD.
async fn create_when_served<K>(api: &Api<K>, object: &K) -> K
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    for _ in 0..50 {
        match api.create(&PostParams::default(), object).await {
            Err(kube::Error::Api(status)) if status.code == 404 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            result => return result.unwrap(),
        }
    }
    panic!("the API server never served the new CRDs");
}

async fn patch_spec<K>(api: &Api<K>, name: &str, spec: Value) -> kube::Result<K>
where
    K: Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    api.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({ "spec": spec })),
    )
    .await
}

fn assert_refused(error: Option<kube::Error>, reason: &str, what: &Value) {
    match error {
        Some(kube::Error::Api(status)) if status.code == 422 => assert!(
            status.message.contains(reason),
            "{what} was refused, but not because {reason:?}: {}",
            status.message
        ),
        other => panic!("{what} should be refused because {reason:?}, got {other:?}"),
    }
}

fn merged(base: &Value, overrides: &Value) -> Value {
    let mut merged = base.clone();
    for (key, value) in overrides.as_object().unwrap() {
        merged[key] = value.clone();
    }
    merged
}

fn quantity(text: &str) -> k8s_openapi::apimachinery::pkg::api::resource::Quantity {
    k8s_openapi::apimachinery::pkg::api::resource::Quantity(text.into())
}
