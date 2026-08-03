//! fvm-anvil process entry point.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use fvm_anvil_node::{ActorBundle, DEFAULT_CHAIN_ID, NodeConfig, NodeService};
use fvm_anvil_rpc::{RpcService, ServerConfig, http_router};

const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const DEFAULT_PORT: u16 = 8_545;

#[derive(Debug, Parser)]
#[command(name = "fvm-anvil", version, about)]
struct Cli {
    /// Address on which the JSON-RPC server listens.
    #[arg(long, default_value_t = DEFAULT_HOST)]
    host: IpAddr,

    /// Port on which the JSON-RPC server listens.
    #[arg(long, default_value_t = DEFAULT_PORT, value_parser = parse_port)]
    port: u16,

    /// EIP-155 chain ID fixed at genesis.
    #[arg(long, default_value_t = DEFAULT_CHAIN_ID, value_parser = parse_chain_id)]
    chain_id: u64,

    /// Local v18 builtin-actors CAR to use instead of the pinned official bundle.
    #[arg(long, value_name = "PATH")]
    actor_bundle: Option<PathBuf>,

    /// Suppress startup and shutdown diagnostics.
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug)]
struct AppConfig {
    listen_addr: SocketAddr,
    node: NodeConfig,
    quiet: bool,
}

impl TryFrom<Cli> for AppConfig {
    type Error = anyhow::Error;

    fn try_from(cli: Cli) -> Result<Self> {
        let actor_bundle = match cli.actor_bundle {
            Some(path) => ActorBundle::local(path).context("invalid local actor bundle")?,
            None => ActorBundle::Official,
        };
        let node =
            NodeConfig::new(cli.chain_id, actor_bundle).context("invalid node configuration")?;

        Ok(Self { listen_addr: SocketAddr::new(cli.host, cli.port), node, quiet: cli.quiet })
    }
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port = value.parse::<u16>().map_err(|error| error.to_string())?;
    if port == 0 {
        return Err("port must be greater than zero".to_owned());
    }
    Ok(port)
}

fn parse_chain_id(value: &str) -> Result<u64, String> {
    let chain_id = value.parse::<u64>().map_err(|error| error.to_string())?;
    if chain_id == 0 {
        return Err("chain ID must be greater than zero".to_owned());
    }
    Ok(chain_id)
}

fn initialize_tracing(quiet: bool) {
    if quiet {
        return;
    }

    let _ = tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let quiet = cli.quiet;
    initialize_tracing(quiet);

    let result = async {
        let config = AppConfig::try_from(cli)?;
        run(config).await
    }
    .await;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if !quiet {
                tracing::error!(error = %format_args!("{error:#}"), "fvm-anvil failed");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(config: AppConfig) -> Result<()> {
    let AppConfig { listen_addr, node, quiet } = config;

    // NodeService::start does not return until genesis and the worker are ready. Binding only after
    // that point ensures invalid genesis configuration never exposes an RPC transport.
    let service = NodeService::start(node).context("failed to start node")?;
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind JSON-RPC listener at {listen_addr}"))?;
    let bound_addr = listener.local_addr().context("failed to inspect JSON-RPC listener")?;
    let router = http_router(ServerConfig::default(), RpcService::new(service.handle()));

    if !quiet {
        tracing::info!(url = %format_args!("http://{bound_addr}"), "JSON-RPC listening");
    }

    let serve_result =
        axum::serve(listener, router).with_graceful_shutdown(shutdown_signal(quiet)).await;
    drop(service);

    serve_result.context("JSON-RPC server failed")
}

async fn shutdown_signal(quiet: bool) {
    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            if !quiet {
                tracing::info!("shutdown signal received");
            }
        }
        Err(error) => {
            if !quiet {
                tracing::error!(%error, "failed to listen for shutdown signal");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::error::ErrorKind;

    use super::*;

    #[test]
    fn defaults_select_official_bundle_and_loopback_rpc() {
        let cli = Cli::try_parse_from(["fvm-anvil"]).expect("default arguments must parse");
        let config = AppConfig::try_from(cli).expect("default configuration must be valid");

        assert_eq!(config.listen_addr, SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)));
        assert_eq!(config.node.chain_id(), DEFAULT_CHAIN_ID);
        assert_eq!(config.node.actor_bundle(), &ActorBundle::Official);
        assert!(!config.quiet);
    }

    #[test]
    fn local_bundle_builds_local_genesis_config() {
        let cli = Cli::try_parse_from([
            "fvm-anvil",
            "--host",
            "::1",
            "--port",
            "9545",
            "--chain-id",
            "1234",
            "--actor-bundle",
            "actors.car",
            "--quiet",
        ])
        .expect("valid local arguments must parse");
        let config = AppConfig::try_from(cli).expect("local configuration must be valid");

        assert_eq!(config.listen_addr, "[::1]:9545".parse().expect("address must be valid"));
        assert_eq!(config.node.chain_id(), 1_234);
        assert_eq!(
            config.node.actor_bundle(),
            &ActorBundle::Local { path: Path::new("actors.car").to_path_buf() }
        );
        assert!(config.quiet);
    }

    #[test]
    fn invalid_listener_and_genesis_values_are_rejected_during_parsing() {
        for arguments in [
            ["fvm-anvil", "--host", "localhost"],
            ["fvm-anvil", "--port", "0"],
            ["fvm-anvil", "--chain-id", "0"],
        ] {
            let error =
                Cli::try_parse_from(arguments).expect_err("invalid argument must be rejected");
            assert_eq!(error.kind(), ErrorKind::ValueValidation);
        }
    }
}
