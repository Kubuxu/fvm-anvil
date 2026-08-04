//! fvm-anvil process entry point.
use std::env;

use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use alloy_primitives::{Address, U256};
use alloy_signer_local::{
    MnemonicBuilder, PrivateKeySigner,
    coins_bip39::{English, Mnemonic},
};
use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Parser, ValueEnum};
use fvm_anvil_node::{
    ACTOR_BUNDLE_VERSION, ActorBundle, DEFAULT_ACTOR_BUNDLE_ROOT, DEFAULT_BASE_FEE_PER_GAS,
    DEFAULT_BLOCK_GAS_LIMIT, DEFAULT_CHAIN_ID, DEFAULT_GENESIS_BLOCK_NUMBER,
    DEFAULT_GENESIS_TIMESTAMP, DEFAULT_PREFUNDED_BALANCE_FIL, GenesisAccount, NETWORK_VERSION,
    NodeConfig, NodeHandle, NodeService,
};
use fvm_anvil_rpc::{RpcService, ServerConfig, http_router};
use rand_08::{SeedableRng, rngs::StdRng};
use serde::Serialize;

const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const DEFAULT_PORT: u16 = 8_545;
const DEFAULT_ACCOUNTS: u32 = 10;
const DEFAULT_MNEMONIC: &str = "test test test test test test test test test test test junk";
const DEFAULT_DERIVATION_PATH: &str = "m/44'/60'/0'/0/";
const ATTO_PER_FIL: u64 = 1_000_000_000_000_000_000;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    fn enabled(self) -> bool {
        match self {
            Self::Auto => io::stderr().is_terminal(),
            Self::Always => true,
            Self::Never => false,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "fvm-anvil", version, about)]
struct Cli {
    /// Address on which the JSON-RPC server listens.
    #[arg(long, env = "ANVIL_IP_ADDR", default_value_t = DEFAULT_HOST)]
    host: IpAddr,

    /// Port on which the JSON-RPC server listens.
    #[arg(long, short, default_value_t = DEFAULT_PORT, value_parser = parse_port)]
    port: u16,

    /// Number of deterministic development accounts to create.
    #[arg(long, short, default_value_t = DEFAULT_ACCOUNTS)]
    accounts: u32,

    /// Balance of every generated development account in whole FIL.
    #[arg(long, default_value_t = DEFAULT_PREFUNDED_BALANCE_FIL)]
    balance: u64,

    /// BIP-39 mnemonic used to derive development accounts.
    #[arg(long, short, conflicts_with_all = ["mnemonic_random", "mnemonic_seed"])]
    mnemonic: Option<String>,

