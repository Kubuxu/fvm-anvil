use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;

use alloy_primitives::{Address as EthAddress, B256, Bytes, Keccak256, U256};
use cid::Cid;
use fvm::call_manager::DefaultCallManager;
use fvm::engine::EnginePool;
use fvm::executor::{ApplyKind, ApplyRet, DefaultExecutor, Executor};
use fvm::kernel::filecoin::DefaultFilecoinKernel;
use fvm::machine::{DefaultMachine, NetworkConfig};
use fvm::state_tree::{ActorState, StateTree};
use fvm_anvil_eth::{
    BridgedTransaction, ExecutionKind, UnsignedCall, build_unsigned_message,
    decode_execution_return, decode_revert_data, decode_signed, delegated_chain_length,
    project_event,
};
use fvm_ipld_blockstore::{Blockstore, MemoryBlockstore};
use fvm_ipld_encoding::CborStore;
use fvm_ipld_encoding::tuple::{Deserialize_tuple, serde_tuple};
use fvm_ipld_kamt::{AsHashedKey, Config as KamtConfig, HashedKey, Kamt};
use fvm_shared::address::{Address as FvmAddress, Payload};
use fvm_shared::bigint::{BigInt, Sign};
use fvm_shared::chainid::ChainID;
use fvm_shared::econ::TokenAmount;
use multihash_codetable::Code;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::bundle::{BundleArtifacts, load_bundle};
use crate::config::NodeConfig;
use crate::error::NodeError;
use crate::externs::DeterministicExterns;
use crate::genesis::{PrefundedAccount, create_genesis};
use crate::types::{
    BASE_FEE_PER_GAS, BLOCK_GAS_LIMIT, Block, BlockSelector, CallRequest, FilecoinGasBreakdown,
    Log, LogFilter, Transaction, TransactionReceipt,
};

type RuntimeMachine = DefaultMachine<Rc<MemoryBlockstore>, DeterministicExterns>;
type RuntimeKernel = DefaultFilecoinKernel<DefaultCallManager<RuntimeMachine>>;
type RuntimeExecutor = DefaultExecutor<RuntimeKernel>;

const GENESIS_EPOCH: i64 = 0;
const GENESIS_TIMESTAMP: u64 = 0;
const BLOCK_TIME_SECONDS: u64 = 1;
const EAM_ACTOR_ID: u64 = 10;

#[derive(Clone)]
struct PendingTransaction {
    bridged: BridgedTransaction,
    transaction: Transaction,
}

#[derive(Clone)]
struct CanonicalBlock {
    block: Block,
    root: Cid,
}

#[derive(Clone)]
struct SnapshotState {
    current_root: Cid,
    blocks: Vec<CanonicalBlock>,
    block_numbers_by_hash: HashMap<B256, u64>,
    transactions: HashMap<B256, Transaction>,
    receipts: HashMap<B256, TransactionReceipt>,
    gas_breakdowns: HashMap<B256, FilecoinGasBreakdown>,
    next_epoch: i64,
    next_timestamp: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MiningSchedule {
    count: u64,
    first_timestamp: u64,
    interval: u64,
    next_timestamp: u64,
}

impl MiningSchedule {
    fn new(
        head_timestamp: u64,
        count: u64,
        interval: Option<u64>,
        timestamp: Option<u64>,
    ) -> Result<Self, NodeError> {
        if count == 0 {
            return Err(NodeError::InvalidMineCount(count));
        }

        let interval = interval.unwrap_or(BLOCK_TIME_SECONDS);
        let first_timestamp = match timestamp {
            Some(timestamp) if timestamp < head_timestamp => {
                return Err(NodeError::InvalidMineTimestamp {
                    minimum: head_timestamp,
                    actual: timestamp,
                });
            }
            Some(timestamp) => timestamp,
            None => head_timestamp
                .checked_add(interval)
                .ok_or(NodeError::MineArithmeticOverflow { field: "timestamp" })?,
        };
        let final_offset = count
            .checked_sub(1)
            .ok_or(NodeError::MineArithmeticOverflow { field: "block count" })?;
        let timestamp_span = interval
            .checked_mul(final_offset)
            .ok_or(NodeError::MineArithmeticOverflow { field: "timestamp interval" })?;
        let final_timestamp = first_timestamp
            .checked_add(timestamp_span)
            .ok_or(NodeError::MineArithmeticOverflow { field: "timestamp" })?;
        let next_timestamp = final_timestamp
            .checked_add(1)
            .ok_or(NodeError::MineArithmeticOverflow { field: "timestamp" })?;

        Ok(Self { count, first_timestamp, interval, next_timestamp })
    }

    fn timestamp_at(self, offset: u64) -> u64 {
        assert!(offset < self.count, "mined block offset must be preflighted");
        self.interval
            .checked_mul(offset)
            .and_then(|span| self.first_timestamp.checked_add(span))
            .expect("mining timestamp arithmetic must be preflighted")
    }
}

// The v18 EVM actor stores both keys and values as minimally encoded, big-endian byte strings.
// This adapter duplicates only that attributed wire representation (Protocol Labs, MIT OR
// Apache-2.0) and deliberately does not depend on an individual actor crate.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct EvmWordV18(U256);

impl Serialize for EvmWordV18 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.0.to_be_bytes::<32>();
        let first_nonzero = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len());
        serializer.serialize_bytes(&bytes[first_nonzero..])
    }
}

impl<'de> Deserialize<'de> for EvmWordV18 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct WordVisitor;

        impl serde::de::Visitor<'_> for WordVisitor {
            type Value = EvmWordV18;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an EVM word encoded in at most 32 big-endian bytes")
            }

            fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if bytes.len() > 32 {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                Ok(EvmWordV18(U256::from_be_slice(bytes)))
            }
        }

        deserializer.deserialize_bytes(WordVisitor)
    }
}

/// v18's contract-state key routing uses the full 32-byte big-endian word unchanged.
struct StateHashAlgorithmV18;

