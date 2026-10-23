//! The launcher (one per VM pod): starts and supervises QEMU, drives it over QMP for
//! checkpoints and restores, relays exec and file traffic to the guest agent over
//! vsock, and streams the serial console to the pod log. It has no Kubernetes
//! credentials; the node agent drives it over a unix socket.

/// Run the launcher until its VM exits.
pub fn run() -> anyhow::Result<()> {
    Err(crate::not_implemented("launcher"))
}