    /// Generate a random BIP-39 mnemonic, optionally with the given word count.
    #[arg(
        long,
        conflicts_with_all = ["mnemonic", "mnemonic_seed"],
        default_missing_value = "12",
        num_args = 0..=1
    )]
    mnemonic_random: Option<usize>,

    /// Generate a deterministic test-only mnemonic from a seed.
    #[arg(
        long = "mnemonic-seed-unsafe",
        conflicts_with_all = ["mnemonic", "mnemonic_random"]
    )]
    mnemonic_seed: Option<u64>,

    /// Base derivation path for generated development accounts.
    #[arg(long, default_value = DEFAULT_DERIVATION_PATH)]
    derivation_path: String,

    /// Additional prefunded account as ADDRESS:AMOUNT, where amount is whole FIL.
    #[arg(long, value_name = "ADDRESS:AMOUNT", num_args = 1..)]
    fund_accounts: Vec<String>,

    /// EIP-155 chain ID fixed at genesis.
    #[arg(
        long,
        visible_alias = "chain",
        default_value_t = DEFAULT_CHAIN_ID,
        value_parser = parse_chain_id
    )]
    chain_id: u64,

    /// Timestamp of the synthetic genesis block.
    #[arg(long, default_value_t = DEFAULT_GENESIS_TIMESTAMP, conflicts_with = "state")]
    timestamp: u64,

    /// Number of the synthetic genesis block.
    #[arg(long, default_value_t = DEFAULT_GENESIS_BLOCK_NUMBER, conflicts_with = "state")]
    number: u64,

    /// Filecoin block gas limit exposed through Ethereum RPC.
    #[arg(
        long,
        visible_alias = "block-gas-limit",
        default_value_t = DEFAULT_BLOCK_GAS_LIMIT,
        value_parser = parse_nonzero_u64
    )]
    gas_limit: u64,

    /// Initial FVM base fee in attoFIL per gas unit.
    #[arg(
        long = "block-base-fee-per-gas",
        visible_alias = "base-fee",
        default_value_t = DEFAULT_BASE_FEE_PER_GAS,
        conflicts_with = "state"
    )]
    base_fee_per_gas: u128,

    /// Disable automatic mining at startup.
    #[arg(long, visible_alias = "no-mine", conflicts_with_all = ["block_time", "mixed_mining"])]
    no_mining: bool,

    /// Mine a block periodically at this interval in seconds.
    #[arg(
        long,
        short,
        visible_alias = "blockTime",
        value_parser = parse_nonzero_u64,
        conflicts_with = "no_mining"
    )]
    block_time: Option<u64>,

    /// Mine both immediately on submission and at the configured interval.
    #[arg(long, requires = "block_time", conflicts_with = "no_mining")]
    mixed_mining: bool,

    /// Local v18 builtin-actors CAR to use instead of the pinned official bundle.
    #[arg(long, value_name = "PATH")]
    actor_bundle: Option<PathBuf>,

    /// Prepared Press state archive to use as the initial Filecoin state.
    #[arg(long, value_name = "PRESS_ARCHIVE")]
    state: Option<PathBuf>,

    /// CORS allow-origin header value.
    #[arg(long, default_value = "*", conflicts_with = "no_cors")]
    allow_origin: String,

    /// Disable CORS response headers.
    #[arg(long)]
    no_cors: bool,

    /// Disable the default JSON-RPC request body size limit.
    #[arg(long)]
    no_request_size_limit: bool,

    /// Write machine-readable startup configuration to this file.
    #[arg(long, value_name = "FILE")]
    config_out: Option<PathBuf>,

    /// Suppress startup, shutdown, and runtime diagnostics.
    #[arg(long, short, visible_alias = "silent")]
    quiet: bool,

    /// Colorize diagnostics.
    #[arg(long, value_enum, default_value_t)]
    color: ColorChoice,

    /// Increase runtime diagnostic verbosity.
    #[arg(long, short, action = ArgAction::Count)]
    verbosity: u8,
}

struct DevelopmentWallet {
    mnemonic: String,
    derivation_path: String,
    signers: Vec<PrivateKeySigner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MiningMode {
    Instant,
    Manual,
    Interval { seconds: u64, mixed: bool },
}

impl MiningMode {
    const fn automine(self) -> bool {
        matches!(self, Self::Instant | Self::Interval { mixed: true, .. })
    }
}

struct AppConfig {
    listen_addr: SocketAddr,
    node: NodeConfig,
    server: ServerConfig,
    wallet: DevelopmentWallet,
    genesis_accounts: Vec<GenesisAccount>,
    mining: MiningMode,
    config_out: Option<PathBuf>,
    quiet: bool,
}

impl TryFrom<Cli> for AppConfig {
    type Error = anyhow::Error;

