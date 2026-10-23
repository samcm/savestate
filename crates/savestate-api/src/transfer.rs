use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An explicit copy of a checkpoint to another node. When it completes, the node is
/// added to the checkpoint's `status.nodes`; the copy belongs to the checkpoint, so
/// deleting the transfer afterwards only deletes this record. Deleting a transfer
/// that is still copying cancels it. The spec cannot change.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "savestate.dev",
    version = "v1alpha1",
    kind = "Transfer",
    namespaced,
    doc = "A copy of a checkpoint to another node",
    status = "TransferStatus",
    shortname = "xfer",
    category = "savestate",
    derive = "PartialEq",
    selectable = ".spec.checkpoint",
    selectable = ".spec.to.node",
    selectable = ".status.from",
    printcolumn(json_path = ".spec.checkpoint", name = "Checkpoint", type_ = "string"),
    printcolumn(json_path = ".status.from", name = "From", type_ = "string"),
    printcolumn(json_path = ".spec.to.node", name = "To", type_ = "string"),
    printcolumn(json_path = ".status.phase", name = "Phase", type_ = "string"),
    printcolumn(
        json_path = ".status.bytesDone",
        name = "Done",
        type_ = "integer",
        description = "Bytes copied"
    ),
    printcolumn(
        json_path = ".status.bytesTotal",
        name = "Total",
        type_ = "integer",
        description = "Bytes to copy"
    ),
    printcolumn(
        json_path = ".metadata.creationTimestamp",
        name = "Age",
        type_ = "date"
    )
)]
#[x_kube(validation = Rule::new("self == oldSelf").message("a transfer cannot change; delete it and create another"))]
#[serde(rename_all = "camelCase")]
pub struct TransferSpec {
    /// Checkpoint to copy, in the transfer's namespace.
    #[schemars(length(min = 1, max = 253))]
    pub checkpoint: String,

    /// Where the copy goes.
    pub to: TransferTarget,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransferTarget {
    /// Node to copy the checkpoint to.
    #[schemars(length(min = 1, max = 253))]
    pub node: String,
}

/// Written by the node agent receiving the copy.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransferStatus {
    #[serde(default)]
    pub phase: TransferPhase,

    /// Node the checkpoint is copied from, once chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int64"))]
    pub bytes_done: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int64"))]
    pub bytes_total: Option<u64>,

    /// Why the transfer is in its phase, mainly for `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransferPhase {
    /// Waiting for the checkpoint to be `Ready` or for the node's transfer slot.
    #[default]
    Pending,
    /// Copying from `status.from`.
    Copying,
    /// The node holds a complete copy and is listed in the checkpoint's
    /// `status.nodes`.
    Ready,
    /// The copy did not complete; partial files are removed. Terminal.
    Failed,
}