impl AsHashedKey<EvmWordV18, 32> for StateHashAlgorithmV18 {
    fn as_hashed_key(key: &EvmWordV18) -> Cow<'_, HashedKey<32>> {
        Cow::Owned(key.0.to_be_bytes())
    }
}

type EvmStateKamtV18<BS> = Kamt<BS, EvmWordV18, EvmWordV18, StateHashAlgorithmV18>;

const EVM_STATE_KAMT_CONFIG_V18: KamtConfig =
    KamtConfig { min_data_depth: 0, bit_width: 5, max_array_width: 1 };

/// builtin-actors v18 EVM state tuple, duplicated locally to avoid an actor-crate dependency.
/// The wire layout is Copyright Protocol Labs, MIT OR Apache-2.0.
#[allow(dead_code)]
#[derive(Deserialize_tuple)]
struct EvmStateV18 {
    bytecode: Cid,
    bytecode_hash: BytecodeHashV18,
    contract_state: Cid,
    transient_data: Option<TransientDataV18>,
    nonce: u64,
    tombstone: Option<TombstoneV18>,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct BytecodeHashV18(
    // This private field remains in place because it is the second v18 wire-schema tuple member.
    #[allow(dead_code)]
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    [u8; 32],
);

#[allow(dead_code)]
#[derive(Deserialize_tuple)]
struct TransientDataV18 {
    transient_data_state: Cid,
    transient_data_lifespan: TransientDataLifespanV18,
}

#[allow(dead_code)]
#[derive(Deserialize_tuple)]
struct TransientDataLifespanV18 {
    origin: u64,
    nonce: u64,
}

#[allow(dead_code)]
#[derive(Deserialize_tuple)]
struct TombstoneV18 {
    origin: u64,
    nonce: u64,
}

pub(crate) struct NodeRuntime {
    store: Rc<MemoryBlockstore>,
    engine_pool: EnginePool,
    network: NetworkConfig,
    current_root: Cid,
    bundle: BundleArtifacts,
    prefunded_accounts: Vec<PrefundedAccount>,
    blocks: Vec<CanonicalBlock>,
    block_numbers_by_hash: HashMap<B256, u64>,
    transactions: HashMap<B256, Transaction>,
    receipts: HashMap<B256, TransactionReceipt>,
    gas_breakdowns: HashMap<B256, FilecoinGasBreakdown>,
    pool: VecDeque<PendingTransaction>,
    automine: bool,
    next_epoch: i64,
    next_timestamp: u64,
    snapshots: HashMap<U256, SnapshotState>,
    next_snapshot_id: u64,
}

impl NodeRuntime {
    pub(crate) fn new(config: NodeConfig) -> Result<Self, NodeError> {
        let store = Rc::new(MemoryBlockstore::default());
        let bundle = load_bundle(config.actor_bundle(), store.as_ref())?;
        let genesis = create_genesis(&store, &bundle)?;

        let mut network = NetworkConfig::new(config.network_version());
        network.chain_id(ChainID::from(config.chain_id())).override_actors(bundle.root);
        let engine_pool = EnginePool::new((&network).into())
            .map_err(|error| NodeError::Runtime(error.to_string()))?;
        engine_pool
            .acquire()
            .preload_all(store.as_ref(), bundle.all_codes())
            .map_err(|error| NodeError::Runtime(error.to_string()))?;

        let genesis_hash = synthetic_genesis_hash(config.chain_id(), &genesis.root);
        let genesis_block = Block {
            hash: genesis_hash,
            parent_hash: B256::ZERO,
            number: 0,
            state_root: ethereum_state_root(&genesis.root),
            timestamp: GENESIS_TIMESTAMP,
            gas_limit: BLOCK_GAS_LIMIT,
            gas_used: 0,
            base_fee_per_gas: BASE_FEE_PER_GAS,
            transactions: Vec::new(),
            logs: Vec::new(),
        };
        let mut block_numbers_by_hash = HashMap::new();
        block_numbers_by_hash.insert(genesis_hash, 0);

        let runtime = Self {
            store,
            engine_pool,
            network,
            current_root: genesis.root,
            bundle,
            prefunded_accounts: genesis.accounts,
            blocks: vec![CanonicalBlock { block: genesis_block, root: genesis.root }],
            block_numbers_by_hash,
            transactions: HashMap::new(),
            receipts: HashMap::new(),
            gas_breakdowns: HashMap::new(),
            pool: VecDeque::new(),
            automine: true,
            next_epoch: GENESIS_EPOCH + 1,
            next_timestamp: GENESIS_TIMESTAMP + BLOCK_TIME_SECONDS,
            snapshots: HashMap::new(),
            next_snapshot_id: 1,
        };
        drop(runtime.new_executor_at(runtime.current_root, GENESIS_EPOCH, GENESIS_TIMESTAMP)?);
        Ok(runtime)
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.network.chain_id.into()
    }

    pub(crate) fn accounts(&self) -> Vec<EthAddress> {
        self.prefunded_accounts.iter().map(|account| account.ethereum).collect()
    }

    pub(crate) fn submit_raw_transaction(&mut self, raw: Bytes) -> Result<B256, NodeError> {
        let bridged = decode_signed(raw.as_ref(), self.chain_id())?;
        let hash = bridged.tx_hash();
        self.validate_pool_admission(&bridged)?;
        let transaction = public_transaction(&bridged);
        self.pool.push_back(PendingTransaction { bridged, transaction });

        if self.automine {
            self.commit_blocks(1, None, None)?;
        }
        Ok(hash)
    }

    pub(crate) fn call(
        &self,
        request: CallRequest,
        selector: BlockSelector,
    ) -> Result<Bytes, NodeError> {
        let simulation = self.simulate(request, selector)?;
        let exit_code = simulation.apply.msg_receipt.exit_code;
        if !exit_code.is_success() {
            return Err(call_failure(&simulation.apply)?);
        }
        let kind = execution_kind(simulation.to);
        Ok(decode_execution_return(kind, &simulation.apply.msg_receipt.return_data)?.output)
    }

