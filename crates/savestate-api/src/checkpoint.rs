use k8s_openapi::apimachinery::pkg::{api::resource::Quantity, apis::meta::v1::Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Ttl;

/// A named, read-only, complete picture of a sandbox at one instant: RAM, CPU and
/// device state, and disk. Taking one pauses the sandbox for a few seconds; it then
/// carries on. Only `ttl` can change after creation.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "savestate.dev",
    version = "v1alpha1",
    kind = "Checkpoint",
    namespaced,
    doc = "A saved, complete picture of a sandbox that new sandboxes start from",
    status = "CheckpointStatus",
    shortname = "ckpt",
    category = "savestate",
    derive = "PartialEq",
    selectable = ".spec.sandbox",
    printcolumn(json_path = ".spec.sandbox", name = "Sandbox", type_ = "string"),
    printcolumn(json_path = ".status.phase", name = "Phase", type_ = "string"),
    printcolumn(json_path = ".status.nodes", name = "Nodes", type_ = "string"),
    printcolumn(
        json_path = ".status.memoryBytes",
        name = "Memory",
        type_ = "integer",
        description = "Bytes of the RAM file on disk"
    ),
    printcolumn(
        json_path = ".status.diskBytes",
        name = "Disk",
        type_ = "integer",
        description = "Bytes of the disk layer this checkpoint adds"
    ),
    printcolumn(
        json_path = ".status.guestTime",
        name = "Guest Time",
        type_ = "string",
        description = "Guest wall clock at the checkpoint"
    ),
    printcolumn(
        json_path = ".status.parent",
        name = "Parent",
        type_ = "string",
        priority = 1
    ),
    printcolumn(
        json_path = ".metadata.creationTimestamp",
        name = "Age",
        type_ = "date"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointSpec {
    /// Sandbox to checkpoint, in the checkpoint's namespace. It must be `Running`.
    #[schemars(length(min = 1, max = 253))]
    #[x_kube(validation = Rule::new("self == oldSelf").message("sandbox cannot change; create another checkpoint"))]
    pub sandbox: String,

    /// Delete the checkpoint this long after its creation, e.g. `48h`. Can be
    /// changed. No TTL: it lives until deleted. A checkpoint that a sandbox's disk
    /// still builds on is deleted only once that sandbox is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Ttl>,
}

/// Written by the node agent holding the checkpoint.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointStatus {
    #[serde(default)]
    pub phase: CheckpointPhase,

    /// Every node holding a complete copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,

    /// Virtual CPUs of the checkpointed VM; sandboxes started from it get the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int32"))]
    pub vcpus: Option<u32>,

    /// Guest RAM of the checkpointed VM; sandboxes started from it get the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Quantity>,

    /// Disk size of the checkpointed VM; sandboxes started from it get the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<Quantity>,

    /// Bytes the RAM file occupies on disk (it is sparse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int64"))]
    pub memory_bytes: Option<u64>,

    /// Bytes of the disk layer this checkpoint adds; layers it shares with its
    /// parent are not counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int64"))]
    pub disk_bytes: Option<u64>,

    /// The guest's wall clock at the checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_time: Option<Time>,

    /// Checkpoint this one's sandbox was started from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,

    /// What a restore must match exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,

    /// Why the checkpoint is in its phase, mainly for `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckpointPhase {
    /// Accepted; the save has not started.
    #[default]
    Pending,
    /// The sandbox is paused and its state is being written.
    Saving,
    /// Complete; sandboxes can start from it.
    Ready,
    /// The save did not complete; nothing was published. Terminal.
    Failed,
}

/// Everything a restore must run exactly as the checkpoint was taken. A restore
/// runs this launcher image with this CPU model.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    /// Launcher image, by digest.
    pub launcher_image: String,
    /// QEMU version string.
    pub qemu: String,
    /// QEMU machine type, e.g. `pc-q35-11.1`.
    pub machine_type: String,
    /// Guest CPU model.
    pub cpu_model: String,
    /// Digest of the VM's device configuration.
    pub devices: String,
}