    fn try_from(cli: Cli) -> Result<Self> {
        let actor_bundle_cache =
            if cli.actor_bundle.is_none() { default_actor_bundle_cache_path() } else { None };
        let actor_bundle = match &cli.actor_bundle {
            Some(path) => ActorBundle::local(path).context("invalid local actor bundle")?,
            None => ActorBundle::Official,
        };
        let wallet = development_wallet(&cli)?;
        let default_balance = whole_fil(cli.balance)?;
        let mut genesis_accounts = wallet
            .signers
            .iter()
            .map(|signer| GenesisAccount::new(signer.address(), default_balance))
            .collect::<Vec<_>>();
        for account in &cli.fund_accounts {
            let account = parse_funded_account(account)?;
            if let Some(existing) = genesis_accounts
                .iter_mut()
                .find(|existing| existing.ethereum() == account.ethereum())
            {
                *existing = account;
            } else {
                genesis_accounts.push(account);
            }
        }

        let mining = match cli.block_time {
            Some(seconds) => MiningMode::Interval { seconds, mixed: cli.mixed_mining },
            None if cli.no_mining => MiningMode::Manual,
            None => MiningMode::Instant,
        };
        let node = NodeConfig::new(cli.chain_id, actor_bundle)
            .context("invalid node configuration")?
            .with_actor_bundle_cache(actor_bundle_cache)
            .with_prepared_state(cli.state)
            .context("invalid prepared-state configuration")?
            .with_genesis_accounts(genesis_accounts.clone())
            .with_genesis_block(cli.number, cli.timestamp)
            .context("invalid genesis block configuration")?
            .with_block_gas_limit(cli.gas_limit)
            .context("invalid block gas limit")?
            .with_base_fee_per_gas(cli.base_fee_per_gas)
            .with_automine(mining.automine());

        let allow_origin = cli
            .allow_origin
            .parse::<axum::http::HeaderValue>()
            .context("invalid CORS allow-origin header")?;
        let mut server = ServerConfig::default().with_allow_origin(allow_origin);
        server.no_cors = cli.no_cors;
        server.no_request_size_limit = cli.no_request_size_limit;

        Ok(Self {
            listen_addr: SocketAddr::new(cli.host, cli.port),
            node,
            server,
            wallet,
            genesis_accounts,
            mining,
            config_out: cli.config_out,
            quiet: cli.quiet,
        })
    }
}

fn development_wallet(cli: &Cli) -> Result<DevelopmentWallet> {
    let mnemonic = if let Some(mnemonic) = &cli.mnemonic {
        mnemonic.clone()
    } else if let Some(word_count) = cli.mnemonic_random {
        Mnemonic::<English>::new_with_count(&mut rand_08::thread_rng(), word_count)
            .map_err(|error| anyhow!("invalid random mnemonic word count: {error}"))?
            .to_phrase()
    } else if let Some(seed) = cli.mnemonic_seed {
        Mnemonic::<English>::new(&mut StdRng::seed_from_u64(seed)).to_phrase()
    } else {
        DEFAULT_MNEMONIC.to_owned()
    };

    let mut derivation_path = cli.derivation_path.clone();
    if !derivation_path.ends_with('/') {
        derivation_path.push('/');
    }
    let mut signers = Vec::new();
    signers
        .try_reserve_exact(cli.accounts as usize)
        .context("development account count exceeds available memory")?;
    for index in 0..cli.accounts {
        let signer = MnemonicBuilder::<English>::default()
            .phrase(&mnemonic)
            .derivation_path(format!("{derivation_path}{index}"))
            .context("invalid account derivation path")?
            .build()
            .context("failed to derive development account")?;
        signers.push(signer);
    }
    Ok(DevelopmentWallet { mnemonic, derivation_path, signers })
}

fn parse_funded_account(value: &str) -> Result<GenesisAccount> {
    let Some((address, amount)) = value.split_once(':') else {
        bail!("funded account must use ADDRESS:AMOUNT syntax");
    };
    if amount.contains(':') {
        bail!("funded account must contain exactly one ':' separator");
    }
    let address = address
        .parse::<Address>()
        .with_context(|| format!("invalid funded account address {address}"))?;
    let amount =
        amount.parse::<u64>().with_context(|| format!("invalid funded account amount {amount}"))?;
    Ok(GenesisAccount::new(address, whole_fil(amount)?))
}

fn whole_fil(amount: u64) -> Result<U256> {
    U256::from(amount)
        .checked_mul(U256::from(ATTO_PER_FIL))
        .ok_or_else(|| anyhow!("account balance exceeds the Ethereum quantity range"))
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

fn parse_nonzero_u64(value: &str) -> Result<u64, String> {
    let parsed = value.parse::<u64>().map_err(|error| error.to_string())?;
    if parsed == 0 {
        return Err("value must be greater than zero".to_owned());
    }
    Ok(parsed)
}
fn default_actor_bundle_cache_path() -> Option<PathBuf> {
    let cache_home = env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".cache"))
        })?;
    Some(
        cache_home.join("fvm-anvil").join(format!(
            "builtin-actors-v{ACTOR_BUNDLE_VERSION}-{DEFAULT_ACTOR_BUNDLE_ROOT}.car"
        )),
    )
}

