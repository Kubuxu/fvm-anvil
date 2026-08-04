//! Stateful ref-FVM execution for fvm-anvil.
//!
//! This crate owns the dedicated worker and is the only production crate allowed to own mutable
//! FVM or synthetic chain state.

mod bundle;
mod config;
mod error;
mod externs;
mod genesis;
mod runtime;
mod service;
mod storage;
mod types;

pub use config::{
    ACTOR_BUNDLE_VERSION, ActorBundle, DEFAULT_ACTOR_BUNDLE_ROOT, DEFAULT_ACTOR_BUNDLE_URL,
    DEFAULT_BASE_FEE_PER_GAS, DEFAULT_BLOCK_GAS_LIMIT, DEFAULT_CHAIN_ID,
    DEFAULT_GENESIS_BLOCK_NUMBER, DEFAULT_GENESIS_TIMESTAMP, DEFAULT_PREFUNDED_BALANCE_FIL,
    DEFAULT_PREFUNDED_ETH_ADDRESS, GenesisAccount, NETWORK_VERSION, NodeConfig,
};
pub use error::{BundleError, ConfigError, NodeError};
pub use service::{NodeHandle, NodeService, NodeStartupInfo};
pub use types::{
    BASE_FEE_PER_GAS, BLOCK_GAS_LIMIT, Block, BlockSelector, CallRequest, FilecoinCallTrace,
    FilecoinGasBreakdown, FilecoinGasCharge, FilecoinGasTrace, FilecoinStateRoots, Log, LogFilter,
    Transaction, TransactionReceipt,
};
