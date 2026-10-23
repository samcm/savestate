//! The controller (Deployment, one replica): creates one Pod per Sandbox, pins
//! sandboxes started from checkpoints to a node holding the checkpoint, enforces
//! TTLs, creates and schedules Transfers, and keeps the finalizers that protect
//! checkpoints still in use.

/// Run the controller until shutdown.
pub fn run() -> anyhow::Result<()> {
    Err(crate::not_implemented("controller"))
}