fn initialize_tracing(quiet: bool, color: ColorChoice, verbosity: u8) {
    if quiet {
        return;
    }
    let own_level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let default_filter = format!(
        "info,fvm_anvil={own_level},fvm_anvil_node={own_level},fvm_anvil_rpc={own_level},fvm_anvil_eth={own_level}"
    );
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .with_env_filter(filter)
        .with_ansi(color.enabled())
        .with_writer(io::stderr)
        .try_init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let quiet = cli.quiet;
    initialize_tracing(quiet, cli.color, cli.verbosity);
    if cli.mnemonic_seed.is_some() && !quiet {
        tracing::warn!("--mnemonic-seed-unsafe produces insecure test keys");
    }

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
    let AppConfig {
        listen_addr,
        node,
        server,
        wallet,
        genesis_accounts,
        mining,
        config_out,
        quiet,
    } = config;
    let chain_id = node.chain_id();
    let block_gas_limit = node.block_gas_limit();

    // NodeService::start does not return until state initialization and the worker are ready.
    // Binding only after that point ensures invalid genesis or prepared state never exposes RPC.
    let service = NodeService::start(node).context("failed to start node")?;
    let startup = service.startup_info();
    let actor_bundle_root = startup.actor_bundle_root();
    let state_roots = startup.state_roots();
    let genesis_number = startup.initial_block_number();
    let genesis_timestamp = startup.initial_timestamp();
    let base_fee_per_gas = startup.base_fee_per_gas();
    let handle = service.handle();
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind JSON-RPC listener at {listen_addr}"))?;
    let bound_addr = listener.local_addr().context("failed to inspect JSON-RPC listener")?;
    let rpc_url = format!("http://{bound_addr}");
    let router = http_router(server, RpcService::new(handle.clone()));
    let accounts = handle.accounts().await.context("failed to inspect genesis accounts")?;

    let output = StartupOutput {
        accounts: &accounts,
        genesis_accounts: &genesis_accounts,
        wallet: &wallet,
        chain_id,
        genesis_number,
        genesis_timestamp,
        block_gas_limit,
        base_fee_per_gas,
        actor_bundle_root: actor_bundle_root.to_string(),
        prepared_state_root: state_roots.prepared.map(|root| root.to_string()),
        current_state_root: state_roots.current.to_string(),
        mining,
        rpc_url: &rpc_url,
    };
    if let Some(path) = config_out {
        write_config_file(&path, &output).context("failed to write startup configuration")?;
    }
    if !quiet {
        let mut stderr = io::stderr().lock();
        render_banner(&mut stderr, &output).context("failed to write startup banner")?;
    }

    let mining_task = start_interval_mining(handle, mining);
    let serve_result =
        axum::serve(listener, router).with_graceful_shutdown(shutdown_signal(quiet)).await;
    if let Some(task) = mining_task {
        task.abort();
        let _ = task.await;
    }
    drop(service);
    serve_result.context("JSON-RPC server failed")
}

fn start_interval_mining(
    node: NodeHandle,
    mining: MiningMode,
) -> Option<tokio::task::JoinHandle<()>> {
    let MiningMode::Interval { seconds, .. } = mining else {
        return None;
    };
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(seconds));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match node.startup_mining_mode_changed().await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(%error, "failed to inspect interval mining mode");
                    return;
                }
            }
            if let Err(error) = node.mine_blocks(1, None, None).await {
                tracing::error!(%error, "interval mining failed");
                return;
            }
        }
    }))
}

struct StartupOutput<'a> {
    accounts: &'a [Address],
    genesis_accounts: &'a [GenesisAccount],
    wallet: &'a DevelopmentWallet,
    chain_id: u64,
    genesis_number: u64,
    genesis_timestamp: u64,
    block_gas_limit: u64,
    base_fee_per_gas: u128,
    actor_bundle_root: String,
    prepared_state_root: Option<String>,
    current_state_root: String,
    mining: MiningMode,
    rpc_url: &'a str,
}

