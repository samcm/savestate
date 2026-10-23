//! The user-facing verbs. Each is shorthand over the `Sandbox` and `Checkpoint`
//! objects (see "API" in `docs/design.md`).

use clap::{Args, Subcommand, ValueEnum};

#[derive(Subcommand)]
pub enum Verb {
    /// Create a sandbox from a disk image or a checkpoint.
    Create(Create),
    /// Take a checkpoint of a running sandbox; it pauses for a few seconds, then
    /// carries on.
    Checkpoint(Checkpoint),
    /// Checkpoint a sandbox and start sandboxes from that checkpoint.
    Fork(Fork),
    /// Run a command in a sandbox.
    Exec(Exec),
    /// Copy files into or out of a sandbox (`NAME:PATH` names a path in a sandbox).
    Cp(Cp),
    /// Delete a sandbox or a checkpoint.
    Delete(Delete),
}

#[derive(Args)]
pub struct Create {
    /// Sandbox name.
    pub name: String,
    /// Disk image to boot, e.g. `ghcr.io/example/base@sha256:…`.
    #[arg(long, required_unless_present = "from", conflicts_with = "from")]
    pub image: Option<String>,
    /// Checkpoint to start from; image, vCPUs, memory and disk come from it.
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub vcpus: Option<u32>,
    /// Guest memory, e.g. `8Gi`.
    #[arg(long)]
    pub memory: Option<String>,
    /// Disk size, e.g. `60Gi`.
    #[arg(long)]
    pub disk: Option<String>,
    /// Guest clock after starting from a checkpoint (with `--from`) [default: frozen].
    #[arg(long, value_enum)]
    pub clock: Option<Clock>,
    /// Delete the sandbox after this long, e.g. `7d`.
    #[arg(long)]
    pub ttl: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Clock {
    /// The guest believes no time passed while it was saved.
    Frozen,
    /// The guest's wall clock is set to the host's current time after the restore.
    Stepped,
}

#[derive(Args)]
pub struct Checkpoint {
    /// Sandbox to checkpoint.
    pub sandbox: String,
    /// Checkpoint name [default: generated from the sandbox name].
    #[arg(long)]
    pub name: Option<String>,
    /// Delete the checkpoint after this long, e.g. `48h`.
    #[arg(long)]
    pub ttl: Option<String>,
}

#[derive(Args)]
pub struct Fork {
    /// Sandbox to fork.
    pub sandbox: String,
    /// Number of new sandboxes.
    #[arg(short = 'n', long, default_value_t = 1)]
    pub count: u32,
}

#[derive(Args)]
pub struct Exec {
    /// Sandbox to run the command in.
    pub sandbox: String,
    /// Command and arguments.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct Cp {
    /// Source: a local path or `NAME:PATH`.
    pub source: String,
    /// Destination: a local path or `NAME:PATH`.
    pub destination: String,
}

#[derive(Args)]
pub struct Delete {
    pub kind: Kind,
    pub name: String,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    Sandbox,
    Checkpoint,
}

/// Run one verb against the cluster in the current kubeconfig context.
pub fn run(verb: Verb) -> anyhow::Result<()> {
    let name = match verb {
        Verb::Create(_) => "create",
        Verb::Checkpoint(_) => "checkpoint",
        Verb::Fork(_) => "fork",
        Verb::Exec(_) => "exec",
        Verb::Cp(_) => "cp",
        Verb::Delete(_) => "delete",
    };
    Err(crate::not_implemented(name))
}