    pub(crate) fn estimate_gas(
        &self,
        request: CallRequest,
        selector: BlockSelector,
    ) -> Result<u64, NodeError> {
        let simulation = self.simulate(request, selector)?;
        if !simulation.apply.msg_receipt.exit_code.is_success() {
            return Err(call_failure(&simulation.apply)?);
        }
        Ok(simulation.apply.msg_receipt.gas_used)
    }

    pub(crate) fn block_number(&self) -> u64 {
        self.blocks.last().map_or(0, |record| record.block.number)
    }

    pub(crate) fn transaction_by_hash(&self, hash: B256) -> Option<Transaction> {
        self.transactions.get(&hash).cloned().or_else(|| {
            self.pool
                .iter()
                .find(|pending| pending.transaction.hash == hash)
                .map(|pending| pending.transaction.clone())
        })
    }

    pub(crate) fn transaction_receipt(&self, hash: B256) -> Option<TransactionReceipt> {
        self.receipts.get(&hash).cloned()
    }

    pub(crate) fn block_by_number(
        &self,
        selector: BlockSelector,
    ) -> Result<Option<Block>, NodeError> {
        let number = match selector {
            BlockSelector::Latest => self.block_number(),
            BlockSelector::Earliest => 0,
            BlockSelector::Number(number) => number,
            BlockSelector::Hash(hash) => {
                let Some(number) = self.block_numbers_by_hash.get(&hash).copied() else {
                    return Ok(None);
                };
                number
            }
            BlockSelector::Pending => return Err(NodeError::UnsupportedSelector("pending")),
        };
        Ok(self.blocks.get(number as usize).map(|record| record.block.clone()))
    }

    pub(crate) fn balance(
        &self,
        address: EthAddress,
        selector: BlockSelector,
    ) -> Result<U256, NodeError> {
        let (root, _, _) = self.resolve_state(selector, true)?;
        let actor = self.actor_at(root, address)?;
        match actor {
            Some(actor) => token_amount_to_u256(&actor.balance),
            None => Ok(U256::ZERO),
        }
    }

    pub(crate) fn transaction_count(
        &self,
        address: EthAddress,
        selector: BlockSelector,
    ) -> Result<u64, NodeError> {
        if selector == BlockSelector::Pending {
            return self.next_pool_nonce(address);
        }
        let (root, _, _) = self.resolve_state(selector, false)?;
        Ok(self.actor_at(root, address)?.map_or(0, |actor| actor.sequence))
    }

    pub(crate) fn code(
        &self,
        address: EthAddress,
        selector: BlockSelector,
    ) -> Result<Bytes, NodeError> {
        let (root, _, _) = self.resolve_state(selector, true)?;
        let Some(actor) = self.actor_at(root, address)? else {
            return Ok(Bytes::new());
        };
        if actor.code != self.bundle.code("evm")? {
            return Ok(Bytes::new());
        }
        let state = self.evm_state_v18(address, &actor)?;
        let bytecode = self
            .store
            .get(&state.bytecode)
            .map_err(|error| NodeError::Execution(error.to_string()))?
            .ok_or_else(|| {
                NodeError::Execution(format!("missing EVM bytecode block {}", state.bytecode))
            })?;
        Ok(Bytes::from(bytecode))
    }

    pub(crate) fn storage(
        &self,
        address: EthAddress,
        slot: U256,
        selector: BlockSelector,
    ) -> Result<B256, NodeError> {
        let (root, _, _) = self.resolve_state(selector, true)?;
        let Some(actor) = self.actor_at(root, address)? else {
            return Ok(B256::ZERO);
        };
        if actor.code != self.bundle.code("evm")? {
            return Ok(B256::ZERO);
        }
        let state = self.evm_state_v18(address, &actor)?;
        let storage = EvmStateKamtV18::load_with_config(
            &state.contract_state,
            Rc::clone(&self.store),
            EVM_STATE_KAMT_CONFIG_V18.clone(),
        )
        .map_err(|error| NodeError::MalformedEvmStorage {
            address,
            root: Box::new(state.contract_state),
            detail: error.to_string(),
        })?;
        let value = storage
            .get(&EvmWordV18(slot))
            .map_err(|error| NodeError::MalformedEvmStorage {
                address,
                root: Box::new(state.contract_state),
                detail: error.to_string(),
            })?
            .map_or(U256::ZERO, |value| value.0);
        Ok(B256::from(value.to_be_bytes::<32>()))
    }

    pub(crate) fn logs(&self, filter: LogFilter) -> Result<Vec<Log>, NodeError> {
        let records = if let Some(hash) = filter.block_hash {
            if filter.from_block.is_some() || filter.to_block.is_some() {
                return Err(NodeError::ConflictingLogFilterBlocks);
            }
            let Some(number) = self.block_numbers_by_hash.get(&hash).copied() else {
                return Ok(Vec::new());
            };
            &self.blocks[number as usize..=number as usize]
        } else {
            let from = self.log_range_endpoint(filter.from_block, self.block_number())?;
            let to = self.log_range_endpoint(filter.to_block, self.block_number())?;
            if from > to {
                return Err(NodeError::InvalidLogBlockRange { from, to });
            }
            if from > self.block_number() {
                return Ok(Vec::new());
            }
            let to = to.min(self.block_number());
            &self.blocks[from as usize..=to as usize]
        };

        Ok(records
            .iter()
            .flat_map(|record| record.block.logs.iter())
            .filter(|log| log_matches_filter(log, &filter))
            .cloned()
            .collect())
    }

    pub(crate) fn mine_blocks(
        &mut self,
        count: u64,
        interval: Option<u64>,
        timestamp: Option<u64>,
    ) -> Result<Vec<Block>, NodeError> {
        self.commit_blocks(count, interval, timestamp)
    }

    pub(crate) fn automine(&self) -> bool {
        self.automine
    }

    pub(crate) fn set_automine(&mut self, enabled: bool) -> Result<(), NodeError> {
        if enabled && !self.automine && !self.pool.is_empty() {
            self.commit_blocks(1, None, None)?;
        }
        self.automine = enabled;
        Ok(())
    }

