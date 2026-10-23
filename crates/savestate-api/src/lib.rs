//! Kubernetes API types for savestate, as described under "API" in
//! `docs/design.md`: the custom resources [`Sandbox`], [`Checkpoint`] and
//! [`Transfer`] in the API group `savestate.dev`, version `v1alpha1`, and the
//! installation [`config`].
//!
//! Validation is CEL rules (and schema constraints) in the generated CRDs; there are
//! no admission webhooks. `savestate crds` prints the CRDs; `deploy/crds/` holds them.

// `#[kube(printcolumn(...))]` repeats keys such as `type_ = "string"` by design.
#![allow(clippy::duplicated_attributes)]

pub mod checkpoint;
pub mod config;
pub mod sandbox;
pub mod transfer;
mod ttl;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt;

pub use checkpoint::{Checkpoint, CheckpointPhase, CheckpointSpec, CheckpointStatus, Fingerprint};
pub use config::Config;
pub use sandbox::{Clock, Sandbox, SandboxPhase, SandboxSource, SandboxSpec, SandboxStatus};
pub use transfer::{Transfer, TransferPhase, TransferSpec, TransferStatus, TransferTarget};
pub use ttl::{ParseTtlError, Ttl};

/// The CRDs for every kind, in install order.
pub fn crds() -> Vec<CustomResourceDefinition> {
    vec![Sandbox::crd(), Checkpoint::crd(), Transfer::crd()]
}
