//! savestate: QEMU/KVM sandboxes on Kubernetes that save their complete state as
//! checkpoints and start any number of new sandboxes from one.
//!
//! One static binary provides every role; each role is a module here (see
//! "Architecture" in `docs/design.md`):
//!
//! - [`controller`]: the cluster-wide operator (Deployment, one replica).
//! - [`node`]: the per-node agent (DaemonSet) that owns `/var/lib/savestate`.
//! - [`launcher`]: one per VM pod; starts and supervises QEMU over QMP.
//! - [`cli`]: the user-facing verbs, shorthand over the `Sandbox` and `Checkpoint`
//!   objects.
//!
//! The CRD types live in the `savestate-api` crate. QEMU, QMP and guest-agent code
//! comes from orbd's shared crates.

pub mod cli;
pub mod controller;
pub mod launcher;
pub mod node;

/// The error every role and verb returns until it is built.
fn not_implemented(what: &str) -> anyhow::Error {
    anyhow::anyhow!("`savestate {what}` is not implemented yet")
}