    pub(crate) fn snapshot(&mut self) -> U256 {
        let id = U256::from(self.next_snapshot_id);
        self.next_snapshot_id = self.next_snapshot_id.saturating_add(1);
        let snapshot = SnapshotState {
            current_root: self.current_root,
            blocks: self.blocks.clone(),
            block_numbers_by_hash: self.block_numbers_by_hash.clone(),
            transactions: self.transactions.clone(),
            receipts: self.receipts.clone(),
            gas_breakdowns: self.gas_breakdowns.clone(),
            next_epoch: self.next_epoch,
            next_timestamp: self.next_timestamp,
        };
        self.snapshots.insert(id, snapshot);
        id
    }

    pub(crate) fn revert(&mut self, id: U256) -> bool {
        let Some(snapshot) = self.snapshots.remove(&id) else {
            return false;
        };
        let current_pool = std::mem::take(&mut self.pool);
        self.current_root = snapshot.current_root;
        self.blocks = snapshot.blocks;
        self.block_numbers_by_hash = snapshot.block_numbers_by_hash;
        self.transactions = snapshot.transactions;
        self.receipts = snapshot.receipts;
        self.gas_breakdowns = snapshot.gas_breakdowns;
        self.next_epoch = snapshot.next_epoch;
        self.next_timestamp = snapshot.next_timestamp;
        for pending in current_pool {
            if self.validate_pool_admission(&pending.bridged).is_ok() {
                self.pool.push_back(pending);
            }
        }
        self.snapshots.retain(|snapshot_id, _| *snapshot_id < id);
        true
    }

    pub(crate) fn filecoin_gas_breakdown(&self, hash: B256) -> Option<FilecoinGasBreakdown> {
        self.gas_breakdowns.get(&hash).cloned()
    }

    fn validate_pool_admission(&self, transaction: &BridgedTransaction) -> Result<(), NodeError> {
        let hash = transaction.tx_hash();
        if self.transactions.contains_key(&hash)
            || self.pool.iter().any(|pending| pending.transaction.hash == hash)
        {
            return Err(NodeError::DuplicateTransaction(hash));
        }
        if transaction.gas_limit() == 0 || transaction.gas_limit() > BLOCK_GAS_LIMIT {
            return Err(NodeError::InvalidGasLimit {
                actual: transaction.gas_limit(),
                maximum: BLOCK_GAS_LIMIT,
            });
        }
        let pending_gas = self
            .pool
            .iter()
            .fold(0_u64, |total, pending| total.saturating_add(pending.bridged.gas_limit()));
        let total_gas = pending_gas.saturating_add(transaction.gas_limit());
        if total_gas > BLOCK_GAS_LIMIT {
            return Err(NodeError::PendingBlockGasLimit {
                actual: total_gas,
                maximum: BLOCK_GAS_LIMIT,
            });
        }
        if transaction.max_priority_fee_per_gas() > transaction.max_fee_per_gas() {
            return Err(NodeError::PriorityFeeAboveCap {
                priority_fee: transaction.max_priority_fee_per_gas(),
                fee_cap: transaction.max_fee_per_gas(),
            });
        }
        if transaction.max_fee_per_gas() < BASE_FEE_PER_GAS {
            return Err(NodeError::FeeCapTooLow {
                fee_cap: transaction.max_fee_per_gas(),
                base_fee: BASE_FEE_PER_GAS,
            });
        }

        let expected_nonce = self.next_pool_nonce(transaction.sender())?;
        if transaction.nonce() < expected_nonce {
            return Err(NodeError::StaleNonce {
                expected: expected_nonce,
                actual: transaction.nonce(),
            });
        }
        if transaction.nonce() > expected_nonce {
            return Err(NodeError::FutureNonce {
                expected: expected_nonce,
                actual: transaction.nonce(),
            });
        }

        let balance = self.balance(transaction.sender(), BlockSelector::Latest)?;
        let reserved = self
            .pool
            .iter()
            .filter(|pending| pending.bridged.sender() == transaction.sender())
            .fold(U256::ZERO, |total, pending| {
                total.saturating_add(maximum_transaction_cost(&pending.bridged))
            });
        let required = reserved.saturating_add(maximum_transaction_cost(transaction));
        if required > balance {
            return Err(NodeError::InsufficientFunds {
                address: transaction.sender(),
                balance,
                required,
            });
        }
        Ok(())
    }

    fn next_pool_nonce(&self, address: EthAddress) -> Result<u64, NodeError> {
        let committed =
            self.actor_at(self.current_root, address)?.map_or(0, |actor| actor.sequence);
        Ok(self
            .pool
            .iter()
            .filter(|pending| pending.bridged.sender() == address)
            .fold(committed, |nonce, _| nonce.saturating_add(1)))
    }

    fn simulate(
        &self,
        request: CallRequest,
        selector: BlockSelector,
    ) -> Result<Simulation, NodeError> {
        let (root, epoch, timestamp) = self.resolve_state(selector, true)?;
        let from = request.from.unwrap_or_default();
        let sender = self.actor_at(root, from)?;
        let nonce =
            request.nonce.unwrap_or_else(|| sender.as_ref().map_or(0, |actor| actor.sequence));
        let to = request.to;
        let call = UnsignedCall {
            from,
            to,
            value: request.value,
            input: request.input,
            gas_limit: request.gas_limit.unwrap_or(BLOCK_GAS_LIMIT),
            nonce,
            max_fee_per_gas: U256::from(request.max_fee_per_gas.unwrap_or(BASE_FEE_PER_GAS)),
            max_priority_fee_per_gas: U256::from(
                request.max_priority_fee_per_gas.unwrap_or_default(),
            ),
        };
        let execution_root = match sender {
            Some(_) => root,
            None => self.temporary_sender_root(root, &call)?,
        };
        let message = build_unsigned_message(&call)?;
        let serialized_length = delegated_chain_length(&message)?;
        let mut executor = self.new_executor_at(execution_root, epoch, timestamp)?;
        let apply = executor
            .execute_message(message, ApplyKind::Explicit, serialized_length)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let _unpublished_root =
            executor.flush().map_err(|error| NodeError::Execution(error.to_string()))?;
        Ok(Simulation { apply, to })
    }

