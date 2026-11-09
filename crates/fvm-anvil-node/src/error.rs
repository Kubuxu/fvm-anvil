use std::io;
use std::path::PathBuf;

use alloy_primitives::{Address, B256, Bytes, U256};
use cid::Cid;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("chain ID {0} is invalid; chain ID zero is reserved by ref-FVM")]
    InvalidChainId(u64),
    #[error("genesis block number {0} leaves no room for the next FVM epoch")]
    InvalidGenesisBlockNumber(u64),
    #[error("genesis timestamp {0} leaves no room for the next block timestamp")]
    InvalidGenesisTimestamp(u64),
    #[error("block gas limit must be greater than zero, found {0}")]
    InvalidBlockGasLimit(u64),
    #[error("the local actor bundle path is empty")]
    EmptyBundlePath,
    #[error("the prepared-state path is empty")]
    EmptyPreparedStatePath,
    #[error("the pinned official actor bundle root is invalid: {0}")]
    InvalidOfficialBundleRoot(String),
    #[error("the pinned official actor bundle URL is invalid: {0}")]
    InvalidOfficialBundleUrl(String),
}

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("failed to open actor bundle at {path}: {source}")]
    Open { path: PathBuf, source: io::Error },
    #[error("failed to download actor bundle from {url}: {detail}")]
    Download { url: String, detail: String },
    #[error("failed to decode actor bundle CAR: {0}")]
    Car(String),
    #[error("actor bundle CAR must contain exactly one root, found {0}")]
    RootCount(usize),
    #[error("actor bundle root mismatch: expected {expected}, found {actual}")]
    RootMismatch { expected: Box<Cid>, actual: Box<Cid> },
    #[error("actor bundle root {0} does not contain manifest information")]
    MissingManifestInformation(Box<Cid>),
    #[error("failed to decode manifest information at {root}: {detail}")]
    InvalidManifestInformation { root: Box<Cid>, detail: String },
    #[error("unsupported builtin manifest format {0}; expected format 1 for actor bundle v18")]
    UnsupportedManifestFormat(u32),
    #[error("failed to load builtin actor manifest: {0}")]
    Manifest(String),
    #[error("builtin actor manifest is missing required actor {0}")]
    MissingActor(&'static str),
    #[error("builtin actor manifest references missing code block {code} for actor {name}")]
    MissingActorCode { name: String, code: Box<Cid> },
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Bundle(#[from] BundleError),
    #[error(transparent)]
    Bridge(#[from] fvm_anvil_eth::BridgeError),
    #[error("genesis initialization failed: {0}")]
    Genesis(String),
    #[error("failed to load prepared state from {path}: {detail}")]
    PreparedState { path: PathBuf, detail: String },
    #[error("FVM runtime initialization failed: {0}")]
    Runtime(String),
    #[error("FVM execution failed: {0}")]
    Execution(String),
    #[error("transaction {0} is already known")]
    DuplicateTransaction(B256),
    #[error("transaction nonce {actual} is stale; expected {expected}")]
    StaleNonce { expected: u64, actual: u64 },
    #[error("transaction nonce {actual} is in the future; expected {expected}")]
    FutureNonce { expected: u64, actual: u64 },
    #[error(
        "replacement transaction underpriced; both fee cap and premium must increase by at least 10%"
    )]
    ReplacementUnderpriced,
    #[error("transaction reservation arithmetic overflow")]
    ReservationOverflow,
    #[error("native tipset is unavailable: {0}")]
    UnknownTipset(String),
    #[error("randomness epoch {epoch} is outside available history {minimum}..={maximum}")]
    RandomnessEpoch { epoch: i64, minimum: i64, maximum: i64 },
    #[error("transaction fee cap {fee_cap} is below the current base fee {base_fee}")]
    FeeCapTooLow { fee_cap: u128, base_fee: u128 },
    #[error("transaction priority fee {priority_fee} exceeds its fee cap {fee_cap}")]
    PriorityFeeAboveCap { priority_fee: u128, fee_cap: u128 },
    #[error("transaction gas limit {actual} is invalid; maximum is {maximum}")]
    InvalidGasLimit { actual: u64, maximum: u64 },
    #[error("serialized Filecoin signed message is {actual} bytes; maximum is {maximum}")]
    TransactionTooLarge { actual: usize, maximum: usize },
    #[error("pending transactions reserve {actual} gas, exceeding the block maximum {maximum}")]
    PendingBlockGasLimit { actual: u64, maximum: u64 },
    #[error("account {address} has {balance} attoFIL but the transaction requires {required}")]
    InsufficientFunds { address: Address, balance: U256, required: U256 },
    #[error("canonical block number {0} does not exist")]
    UnknownBlockNumber(u64),
    #[error("canonical block hash {0} does not exist")]
    UnknownBlockHash(B256),
    #[error("block selector {0} is not supported for this operation")]
    UnsupportedSelector(&'static str),
    #[error("a log filter cannot combine blockHash with fromBlock or toBlock")]
    ConflictingLogFilterBlocks,
    #[error("invalid log block range: from block {from} is after to block {to}")]
    InvalidLogBlockRange { from: u64, to: u64 },
    #[error("EVM actor {address} has malformed state {state}: {detail}")]
    MalformedEvmState { address: Address, state: Box<Cid>, detail: String },
    #[error("EVM actor {address} has malformed contract storage {root}: {detail}")]
    MalformedEvmStorage { address: Address, root: Box<Cid>, detail: String },
    #[error("ERC-20 token {token} is not a live EVM actor")]
    Erc20TokenNotEvm { token: Address },
    #[error("ERC-20 {field} call on token {token} returned {length} bytes; expected 32")]
    MalformedErc20Return { token: Address, field: &'static str, length: usize },
    #[error("unable to find ERC-20 {field} storage slot for token {token}")]
    Erc20StorageSlotNotFound { token: Address, field: &'static str },
    #[error("mine block count must be at least one, found {0}")]
    InvalidMineCount(u64),
    #[error("mining {field} arithmetic overflow")]
    MineArithmeticOverflow { field: &'static str },
    #[error("cannot reserve memory for {count} mined blocks: {detail}")]
    MineCapacity { count: u64, detail: String },
    #[error("first mined timestamp {actual} is before the next canonical timestamp {minimum}")]
    InvalidMineTimestamp { minimum: u64, actual: u64 },
    #[error("FEVM execution reverted with exit code {exit_code}: {detail}")]
    CallFailed { exit_code: u32, detail: String, data: Bytes },
    #[error("actor delegated address cannot be projected to Ethereum: {0}")]
    InvalidDelegatedAddress(String),
    #[error("FVM token amount cannot be represented as an Ethereum quantity: {0}")]
    InvalidTokenAmount(String),
    #[error("snapshot ID {0} is invalid")]
    InvalidSnapshotId(U256),
    #[error("failed to spawn the FVM worker thread: {0}")]
    WorkerSpawn(#[source] io::Error),
    #[error("the FVM worker stopped before reporting readiness")]
    WorkerStartup,
    #[error("the FVM worker is unavailable")]
    WorkerUnavailable,
}
