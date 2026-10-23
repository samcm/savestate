use std::{io::Write, net::SocketAddr, path::PathBuf};

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
    Controller(Controller),
    /// Run the node agent (one per node, as a `DaemonSet`).
    Node(ConfigPath),
    /// Run the launcher for one VM (inside its pod; started by the controller).
    Launcher,
    /// Print the `CustomResourceDefinition`s as YAML, for `kubectl apply -f -`.
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

#[derive(Args)]
struct Controller {
    #[command(flatten)]
    config: ConfigPath,
    /// Address serving Prometheus metrics at `/metrics`.
    #[arg(long, env = "SAVESTATE_METRICS_LISTEN", default_value = "0.0.0.0:9090")]
    metrics_listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Controller(args) => {
            let config = savestate::load_config(&args.config.config)?;
            savestate::telemetry::init_logging();
            savestate::telemetry::init_metrics(args.metrics_listen)?;
            savestate::controller::run(config).await
        }
        Command::Node(args) => savestate::node::run(savestate::load_config(&args.config)?),
        Command::Launcher => savestate::launcher::run(),
        Command::Crds => {
            let yaml = savestate::crds_yaml()?;
            std::io::stdout().lock().write_all(yaml.as_bytes())?;
            Ok(())
        }
        Command::Verb(verb) => savestate::cli::run(&verb),
    }
}