    fn commit_blocks(
        &mut self,
        count: u64,
        interval: Option<u64>,
        timestamp: Option<u64>,
    ) -> Result<Vec<Block>, NodeError> {
        let head_timestamp =
            self.blocks.last().map_or(GENESIS_TIMESTAMP, |record| record.block.timestamp);
        let schedule = MiningSchedule::new(head_timestamp, count, interval, timestamp)?;
        let capacity = usize::try_from(count)
            .map_err(|_| NodeError::MineArithmeticOverflow { field: "block count" })?;
        let count_i64 = i64::try_from(count)
            .map_err(|_| NodeError::MineArithmeticOverflow { field: "epoch" })?;
        let number = self
            .block_number()
            .checked_add(1)
            .ok_or(NodeError::MineArithmeticOverflow { field: "block number" })?;
        self.block_number()
            .checked_add(count)
            .ok_or(NodeError::MineArithmeticOverflow { field: "block number" })?;
        let next_epoch = self
            .next_epoch
            .checked_add(count_i64)
            .ok_or(NodeError::MineArithmeticOverflow { field: "epoch" })?;
        let mut records = Vec::new();
        records
            .try_reserve_exact(capacity)
            .map_err(|error| NodeError::MineCapacity { count, detail: error.to_string() })?;

        let first_timestamp = schedule.first_timestamp;

        let pending: Vec<_> = self.pool.iter().cloned().collect();
        let mut applications = Vec::with_capacity(pending.len());
        let root = if pending.is_empty() {
            self.current_root
        } else {
            let mut executor =
                self.new_executor_at(self.current_root, self.next_epoch, first_timestamp)?;
            for transaction in &pending {
                let apply = executor
                    .execute_message(
                        transaction.bridged.signed_message().message().clone(),
                        ApplyKind::Explicit,
                        transaction.bridged.chain_length(),
                    )
                    .map_err(|error| NodeError::Execution(error.to_string()))?;
                applications.push(apply);
            }
            let root = executor.flush().map_err(|error| NodeError::Execution(error.to_string()))?;
            drop(executor);
            root
        };

        let parent_hash = self.blocks.last().map_or(B256::ZERO, |record| record.block.hash);
        let transaction_hashes: Vec<_> =
            pending.iter().map(|transaction| transaction.transaction.hash).collect();
        let block_hash =
            synthetic_block_hash(parent_hash, number, first_timestamp, &root, &transaction_hashes);

        let mut transactions = Vec::with_capacity(pending.len());
        let mut receipts = Vec::with_capacity(pending.len());
        let mut gas_breakdowns = Vec::with_capacity(pending.len());
        let mut block_logs = Vec::new();
        let mut cumulative_gas_used = 0_u64;
        let mut next_log_index = 0_u64;
        for (index, (pending, apply)) in pending.iter().zip(applications.iter()).enumerate() {
            let index = u64::try_from(index)
                .map_err(|_| NodeError::MineArithmeticOverflow { field: "transaction index" })?;
            let mut transaction = pending.transaction.clone();
            transaction.block_hash = Some(block_hash);
            transaction.block_number = Some(number);
            transaction.transaction_index = Some(index);

            cumulative_gas_used = cumulative_gas_used
                .checked_add(apply.msg_receipt.gas_used)
                .ok_or(NodeError::MineArithmeticOverflow { field: "cumulative gas" })?;
            let execution_return = if apply.msg_receipt.exit_code.is_success() {
                Some(decode_execution_return(
                    execution_kind(pending.bridged.recipient()),
                    &apply.msg_receipt.return_data,
                )?)
            } else {
                None
            };
            let mut logs = Vec::new();
            for event in &apply.events {
                let Some(emitter) = self.emitter_address(root, event.emitter)? else {
                    continue;
                };
                let projected = project_event(emitter, &event.event)?;
                logs.push(Log {
                    address: projected.address,
                    topics: projected.topics,
                    data: projected.data,
                    block_hash,
                    block_number: number,
                    transaction_hash: transaction.hash,
                    transaction_index: index,
                    log_index: next_log_index,
                    removed: false,
                });
                next_log_index = next_log_index
                    .checked_add(1)
                    .ok_or(NodeError::MineArithmeticOverflow { field: "log index" })?;
            }
            block_logs.extend(logs.iter().cloned());

            let gas = filecoin_gas_breakdown(apply)?;
            let effective_gas_price = effective_gas_price(&gas, apply.msg_receipt.gas_used)?;
            transaction.effective_gas_price = Some(effective_gas_price);
            let receipt = TransactionReceipt {
                transaction_hash: transaction.hash,
                transaction_index: index,
                block_hash,
                block_number: number,
                from: transaction.from,
                to: transaction.to,
                contract_address: execution_return
                    .as_ref()
                    .and_then(|result| result.contract_address),
                cumulative_gas_used,
                gas_used: apply.msg_receipt.gas_used,
                effective_gas_price,
                status: apply.msg_receipt.exit_code.is_success(),
                exit_code: apply.msg_receipt.exit_code.value(),
                output: execution_return.map_or_else(Bytes::new, |result| result.output),
                logs,
            };
            transactions.push(transaction);
            receipts.push(receipt);
            gas_breakdowns.push(gas);
        }

        let first_block = Block {
            hash: block_hash,
            parent_hash,
            number,
            state_root: ethereum_state_root(&root),
            timestamp: first_timestamp,
            gas_limit: BLOCK_GAS_LIMIT,
            gas_used: cumulative_gas_used,
            base_fee_per_gas: BASE_FEE_PER_GAS,
            transactions: transactions.clone(),
            logs: block_logs,
        };
        records.push(CanonicalBlock { block: first_block, root });

        let mut parent_hash = block_hash;
        for offset in 1..count {
            let number = number
                .checked_add(offset)
                .expect("mining block-number arithmetic must be preflighted");
            let timestamp = schedule.timestamp_at(offset);
            let hash = synthetic_block_hash(parent_hash, number, timestamp, &root, &[]);
            let block = Block {
                hash,
                parent_hash,
                number,
                state_root: ethereum_state_root(&root),
                timestamp,
                gas_limit: BLOCK_GAS_LIMIT,
                gas_used: 0,
                base_fee_per_gas: BASE_FEE_PER_GAS,
                transactions: Vec::new(),
                logs: Vec::new(),
            };
            records.push(CanonicalBlock { block, root });
            parent_hash = hash;
        }

        self.current_root = root;
        for record in &records {
            self.block_numbers_by_hash.insert(record.block.hash, record.block.number);
        }
        for ((transaction, receipt), gas) in
            transactions.into_iter().zip(receipts).zip(gas_breakdowns)
        {
            let hash = transaction.hash;
            self.transactions.insert(hash, transaction);
            self.receipts.insert(hash, receipt);
            self.gas_breakdowns.insert(hash, gas);
        }
        for _ in 0..pending.len() {
            self.pool.pop_front();
        }
        let blocks = records.iter().map(|record| record.block.clone()).collect();
        self.blocks.extend(records);
        self.next_epoch = next_epoch;
        self.next_timestamp = schedule.next_timestamp;
        Ok(blocks)
    }

