use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "savestate",
    version,
    about = "QEMU/KVM sandboxes on Kubernetes with checkpoints"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the controller (one replica per cluster).
    Controller,
    /// Run the node agent (one per node, as a DaemonSet).
    Node,
    /// Run the launcher for one VM (inside its pod; started by the controller).
    Launcher,
    #[command(flatten)]
    Verb(savestate::cli::Verb),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Controller => savestate::controller::run(),
        Command::Node => savestate::node::run(),
        Command::Launcher => savestate::launcher::run(),
        Command::Verb(verb) => savestate::cli::run(verb),
    }
}
