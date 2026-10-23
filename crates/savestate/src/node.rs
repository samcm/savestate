//! The node agent (DaemonSet): owns `/var/lib/savestate` on its node (base images,
//! disk layers, checkpoints), tells the launchers on its node to checkpoint or
//! restore over their unix sockets, garbage-collects layers and checkpoints whose
//! objects are gone, serves and fetches transfers, and reports disk use. It is the
//! only component on a node that holds API credentials.

/// Run the node agent until shutdown.
pub fn run(_config: savestate_api::Config) -> anyhow::Result<()> {
    Err(crate::not_implemented("node"))
}