    fn resolve_state(
        &self,
        selector: BlockSelector,
        allow_pending: bool,
    ) -> Result<(Cid, i64, u64), NodeError> {
        if selector == BlockSelector::Pending {
            if allow_pending {
                return Ok((self.current_root, self.next_epoch, self.next_timestamp));
            }
            return Err(NodeError::UnsupportedSelector("pending"));
        }
        let number = self.resolve_block_number(selector)?;
        let record =
            self.blocks.get(number as usize).ok_or(NodeError::UnknownBlockNumber(number))?;
        let epoch = i64::try_from(record.block.number).map_err(|_| {
            NodeError::Execution("canonical block exceeds the FVM epoch range".into())
        })?;
        Ok((record.root, epoch, record.block.timestamp))
    }

    fn resolve_block_number(&self, selector: BlockSelector) -> Result<u64, NodeError> {
        match selector {
            BlockSelector::Latest => Ok(self.block_number()),
            BlockSelector::Earliest => Ok(0),
            BlockSelector::Number(number) if number <= self.block_number() => Ok(number),
            BlockSelector::Number(number) => Err(NodeError::UnknownBlockNumber(number)),
            BlockSelector::Hash(hash) => self
                .block_numbers_by_hash
                .get(&hash)
                .copied()
                .ok_or(NodeError::UnknownBlockHash(hash)),
            BlockSelector::Pending => Err(NodeError::UnsupportedSelector("pending")),
        }
    }

    fn log_range_endpoint(
        &self,
        selector: Option<BlockSelector>,
        default: u64,
    ) -> Result<u64, NodeError> {
        match selector {
            None | Some(BlockSelector::Latest | BlockSelector::Pending) => Ok(default),
            Some(BlockSelector::Earliest) => Ok(0),
            Some(BlockSelector::Number(number)) => Ok(number),
            Some(BlockSelector::Hash(_)) => {
                Err(NodeError::UnsupportedSelector("block hash in log range"))
            }
        }
    }

    fn evm_state_v18(
        &self,
        address: EthAddress,
        actor: &ActorState,
    ) -> Result<EvmStateV18, NodeError> {
        self.store
            .get_cbor(&actor.state)
            .map_err(|error| NodeError::MalformedEvmState {
                address,
                state: Box::new(actor.state),
                detail: error.to_string(),
            })?
            .ok_or_else(|| NodeError::MalformedEvmState {
                address,
                state: Box::new(actor.state),
                detail: "state block is absent".to_owned(),
            })
    }

    fn actor_at(
        &self,
        root: Cid,
        address: EthAddress,
    ) -> Result<Option<fvm::state_tree::ActorState>, NodeError> {
        let delegated = FvmAddress::new_delegated(EAM_ACTOR_ID, address.as_slice())
            .map_err(|error| NodeError::InvalidDelegatedAddress(error.to_string()))?;
        let tree = StateTree::new_from_root(Rc::clone(&self.store), &root)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let Some(actor_id) =
            tree.lookup_id(&delegated).map_err(|error| NodeError::Execution(error.to_string()))?
        else {
            return Ok(None);
        };
        tree.get_actor(actor_id).map_err(|error| NodeError::Execution(error.to_string()))
    }

    fn temporary_sender_root(&self, root: Cid, call: &UnsignedCall) -> Result<Cid, NodeError> {
        let delegated = FvmAddress::new_delegated(EAM_ACTOR_ID, call.from.as_slice())
            .map_err(|error| NodeError::InvalidDelegatedAddress(error.to_string()))?;
        let placeholder_state = self
            .store
            // builtin-actors v18 placeholder state, matching the attributed genesis layout.
            .put_cbor(&[0_u8; 32], Code::Blake2b256)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let gas_funds = call.max_fee_per_gas.saturating_mul(U256::from(call.gas_limit));
        let balance = call.value.saturating_add(gas_funds);
        let mut tree = StateTree::new_from_root(Rc::clone(&self.store), &root)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let actor_id = tree
            .register_new_address(&delegated)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        tree.set_actor(
            actor_id,
            ActorState {
                code: self.bundle.code("placeholder")?,
                state: placeholder_state,
                sequence: call.nonce,
                balance: u256_to_token_amount(balance),
                delegated_address: Some(delegated),
            },
        );
        tree.flush().map_err(|error| NodeError::Execution(error.to_string()))
    }

