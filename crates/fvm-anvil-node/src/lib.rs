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
mod types;

pub use config::{
    ACTOR_BUNDLE_VERSION, ActorBundle, DEFAULT_ACTOR_BUNDLE_ROOT, DEFAULT_ACTOR_BUNDLE_URL,
    DEFAULT_CHAIN_ID, KNOWN_V18_ACTOR_BUNDLE_ROOTS, NETWORK_VERSION, NodeConfig,
};
pub use error::{BundleError, ConfigError, NodeError};
pub use service::{NodeHandle, NodeService};
pub use types::{
    BASE_FEE_PER_GAS, BLOCK_GAS_LIMIT, Block, BlockSelector, CallRequest, FilecoinGasBreakdown,
    Log, LogFilter, Transaction, TransactionReceipt,
};