fn render_banner(mut writer: impl io::Write, output: &StartupOutput<'_>) -> io::Result<()> {
    writeln!(writer, "fvm-anvil {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(writer)?;
    writeln!(writer, "Available Accounts")?;
    writeln!(writer, "==================")?;
    for (index, account) in output.accounts.iter().enumerate() {
        let balance = output
            .genesis_accounts
            .iter()
            .find(|entry| entry.ethereum() == *account)
            .map_or(U256::ZERO, GenesisAccount::balance);
        writeln!(writer, "({index}) {account} ({balance} attoFIL)")?;
    }
    writeln!(writer)?;
    writeln!(writer, "Private Keys")?;
    writeln!(writer, "==================")?;
    for (index, signer) in output.wallet.signers.iter().enumerate() {
        writeln!(writer, "({index}) {}", signer.to_bytes())?;
    }
    writeln!(writer)?;
    writeln!(writer, "Wallet")?;
    writeln!(writer, "==================")?;
    writeln!(writer, "Mnemonic: {}", output.wallet.mnemonic)?;
    writeln!(writer, "Derivation path: {}", output.wallet.derivation_path)?;
    writeln!(writer)?;
    writeln!(writer, "Chain")?;
    writeln!(writer, "==================")?;
    writeln!(writer, "Chain ID: {}", output.chain_id)?;
    writeln!(writer, "Network version: {NETWORK_VERSION}")?;
    writeln!(writer, "Actor bundle version: {ACTOR_BUNDLE_VERSION}")?;
    writeln!(writer, "Actor bundle root: {}", output.actor_bundle_root)?;
    match &output.prepared_state_root {
        Some(root) => writeln!(writer, "Prepared state root: {root}")?,
        None => writeln!(writer, "Prepared state root: none")?,
    }
    writeln!(writer, "Current state root: {}", output.current_state_root)?;
    writeln!(writer, "Base fee: {} attoFIL/gas", output.base_fee_per_gas)?;
    writeln!(writer, "Block gas limit: {}", output.block_gas_limit)?;
    writeln!(writer, "Initial timestamp: {}", output.genesis_timestamp)?;
    writeln!(writer, "Initial block number: {}", output.genesis_number)?;
    writeln!(writer, "Mining mode: {}", mining_mode_name(output.mining))?;
    writeln!(writer)?;
    writeln!(writer, "Listening on {}", output.rpc_url)
}

const fn mining_mode_name(mode: MiningMode) -> &'static str {
    match mode {
        MiningMode::Instant => "instant",
        MiningMode::Manual => "manual",
        MiningMode::Interval { mixed: false, .. } => "interval",
        MiningMode::Interval { mixed: true, .. } => "mixed",
    }
}

#[derive(Serialize)]
struct ConfigFile<'a> {
    available_accounts: Vec<String>,
    private_keys: Vec<String>,
    wallet: WalletFile<'a>,
    chain_id: u64,
    base_fee: String,
    gas_price: String,
    gas_limit: String,
    genesis_timestamp: String,
    genesis_number: String,
    rpc_url: &'a str,
    filecoin: FilecoinFile<'a>,
}

#[derive(Serialize)]
struct WalletFile<'a> {
    mnemonic: &'a str,
    derivation_path: &'a str,
}

#[derive(Serialize)]
struct FilecoinFile<'a> {
    network_version: u32,
    actor_bundle_version: u32,
    actor_bundle_root: &'a str,
    prepared_state_root: Option<&'a str>,
    current_state_root: &'a str,
    mining_mode: &'static str,
}