    fn emitter_address(&self, root: Cid, actor_id: u64) -> Result<Option<EthAddress>, NodeError> {
        let tree = StateTree::new_from_root(Rc::clone(&self.store), &root)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let Some(actor) =
            tree.get_actor(actor_id).map_err(|error| NodeError::Execution(error.to_string()))?
        else {
            return Ok(None);
        };
        let Some(delegated) = actor.delegated_address else {
            return Ok(None);
        };
        let Payload::Delegated(payload) = delegated.payload() else {
            return Ok(None);
        };
        if payload.namespace() != EAM_ACTOR_ID {
            return Ok(None);
        }
        let bytes: [u8; 20] = payload.subaddress().try_into().map_err(|_| {
            NodeError::InvalidDelegatedAddress(format!(
                "actor {actor_id} has {} delegated subaddress bytes",
                payload.subaddress().len()
            ))
        })?;
        Ok(Some(EthAddress::from(bytes)))
    }

    fn new_executor_at(
        &self,
        root: Cid,
        epoch: i64,
        timestamp: u64,
    ) -> Result<RuntimeExecutor, NodeError> {
        let mut context = self.network.for_epoch(epoch, timestamp, root);
        context.set_base_fee(TokenAmount::from_atto(BASE_FEE_PER_GAS));
        let machine = DefaultMachine::new(&context, Rc::clone(&self.store), DeterministicExterns)
            .map_err(|error| NodeError::Runtime(error.to_string()))?;
        DefaultExecutor::new(self.engine_pool.clone(), machine)
            .map_err(|error| NodeError::Runtime(error.to_string()))
    }
}

struct Simulation {
    apply: ApplyRet,
    to: Option<EthAddress>,
}

fn public_transaction(transaction: &BridgedTransaction) -> Transaction {
    Transaction {
        hash: transaction.tx_hash(),
        chain_id: transaction.chain_id(),
        from: transaction.sender(),
        to: transaction.recipient(),
        nonce: transaction.nonce(),
        value: transaction.value(),
        input: transaction.input().clone(),
        gas_limit: transaction.gas_limit(),
        max_fee_per_gas: transaction.max_fee_per_gas(),
        max_priority_fee_per_gas: transaction.max_priority_fee_per_gas(),
        raw: transaction.raw_envelope().clone(),
        effective_gas_price: None,
        block_hash: None,
        block_number: None,
        transaction_index: None,
    }
}

fn maximum_transaction_cost(transaction: &BridgedTransaction) -> U256 {
    let gas_cost = U256::from(transaction.gas_limit())
        .saturating_mul(U256::from(transaction.max_fee_per_gas()));
    transaction.value().saturating_add(gas_cost)
}

fn filecoin_gas_breakdown(apply: &ApplyRet) -> Result<FilecoinGasBreakdown, NodeError> {
    Ok(FilecoinGasBreakdown {
        base_fee_burn: token_amount_to_u256(&apply.base_fee_burn)?,
        over_estimation_burn: token_amount_to_u256(&apply.over_estimation_burn)?,
        miner_penalty: token_amount_to_u256(&apply.penalty)?,
        miner_tip: token_amount_to_u256(&apply.miner_tip)?,
        refund: token_amount_to_u256(&apply.refund)?,
        gas_refund: apply.gas_refund,
        gas_burned: apply.gas_burned,
    })
}

fn effective_gas_price(gas: &FilecoinGasBreakdown, gas_used: u64) -> Result<U256, NodeError> {
    if gas_used == 0 {
        return Ok(U256::ZERO);
    }
    let charged = gas
        .base_fee_burn
        .checked_add(gas.over_estimation_burn)
        .and_then(|total| total.checked_add(gas.miner_tip))
        .ok_or_else(|| NodeError::GasAccounting("fee charge sum exceeds U256".to_owned()))?;
    Ok(charged / U256::from(gas_used))
}

fn execution_kind(recipient: Option<EthAddress>) -> ExecutionKind {
    match recipient {
        Some(_) => ExecutionKind::InvokeContract,
        None => ExecutionKind::CreateExternal,
    }
}

fn call_failure(apply: &ApplyRet) -> Result<NodeError, NodeError> {
    let detail = apply
        .failure_info
        .as_ref()
        .map_or_else(|| "execution reverted".to_owned(), ToString::to_string);
    let data = decode_revert_data(&apply.msg_receipt.return_data)?;
    Ok(NodeError::CallFailed { exit_code: apply.msg_receipt.exit_code.value(), detail, data })
}

fn u256_to_token_amount(amount: U256) -> TokenAmount {
    TokenAmount::from_atto(BigInt::from_bytes_be(Sign::Plus, &amount.to_be_bytes::<32>()))
}

fn token_amount_to_u256(amount: &TokenAmount) -> Result<U256, NodeError> {
    let (sign, bytes) = amount.atto().to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 32 {
        return Err(NodeError::InvalidTokenAmount(amount.to_string()));
    }
    Ok(U256::from_be_slice(&bytes))
}

fn log_matches_filter(log: &Log, filter: &LogFilter) -> bool {
    if !filter.addresses.is_empty() && !filter.addresses.contains(&log.address) {
        return false;
    }
    filter.topics.iter().enumerate().all(|(position, alternatives)| {
        alternatives.as_ref().is_none_or(|alternatives| {
            log.topics.get(position).is_some_and(|topic| alternatives.contains(topic))
        })
    })
}

fn ethereum_state_root(root: &Cid) -> B256 {
    let digest = root.hash().digest();
    if digest.len() == 32 {
        let mut output = [0_u8; 32];
        output.copy_from_slice(digest);
        B256::from(output)
    } else {
        let mut hasher = Keccak256::new();
        hasher.update(root.to_bytes());
        hasher.finalize()
    }
}

fn synthetic_genesis_hash(chain_id: u64, root: &Cid) -> B256 {
    let mut hasher = Keccak256::new();
    hasher.update(b"fvm-anvil/genesis/v1");
    hasher.update(chain_id.to_be_bytes());
    hasher.update(root.to_bytes());
    hasher.finalize()
}

fn synthetic_block_hash(
    parent_hash: B256,
    number: u64,
    timestamp: u64,
    root: &Cid,
    transaction_hashes: &[B256],
) -> B256 {
    let mut hasher = Keccak256::new();
    hasher.update(b"fvm-anvil/block/v1");
    hasher.update(parent_hash);
    hasher.update(number.to_be_bytes());
    hasher.update(timestamp.to_be_bytes());
    hasher.update(root.to_bytes());
    let transaction_count =
        u64::try_from(transaction_hashes.len()).expect("transaction vector length must fit u64");
    hasher.update(transaction_count.to_be_bytes());
    for hash in transaction_hashes {
        hasher.update(hash);
    }
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use alloy_primitives::{Address, B256, Bytes, U256};
    use fvm_ipld_blockstore::MemoryBlockstore;
    use fvm_ipld_encoding::{from_slice, to_vec};
    use fvm_ipld_kamt::AsHashedKey;

    use super::{
        EVM_STATE_KAMT_CONFIG_V18, EvmStateKamtV18, EvmWordV18, MiningSchedule,
        StateHashAlgorithmV18, effective_gas_price, log_matches_filter,
    };
    use crate::error::NodeError;
    use crate::types::{FilecoinGasBreakdown, Log, LogFilter};

    #[test]
    fn v18_word_adapter_uses_minimal_serde_full_width_hash_and_exact_kamt_config() {
        let zero = EvmWordV18(U256::ZERO);
        let value = EvmWordV18(U256::from(0x0100_u64));
        assert_eq!(to_vec(&zero).unwrap(), [0x40]);
        assert_eq!(to_vec(&value).unwrap(), [0x42, 0x01, 0x00]);
        assert_eq!(from_slice::<EvmWordV18>(&[0x42, 0x01, 0x00]).unwrap(), value);
        assert_eq!(
            <StateHashAlgorithmV18 as AsHashedKey<EvmWordV18, 32>>::as_hashed_key(&value).as_ref(),
            &value.0.to_be_bytes::<32>()
        );
        assert_eq!(EVM_STATE_KAMT_CONFIG_V18.bit_width, 5);
        assert_eq!(EVM_STATE_KAMT_CONFIG_V18.min_data_depth, 0);
        assert_eq!(EVM_STATE_KAMT_CONFIG_V18.max_array_width, 1);

        let store = Rc::new(MemoryBlockstore::default());
        let mut storage =
            EvmStateKamtV18::new_with_config(Rc::clone(&store), EVM_STATE_KAMT_CONFIG_V18.clone());
        storage.set(value, EvmWordV18(U256::from(9_u64))).unwrap();
        let root = storage.flush().unwrap();
        let loaded = EvmStateKamtV18::load_with_config(
            &root,
            Rc::clone(&store),
            EVM_STATE_KAMT_CONFIG_V18.clone(),
        )
        .unwrap();
        assert_eq!(loaded.get(&value).unwrap().unwrap().0, U256::from(9_u64));
    }

    #[test]
    fn effective_price_charges_base_overestimation_and_tip() {
        let gas = FilecoinGasBreakdown {
            base_fee_burn: U256::from(100_u64),
            over_estimation_burn: U256::from(20_u64),
            miner_penalty: U256::ZERO,
            miner_tip: U256::from(30_u64),
            refund: U256::ZERO,
            gas_refund: 0,
            gas_burned: 0,
        };

        assert_eq!(effective_gas_price(&gas, 10).unwrap(), U256::from(15_u64));
        assert_eq!(effective_gas_price(&gas, 0).unwrap(), U256::ZERO);
    }

    #[test]
    fn mining_schedule_matches_foundry_timestamp_semantics() {
        let interval = MiningSchedule::new(0, 2, Some(7), None).unwrap();
        assert_eq!(
            (0..2).map(|offset| interval.timestamp_at(offset)).collect::<Vec<_>>(),
            vec![7, 14]
        );
        assert_eq!(interval.next_timestamp, 15);

        let default = MiningSchedule::new(14, 1, None, None).unwrap();
        assert_eq!(default.timestamp_at(0), 15);
        assert_eq!(default.next_timestamp, 16);

        let same_timestamp = MiningSchedule::new(14, 1, None, Some(14)).unwrap();
        assert_eq!(same_timestamp.timestamp_at(0), 14);
        assert_eq!(same_timestamp.next_timestamp, 15);

        assert!(matches!(
            MiningSchedule::new(14, 1, None, Some(13)),
            Err(NodeError::InvalidMineTimestamp { minimum: 14, actual: 13 })
        ));

        let zero_interval = MiningSchedule::new(14, 2, Some(0), None).unwrap();
        assert_eq!(
            (0..2).map(|offset| zero_interval.timestamp_at(offset)).collect::<Vec<_>>(),
            vec![14, 14]
        );
        assert_eq!(zero_interval.next_timestamp, 15);
    }

    #[test]
    fn mining_schedule_rejects_all_overflow_before_use() {
        assert!(matches!(
            MiningSchedule::new(0, 2, Some(u64::MAX), None),
            Err(NodeError::MineArithmeticOverflow { .. })
        ));
        assert!(matches!(
            MiningSchedule::new(u64::MAX, 1, Some(0), Some(u64::MAX)),
            Err(NodeError::MineArithmeticOverflow { .. })
        ));
    }

    #[test]
    fn log_filter_applies_address_and_positional_topic_or_lists() {
        let address = Address::with_last_byte(1);
        let first = B256::with_last_byte(1);
        let second = B256::with_last_byte(2);
        let log = Log {
            address,
            topics: vec![first, second],
            data: Bytes::new(),
            block_hash: B256::with_last_byte(3),
            block_number: 4,
            transaction_hash: B256::with_last_byte(5),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        let matching = LogFilter {
            addresses: vec![Address::ZERO, address],
            topics: [Some(vec![first]), None, None, None],
            ..LogFilter::default()
        };
        let missing_second = LogFilter {
            topics: [None, Some(vec![B256::ZERO]), None, None],
            ..LogFilter::default()
        };
        let empty_or =
            LogFilter { topics: [Some(Vec::new()), None, None, None], ..LogFilter::default() };

        assert!(log_matches_filter(&log, &matching));
        assert!(!log_matches_filter(&log, &missing_second));
        assert!(!log_matches_filter(&log, &empty_or));
    }
}
