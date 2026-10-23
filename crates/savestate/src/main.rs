use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
    Controller(ConfigPath),
    /// Run the node agent (one per node, as a DaemonSet).
    Node(ConfigPath),
    /// Run the launcher for one VM (inside its pod; started by the controller).
    Launcher,
    /// Print the CustomResourceDefinitions as YAML, for `kubectl apply -f -`.
    Crds,
    #[command(flatten)]
    Verb(savestate::cli::Verb),
}

#[derive(Args)]
struct ConfigPath {
    /// Installation config file (see deploy/README.md).
    #[arg(
        long,
        env = "SAVESTATE_CONFIG",
        default_value = "/etc/savestate/config.yaml"
    )]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Controller(args) => {
            savestate::controller::run(savestate::load_config(&args.config)?)
        }
        Command::Node(args) => savestate::node::run(savestate::load_config(&args.config)?),
        Command::Launcher => savestate::launcher::run(),
        Command::Crds => {
            print!("{}", savestate::crds_yaml()?);
            Ok(())
        }
        Command::Verb(verb) => savestate::cli::run(verb),
    }
}