fn write_config_file(path: &Path, output: &StartupOutput<'_>) -> Result<()> {
    let config = ConfigFile {
        available_accounts: output.accounts.iter().map(ToString::to_string).collect(),
        private_keys: output
            .wallet
            .signers
            .iter()
            .map(|signer| signer.to_bytes().to_string())
            .collect(),
        wallet: WalletFile {
            mnemonic: &output.wallet.mnemonic,
            derivation_path: &output.wallet.derivation_path,
        },
        chain_id: output.chain_id,
        base_fee: output.base_fee_per_gas.to_string(),
        gas_price: output.base_fee_per_gas.saturating_add(1).to_string(),
        gas_limit: output.block_gas_limit.to_string(),
        genesis_timestamp: output.genesis_timestamp.to_string(),
        genesis_number: output.genesis_number.to_string(),
        rpc_url: output.rpc_url,
        filecoin: FilecoinFile {
            network_version: NETWORK_VERSION.into(),
            actor_bundle_version: ACTOR_BUNDLE_VERSION,
            actor_bundle_root: &output.actor_bundle_root,
            prepared_state_root: output.prepared_state_root.as_deref(),
            current_state_root: &output.current_state_root,
            mining_mode: mining_mode_name(output.mining),
        },
    };
    let bytes = serde_json::to_vec_pretty(&config).context("failed to encode config-out JSON")?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file =
        options.open(path).with_context(|| format!("failed to open {}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))?;
    file.write_all(&bytes).with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(b"\n").with_context(|| format!("failed to finish {}", path.display()))?;
    file.sync_all().with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(())
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
    use std::fs::File;
    use std::path::Path;

    use clap::error::ErrorKind;

    use super::*;

    #[test]
    fn defaults_select_anvil_accounts_and_instant_mining() {
        let cli = Cli::try_parse_from(["fvm-anvil"]).expect("default arguments must parse");
        let config = AppConfig::try_from(cli).expect("default configuration must be valid");

        assert_eq!(config.listen_addr, SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)));
        assert_eq!(config.node.chain_id(), DEFAULT_CHAIN_ID);
        assert_eq!(config.node.actor_bundle(), &ActorBundle::Official);
        assert_eq!(config.node.genesis_accounts().len(), DEFAULT_ACCOUNTS as usize);
        assert_eq!(
            config.wallet.signers[0].address(),
            config.node.genesis_accounts()[0].ethereum()
        );
        assert_eq!(config.mining, MiningMode::Instant);
        assert!(config.node.prepared_state().is_none());
        assert!(!config.quiet);
    }

    #[test]
    fn local_bundle_and_environment_build_genesis_config() {
        let cli = Cli::try_parse_from([
            "fvm-anvil",
            "--host",
            "::1",
            "-p",
            "9545",
            "--chain",
            "1234",
            "--actor-bundle",
            "actors.car",
            "--accounts",
            "2",
            "--balance",
            "7",
            "--timestamp",
            "42",
            "--number",
            "9",
            "--gas-limit",
            "123456",
            "--base-fee",
            "77",
            "--no-mine",
            "--silent",
        ])
        .expect("valid local arguments must parse");
        let config = AppConfig::try_from(cli).expect("local configuration must be valid");

        assert_eq!(config.listen_addr, "[::1]:9545".parse().expect("address must be valid"));
        assert_eq!(config.node.chain_id(), 1_234);
        assert_eq!(
            config.node.actor_bundle(),
            &ActorBundle::Local { path: Path::new("actors.car").to_path_buf() }
        );
        assert_eq!(config.node.genesis_block_number(), 9);
        assert_eq!(config.node.genesis_timestamp(), 42);
        assert_eq!(config.node.block_gas_limit(), 123_456);
        assert_eq!(config.node.base_fee_per_gas(), 77);
        assert!(!config.node.automine());
        assert_eq!(config.mining, MiningMode::Manual);
        assert!(config.quiet);
    }

    #[test]
    fn prepared_state_is_genesis_time_configuration() {
        let cli = Cli::try_parse_from(["fvm-anvil", "--state", "prepared.press.car.zst"]).unwrap();
        let config = AppConfig::try_from(cli).unwrap();

        assert_eq!(config.node.prepared_state(), Some(Path::new("prepared.press.car.zst")));
        let error = Cli::try_parse_from([
            "fvm-anvil",
            "--state",
            "prepared.press.car.zst",
            "--timestamp",
            "1",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn funded_account_overrides_generated_balance_without_duplication() {
        let first = MnemonicBuilder::<English>::default()
            .phrase(DEFAULT_MNEMONIC)
            .build()
            .unwrap()
            .address();
        let cli = Cli::try_parse_from([
            "fvm-anvil",
            "--accounts",
            "1",
            "--fund-accounts",
            &format!("{first}:3"),
        ])
        .unwrap();
        let config = AppConfig::try_from(cli).unwrap();

        assert_eq!(config.genesis_accounts.len(), 1);
        assert_eq!(config.genesis_accounts[0].balance(), whole_fil(3).unwrap());
    }

    #[test]
    fn interval_and_mixed_mining_map_to_automine_contract() {
        let interval =
            AppConfig::try_from(Cli::try_parse_from(["fvm-anvil", "--block-time", "2"]).unwrap())
                .unwrap();
        assert_eq!(interval.mining, MiningMode::Interval { seconds: 2, mixed: false });
        assert!(!interval.node.automine());

        let mixed = AppConfig::try_from(
            Cli::try_parse_from(["fvm-anvil", "--blockTime", "2", "--mixed-mining"]).unwrap(),
        )
        .unwrap();
        assert_eq!(mixed.mining, MiningMode::Interval { seconds: 2, mixed: true });
        assert!(mixed.node.automine());
    }

    #[test]
    fn invalid_listener_genesis_and_mining_values_are_rejected_during_parsing() {
        for arguments in [
            vec!["fvm-anvil", "--host", "localhost"],
            vec!["fvm-anvil", "--port", "0"],
            vec!["fvm-anvil", "--chain-id", "0"],
            vec!["fvm-anvil", "--gas-limit", "0"],
            vec!["fvm-anvil", "--block-time", "0"],
            vec!["fvm-anvil", "--mixed-mining"],
            vec!["fvm-anvil", "--no-mining", "--block-time", "1"],
        ] {
            let error = Cli::try_parse_from(arguments).expect_err("invalid argument must fail");
            assert!(matches!(
                error.kind(),
                ErrorKind::ValueValidation
                    | ErrorKind::MissingRequiredArgument
                    | ErrorKind::ArgumentConflict
            ));
        }
    }

    #[test]
    fn config_out_contains_compatible_and_filecoin_fields() {
        let cli = Cli::try_parse_from(["fvm-anvil", "--accounts", "1"]).unwrap();
        let config = AppConfig::try_from(cli).unwrap();
        let accounts =
            config.genesis_accounts.iter().map(GenesisAccount::ethereum).collect::<Vec<_>>();
        let output = StartupOutput {
            accounts: &accounts,
            genesis_accounts: &config.genesis_accounts,
            wallet: &config.wallet,
            chain_id: config.node.chain_id(),
            genesis_number: config.node.genesis_block_number(),
            genesis_timestamp: config.node.genesis_timestamp(),
            block_gas_limit: config.node.block_gas_limit(),
            base_fee_per_gas: config.node.base_fee_per_gas(),
            actor_bundle_root: "bafy-test".to_owned(),
            prepared_state_root: Some("bafy-prepared".to_owned()),
            current_state_root: "bafy-current".to_owned(),
            mining: config.mining,
            rpc_url: "http://127.0.0.1:8545",
        };
        let path =
            std::env::temp_dir().join(format!("fvm-anvil-config-{}.json", std::process::id()));
        write_config_file(&path, &output).unwrap();
        let value: serde_json::Value = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(value["available_accounts"].as_array().unwrap().len(), 1);
        assert_eq!(value["private_keys"].as_array().unwrap().len(), 1);
        assert_eq!(value["chain_id"], DEFAULT_CHAIN_ID);
        assert_eq!(value["base_fee"], DEFAULT_BASE_FEE_PER_GAS.to_string());
        assert_eq!(value["filecoin"]["network_version"], 28);
        assert_eq!(value["filecoin"]["actor_bundle_root"], "bafy-test");
        assert_eq!(value["filecoin"]["prepared_state_root"], "bafy-prepared");
        assert_eq!(value["filecoin"]["current_state_root"], "bafy-current");
    }
}
