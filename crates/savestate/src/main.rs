use std::{io::Write, net::SocketAddr, path::PathBuf, process::ExitCode};

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
    Controller(Daemon),
    /// Run the node agent (one per node, as a `DaemonSet`).
    Node(Node),
    /// Run the launcher for one VM (inside its pod; started by the controller).
    Launcher(savestate::launcher::Args),
    /// Talk to the guest agent of this pod's VM (inside a VM pod; used by `exec` and `cp`).
    #[command(subcommand, hide = true)]
    Guest(savestate::launcher::guest::Guest),
    /// Print the `CustomResourceDefinition`s as YAML, for `kubectl apply -f -`.
    Crds,
    #[command(flatten)]
    Verb(savestate::cli::Verb),
}

#[derive(Args)]
struct Node {
    #[command(flatten)]
    daemon: Daemon,
    /// Only check that this node runs the configured guest CPU model, then exit.
    #[arg(long)]
    probe_cpu: bool,
}

#[derive(Args)]
struct Daemon {
    /// Installation config file (see deploy/README.md).
    #[arg(
        long,
        env = "SAVESTATE_CONFIG",
        default_value = "/etc/savestate/config.yaml"
    )]
    config: PathBuf,
    /// Address serving Prometheus metrics at `/metrics`.
    #[arg(long, env = "SAVESTATE_METRICS_LISTEN", default_value = "0.0.0.0:9090")]
    metrics_listen: SocketAddr,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Launcher(args) => return savestate::launcher::run(&args),
        Command::Guest(command) => return savestate::launcher::guest::run(command),
        Command::Verb(verb) => return savestate::cli::main(verb),
        _ => {}
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let result = runtime.map_err(anyhow::Error::from).and_then(|runtime| {
        runtime.block_on(async {
            match cli.command {
                Command::Controller(args) => {
                    let config = savestate::load_config(&args.config)?;
                    savestate::telemetry::init_logging();
                    savestate::telemetry::init_metrics(args.metrics_listen)?;
                    savestate::controller::run(config).await
                }
                Command::Node(Node { daemon, probe_cpu }) => {
                    let config = savestate::load_config(&daemon.config)?;
                    savestate::telemetry::init_logging();
                    if probe_cpu {
                        let probed = savestate::node::probe_cpu(&config);
                        if let Err(error) = &probed {
                            let _ = std::fs::write("/dev/termination-log", format!("{error:#}"));
                        }
                        return probed;
                    }
                    savestate::telemetry::init_metrics(daemon.metrics_listen)?;
                    savestate::node::run(config).await
                }
                Command::Crds => {
                    let yaml = savestate::crds_yaml()?;
                    std::io::stdout().lock().write_all(yaml.as_bytes())?;
                    Ok(())
                }
                Command::Launcher(_) | Command::Guest(_) | Command::Verb(_) => Ok(()),
            }
        })
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
