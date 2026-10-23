use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A copy of a checkpoint between a node and an object store: an export from a node
/// holding it to a store, or an import from a store holding it to a node. The node
/// agent of the transfer's node does the copy. When it completes, the store is added
/// to the checkpoint's `status.stores` (export) or the node to its `status.nodes`
/// (import); the copy belongs to the checkpoint, so deleting the transfer afterwards
/// only deletes this record. Deleting a transfer that is still copying cancels it.
/// The spec cannot change.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "savestate.dev",
    version = "v1alpha1",
    kind = "Transfer",
    namespaced,
    doc = "A copy of a checkpoint between a node and an object store",
    status = "TransferStatus",
    shortname = "xfer",
    category = "savestate",
    derive = "PartialEq",
    selectable = ".spec.checkpoint",
    selectable = ".spec.from.node",
    selectable = ".spec.to.node",
    printcolumn(json_path = ".spec.checkpoint", name = "Checkpoint", type_ = "string"),
    printcolumn(json_path = ".spec.from.node", name = "From Node", type_ = "string"),
    printcolumn(json_path = ".spec.from.store", name = "From Store", type_ = "string"),
    printcolumn(json_path = ".spec.to.node", name = "To Node", type_ = "string"),
    printcolumn(json_path = ".spec.to.store", name = "To Store", type_ = "string"),
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
#[x_kube(
    validation = Rule::new("has(self.from.store) != has(self.to.store)")
        .message("a transfer copies between a node and a store: from a node to a store, or from a store to a node"),
    validation = Rule::new("self == oldSelf").message("a transfer cannot change; delete it and create another"),
)]
#[serde(rename_all = "camelCase")]
pub struct TransferSpec {
    /// Checkpoint to copy, in the transfer's namespace.
    #[schemars(length(min = 1, max = 253))]
    pub checkpoint: String,

    /// Where the copy comes from: a node holding the checkpoint (an export), or a
    /// store holding it (an import).
    pub from: TransferEnd,

    /// Where the copy goes: a store (an export) or a node (an import).
    pub to: TransferEnd,
}

/// One end of a transfer: exactly one of a node and a store.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[x_kube(validation = Rule::new("has(self.node) != has(self.store)").message("set exactly one of node and store"))]
#[serde(rename_all = "camelCase")]
pub struct TransferEnd {
    /// A node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 253))]
    pub node: Option<String>,

    /// An object store, by its name in the installation config's `stores`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 63))]
    pub store: Option<String>,
}

impl TransferEnd {
    pub fn node(name: impl Into<String>) -> Self {
        Self {
            node: Some(name.into()),
            store: None,
        }
    }

    pub fn store(name: impl Into<String>) -> Self {
        Self {
            node: None,
            store: Some(name.into()),
        }
    }
}

/// Written by the node agent doing the copy.
#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransferStatus {
    #[serde(default)]
    pub phase: TransferPhase,

    /// Bytes of the checkpoint's files copied, or found already at the destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int64"))]
    pub bytes_done: Option<u64>,

    /// Bytes of the checkpoint's files (their data, without holes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "int64"))]
    pub bytes_total: Option<u64>,

    /// Why the transfer is in its phase, mainly for `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransferPhase {
    /// Waiting for the node's transfer slot.
    #[default]
    Pending,
    /// Copying.
    Copying,
    /// The destination holds a complete copy and is listed in the checkpoint's
    /// `status.stores` or `status.nodes`.
    Ready,
    /// The copy did not complete; partial files are removed. Terminal.
    Failed,
}
