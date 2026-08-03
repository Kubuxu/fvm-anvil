use std::path::{Path, PathBuf};
use std::str::FromStr;

use alloy_primitives::{Address as EthAddress, U256, address};
use cid::Cid;
use fvm_shared::version::NetworkVersion;
use url::Url;

use crate::error::ConfigError;

pub const DEFAULT_CHAIN_ID: u64 = 31_337;
pub const NETWORK_VERSION: NetworkVersion = NetworkVersion::V28;
pub const ACTOR_BUNDLE_VERSION: u32 = 18;
pub const DEFAULT_ACTOR_BUNDLE_ROOT: &str =
    "bafy2bzacedxsqapxwj5znwy4leem5gsuenhhfyqcntews3yis5iek67thy6lc";
pub const DEFAULT_ACTOR_BUNDLE_URL: &str = "https://github.com/filecoin-project/builtin-actors/releases/download/v18.0.0/builtin-actors-mainnet.car";
pub const DEFAULT_GENESIS_BLOCK_NUMBER: u64 = 0;
pub const DEFAULT_GENESIS_TIMESTAMP: u64 = 0;
pub const DEFAULT_BLOCK_GAS_LIMIT: u64 = 10_000_000_000;
pub const DEFAULT_BASE_FEE_PER_GAS: u128 = 100;
pub const DEFAULT_PREFUNDED_BALANCE_FIL: u64 = 10_000;
pub const DEFAULT_PREFUNDED_ETH_ADDRESS: EthAddress =
    address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

/// Selects the builtin-actors v18 CAR used to create genesis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActorBundle {
    /// Downloads the pinned official mainnet v18 CAR and verifies its root.
    Official,
    /// Reads a checked-in, known v18 release CAR from disk and verifies its inferred root.
    Local { path: PathBuf },
}

impl ActorBundle {
    pub fn local(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let bundle = Self::Local { path: path.into() };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn actor_version(&self) -> u32 {
        ACTOR_BUNDLE_VERSION
    }

    pub(crate) fn expected_root(&self) -> Result<Option<Cid>, ConfigError> {
        match self {
            Self::Official => Cid::from_str(DEFAULT_ACTOR_BUNDLE_ROOT)
                .map(Some)
                .map_err(|error| ConfigError::InvalidOfficialBundleRoot(error.to_string())),
            Self::Local { .. } => Ok(None),
        }
    }

    pub fn source_url(&self) -> Result<Option<Url>, ConfigError> {
        match self {
            Self::Official => Url::parse(DEFAULT_ACTOR_BUNDLE_URL)
                .map(Some)
                .map_err(|error| ConfigError::InvalidOfficialBundleUrl(error.to_string())),
            Self::Local { .. } => Ok(None),
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Official => None,
            Self::Local { path } => Some(path),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Official => {
                self.expected_root()?;
                self.source_url()?;
            }
            Self::Local { path } if path.as_os_str().is_empty() => {
                return Err(ConfigError::EmptyBundlePath);
            }
            Self::Local { .. } => {}
        }
        Ok(())
    }
}

/// One delegated placeholder account created at genesis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenesisAccount {
    ethereum: EthAddress,
    balance: U256,
}

impl GenesisAccount {
    pub const fn new(ethereum: EthAddress, balance: U256) -> Self {
        Self { ethereum, balance }
    }

    pub const fn ethereum(&self) -> EthAddress {
        self.ethereum
    }

    pub const fn balance(&self) -> U256 {
        self.balance
    }
}

/// Immutable genesis-time configuration for a node worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeConfig {
    chain_id: u64,
    actor_bundle: ActorBundle,
    actor_bundle_cache: Option<PathBuf>,
    genesis_accounts: Vec<GenesisAccount>,
    genesis_block_number: u64,
    genesis_timestamp: u64,
    block_gas_limit: u64,
    base_fee_per_gas: u128,
    automine: bool,
}

impl NodeConfig {
    pub fn new(chain_id: u64, actor_bundle: ActorBundle) -> Result<Self, ConfigError> {
        if chain_id == 0 {
            return Err(ConfigError::InvalidChainId(chain_id));
        }
        actor_bundle.validate()?;
        Ok(Self {
            chain_id,
            actor_bundle,
            actor_bundle_cache: None,
            genesis_accounts: vec![GenesisAccount::new(
                DEFAULT_PREFUNDED_ETH_ADDRESS,
                U256::from(DEFAULT_PREFUNDED_BALANCE_FIL) * U256::from(10_u64.pow(18)),
            )],
            genesis_block_number: DEFAULT_GENESIS_BLOCK_NUMBER,
            genesis_timestamp: DEFAULT_GENESIS_TIMESTAMP,
            block_gas_limit: DEFAULT_BLOCK_GAS_LIMIT,
            base_fee_per_gas: DEFAULT_BASE_FEE_PER_GAS,
            automine: true,
        })
    }

