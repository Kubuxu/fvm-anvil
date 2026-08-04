use alloy_primitives::{Address, B256, Bytes, U256};

use cid::Cid;
/// Fixed genesis and synthetic-block base fee, in attoFIL per gas unit.
pub const BASE_FEE_PER_GAS: u128 = 100;
/// Maximum sum of Filecoin gas limits admitted to one synthetic block.
pub const BLOCK_GAS_LIMIT: u64 = 10_000_000_000;

/// Identifies an existing canonical state root.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BlockSelector {
    Latest,
    Pending,
    Earliest,
    Number(u64),
    Hash(B256),
}

/// Ethereum call inputs understood by FEVM.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CallRequest {
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub value: U256,
    pub input: Bytes,
    pub gas_limit: Option<u64>,
    pub max_fee_per_gas: Option<u128>,
    pub max_priority_fee_per_gas: Option<u128>,
    pub nonce: Option<u64>,
}

/// An Ethereum transaction admitted by the node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    pub hash: B256,
    pub chain_id: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub nonce: u64,
    pub value: U256,
    pub input: Bytes,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub raw: Bytes,
    pub effective_gas_price: Option<U256>,
    pub block_hash: Option<B256>,
    pub block_number: Option<u64>,
    pub transaction_index: Option<u64>,
}

/// One Ethereum-compatible FEVM event projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
    pub block_hash: B256,
    pub block_number: u64,
    pub transaction_hash: B256,
    pub transaction_index: u64,
    pub log_index: u64,
    pub removed: bool,
}

/// Receipt for one committed Ethereum transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionReceipt {
    pub transaction_hash: B256,
    pub transaction_index: u64,
    pub block_hash: B256,
    pub block_number: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub contract_address: Option<Address>,
    pub cumulative_gas_used: u64,
    pub gas_used: u64,
    pub effective_gas_price: U256,
    pub status: bool,
    pub exit_code: u32,
    pub output: Bytes,
    pub logs: Vec<Log>,
}

/// A synthetic canonical Ethereum block backed by one FVM state root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub hash: B256,
    pub parent_hash: B256,
    pub number: u64,
    pub state_root: B256,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub base_fee_per_gas: u128,
    pub transactions: Vec<Transaction>,
    pub logs: Vec<Log>,
}

/// Canonical Ethereum log selection.
///
/// An empty address list is a wildcard. Each populated topic position is an OR-list; `None` is a
/// positional wildcard. `block_hash` is mutually exclusive with both range endpoints.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LogFilter {
    pub block_hash: Option<B256>,
    pub from_block: Option<BlockSelector>,
    pub to_block: Option<BlockSelector>,
    pub addresses: Vec<Address>,
    pub topics: [Option<Vec<B256>>; 4],
}

/// Prepared import root and the currently adopted writable execution root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilecoinStateRoots {
    pub prepared: Option<Cid>,
    pub current: Cid,
}

/// The exact ref-FVM gas accounting fields for a committed transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilecoinGasBreakdown {
    pub base_fee_burn: U256,
    pub over_estimation_burn: U256,
    pub miner_penalty: U256,
    pub miner_tip: U256,
    pub refund: U256,
    pub gas_refund: u64,
    pub gas_burned: u64,
}

/// Exact gas charged at one ref-FVM charging point.
///
/// The values use ref-FVM's internal milligas unit so aggregating repeated charges does not lose
/// precision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilecoinGasCharge {
    pub name: String,
    pub compute_milligas: u64,
    pub other_milligas: u64,
    pub total_milligas: u64,
}

/// One actor invocation and its directly attributable gas charges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilecoinCallTrace {
    pub from_actor_id: u64,
    pub to: String,
    pub method: u64,
    pub gas_limit: u64,
    pub read_only: bool,
    pub invoked_actor_id: Option<u64>,
    pub exit_code: Option<u32>,
    pub syscall_error: Option<String>,
    pub gas_charges: Vec<FilecoinGasCharge>,
    pub subcalls: Vec<FilecoinCallTrace>,
}

/// Compact ref-FVM execution and gas trace for one committed transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilecoinGasTrace {
    pub gas_used: u64,
    pub exit_code: u32,
    pub failure_info: Option<String>,
    pub gas_charges: Vec<FilecoinGasCharge>,
    pub calls: Vec<FilecoinCallTrace>,
}
