//! Kubernetes API types for savestate.
//!
//! This crate will hold the custom resources described under "API" in
//! `docs/design.md`: `Sandbox`, `Checkpoint` and, after the MVP, `Transfer`, in the
//! API group `savestate.dev`, version `v1alpha1`. Their validation is CEL rules in
//! the generated CRDs; there are no admission webhooks.
//!
//! The controller, the node agent and the CLI in the `savestate` binary all use these
//! types. Nothing is defined yet.