    pub fn with_genesis_accounts(mut self, accounts: Vec<GenesisAccount>) -> Self {
        self.genesis_accounts = accounts;
        self
    }
    /// Caches the pinned official bundle at `path`. Local bundles are never copied.
    pub fn with_actor_bundle_cache(mut self, path: Option<PathBuf>) -> Self {
        self.actor_bundle_cache = path;
        self
    }

    pub fn with_genesis_block(mut self, number: u64, timestamp: u64) -> Result<Self, ConfigError> {
        if number >= i64::MAX as u64 {
            return Err(ConfigError::InvalidGenesisBlockNumber(number));
        }
        if timestamp == u64::MAX {
            return Err(ConfigError::InvalidGenesisTimestamp(timestamp));
        }
        self.genesis_block_number = number;
        self.genesis_timestamp = timestamp;
        Ok(self)
    }

    pub fn with_block_gas_limit(mut self, gas_limit: u64) -> Result<Self, ConfigError> {
        if gas_limit == 0 {
            return Err(ConfigError::InvalidBlockGasLimit(gas_limit));
        }
        self.block_gas_limit = gas_limit;
        Ok(self)
    }

    pub const fn with_base_fee_per_gas(mut self, base_fee_per_gas: u128) -> Self {
        self.base_fee_per_gas = base_fee_per_gas;
        self
    }

    pub const fn with_automine(mut self, automine: bool) -> Self {
        self.automine = automine;
        self
    }

    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub const fn network_version(&self) -> NetworkVersion {
        NETWORK_VERSION
    }

    pub const fn actor_bundle(&self) -> &ActorBundle {
        &self.actor_bundle
    }
    pub fn actor_bundle_cache(&self) -> Option<&Path> {
        self.actor_bundle_cache.as_deref()
    }

    pub fn genesis_accounts(&self) -> &[GenesisAccount] {
        &self.genesis_accounts
    }

    pub const fn genesis_block_number(&self) -> u64 {
        self.genesis_block_number
    }

    pub const fn genesis_timestamp(&self) -> u64 {
        self.genesis_timestamp
    }

    pub const fn block_gas_limit(&self) -> u64 {
        self.block_gas_limit
    }

    pub const fn base_fee_per_gas(&self) -> u128 {
        self.base_fee_per_gas
    }

    pub const fn automine(&self) -> bool {
        self.automine
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self::new(DEFAULT_CHAIN_ID, ActorBundle::Official)
            .expect("the pinned default node configuration must be valid")
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use fvm_shared::version::NetworkVersion;

    use super::{
        ACTOR_BUNDLE_VERSION, ActorBundle, DEFAULT_CHAIN_ID, GenesisAccount, NETWORK_VERSION,
        NodeConfig,
    };
    use crate::error::ConfigError;

    #[test]
    fn defaults_identify_nv28_and_actor_bundle_v18() {
        let config = NodeConfig::default();

        assert_eq!(config.chain_id(), DEFAULT_CHAIN_ID);
        assert_eq!(config.network_version(), NetworkVersion::V28);
        assert_eq!(config.network_version(), NETWORK_VERSION);
        assert_eq!(config.actor_bundle().actor_version(), ACTOR_BUNDLE_VERSION);
        assert_eq!(config.actor_bundle(), &ActorBundle::Official);
    }

    #[test]
    fn zero_chain_id_is_rejected() {
        assert!(matches!(
            NodeConfig::new(0, ActorBundle::Official),
            Err(ConfigError::InvalidChainId(0))
        ));
    }
    #[test]
    fn configured_genesis_environment_is_preserved() {
        let account = GenesisAccount::new(Address::with_last_byte(7), U256::from(99));
        let config = NodeConfig::default()
            .with_genesis_accounts(vec![account.clone()])
            .with_genesis_block(41, 42)
            .unwrap()
            .with_block_gas_limit(43)
            .unwrap()
            .with_base_fee_per_gas(44)
            .with_automine(false);

        assert_eq!(config.genesis_accounts(), &[account]);
        assert_eq!(config.genesis_block_number(), 41);
        assert_eq!(config.genesis_timestamp(), 42);
        assert_eq!(config.block_gas_limit(), 43);
        assert_eq!(config.base_fee_per_gas(), 44);
        assert!(!config.automine());
    }

    #[test]
    fn invalid_genesis_successors_and_zero_gas_limit_are_rejected() {
        assert!(matches!(
            NodeConfig::default().with_genesis_block(i64::MAX as u64, 0),
            Err(ConfigError::InvalidGenesisBlockNumber(_))
        ));
        assert!(matches!(
            NodeConfig::default().with_genesis_block(0, u64::MAX),
            Err(ConfigError::InvalidGenesisTimestamp(_))
        ));
        assert!(matches!(
            NodeConfig::default().with_block_gas_limit(0),
            Err(ConfigError::InvalidBlockGasLimit(0))
        ));
    }

    #[test]
    fn local_bundle_requires_only_a_nonempty_path() {
        assert_eq!(
            ActorBundle::local("actors.car").unwrap(),
            ActorBundle::Local { path: "actors.car".into() }
        );
        assert!(matches!(ActorBundle::local(""), Err(ConfigError::EmptyBundlePath)));
    }
}
