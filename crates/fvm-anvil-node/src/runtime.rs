use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Instant;

use alloy_primitives::{Address as EthAddress, B256, Bytes, Keccak256, U256, keccak256};
use cid::Cid;
use fvm::call_manager::DefaultCallManager;
use fvm::engine::EnginePool;
use fvm::executor::{ApplyKind, ApplyRet, DefaultExecutor, Executor};
use fvm::kernel::filecoin::DefaultFilecoinKernel;
use fvm::machine::{DefaultMachine, NetworkConfig};
use fvm::state_tree::{ActorState, StateTree};
use fvm::trace::ExecutionEvent;
use fvm_anvil_eth::{
    BridgedTransaction, ExecutionKind, UnsignedCall, build_unsigned_message,
    decode_execution_return, decode_revert_data, decode_signed, delegated_chain_length,
    project_event,
};
use fvm_anvil_state::{
    ForestCar, evm::EvmStateV18, prepared::PreparedStateMetadata, verify::validate_prepared_archive,
};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::CborStore;
use fvm_shared::address::{Address as FvmAddress, Payload};
use fvm_shared::bigint::{BigInt, Sign};
use fvm_shared::chainid::ChainID;
use fvm_shared::econ::TokenAmount;
use multihash_codetable::Code;

use crate::bundle::{BundleArtifacts, load_bundle};
use crate::config::NodeConfig;
use crate::error::NodeError;
use crate::externs::DeterministicExterns;
use crate::genesis::{GenesisOutput, PrefundedAccount, create_genesis, fund_development_accounts};
use crate::storage::RuntimeBlockstore;
use crate::types::{
    Block, BlockSelector, CallRequest, FilecoinCallTrace, FilecoinGasBreakdown, FilecoinGasCharge,
    FilecoinGasTrace, FilecoinStateRoots, Log, LogFilter, Transaction, TransactionReceipt,
};

type RuntimeMachine = DefaultMachine<Rc<RuntimeBlockstore>, DeterministicExterns>;
type RuntimeKernel = DefaultFilecoinKernel<DefaultCallManager<RuntimeMachine>>;
type RuntimeExecutor = DefaultExecutor<RuntimeKernel>;

const BLOCK_TIME_SECONDS: u64 = 1;
const EAM_ACTOR_ID: u64 = 10;
const PRELOAD_ACTORS: [&str; 4] = ["eam", "ethaccount", "evm", "placeholder"];
const ERC20_BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
const ERC20_ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
const ERC20_MAX_BASE_SLOT: u64 = 255;

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
    gas_traces: HashMap<B256, FilecoinGasTrace>,
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

struct PreparedInput {
    archive: ForestCar,
    metadata: PreparedStateMetadata,
}

struct InitialState {
    prepared_root: Option<Cid>,
    current_root: Cid,
    block_number: u64,
    epoch: i64,
    timestamp: u64,
    base_fee_per_gas: u128,
    accounts: Vec<PrefundedAccount>,
}

pub(crate) struct NodeRuntime {
    store: Rc<RuntimeBlockstore>,
    engine_pool: EnginePool,
    network: NetworkConfig,
    current_root: Cid,
    prepared_root: Option<Cid>,
    bundle: BundleArtifacts,
    prefunded_accounts: Vec<PrefundedAccount>,
    genesis_block_number: u64,
    block_gas_limit: u64,
    base_fee_per_gas: u128,
    blocks: Vec<CanonicalBlock>,
    block_numbers_by_hash: HashMap<B256, u64>,
    transactions: HashMap<B256, Transaction>,
    receipts: HashMap<B256, TransactionReceipt>,
    gas_breakdowns: HashMap<B256, FilecoinGasBreakdown>,
    gas_traces: HashMap<B256, FilecoinGasTrace>,
    pool: VecDeque<PendingTransaction>,
    automine: bool,
    startup_mining_mode_changed: bool,
    next_epoch: i64,
    next_timestamp: u64,
    snapshots: HashMap<U256, SnapshotState>,
    next_snapshot_id: u64,
}

impl NodeRuntime {
    pub(crate) fn new(config: NodeConfig) -> Result<Self, NodeError> {
        let prepared = config.prepared_state().map(load_prepared_state).transpose()?;
        let (base, prepared_metadata) = match prepared {
            Some(input) => (Some(input.archive), Some(input.metadata)),
            None => (None, None),
        };
        let store = Rc::new(match base {
            Some(base) => RuntimeBlockstore::over(base),
            None => RuntimeBlockstore::empty(),
        });
        let bundle_started = Instant::now();
        let bundle =
            load_bundle(config.actor_bundle(), config.actor_bundle_cache(), store.as_ref())?;
        tracing::debug!(elapsed = ?bundle_started.elapsed(), "loaded actor bundle");

        let state_started = Instant::now();
        let initial = match prepared_metadata {
            Some(metadata) => initialize_prepared_state(&config, &store, &bundle, metadata)?,
            None => {
                let genesis = create_genesis(&store, &bundle, config.genesis_accounts())?;
                let epoch = i64::try_from(config.genesis_block_number()).map_err(|_| {
                    NodeError::Runtime("genesis block exceeds the FVM epoch range".into())
                })?;
                InitialState {
                    prepared_root: None,
                    current_root: genesis.root,
                    block_number: config.genesis_block_number(),
                    epoch,
                    timestamp: config.genesis_timestamp(),
                    base_fee_per_gas: config.base_fee_per_gas(),
                    accounts: genesis.accounts,
                }
            }
        };
        tracing::debug!(
            imported = initial.prepared_root.is_some(),
            elapsed = ?state_started.elapsed(),
            "initialized startup state"
        );
        let next_epoch = initial
            .epoch
            .checked_add(1)
            .ok_or_else(|| NodeError::Runtime("initial FVM epoch has no successor".into()))?;
        let next_timestamp = initial
            .timestamp
            .checked_add(BLOCK_TIME_SECONDS)
            .ok_or_else(|| NodeError::Runtime("initial timestamp has no successor".into()))?;

        let mut network = NetworkConfig::new(config.network_version());
        network.chain_id(ChainID::from(config.chain_id())).override_actors(bundle.root);
        let engine_started = Instant::now();
        let engine_pool = EnginePool::new((&network).into())
            .map_err(|error| NodeError::Runtime(error.to_string()))?;
        tracing::debug!(elapsed = ?engine_started.elapsed(), "created FVM engine pool");
        let engine = engine_pool.acquire();
        for name in PRELOAD_ACTORS {
            let code = bundle.code(name)?;
            let preload_started = Instant::now();
            let bytes = engine.preload(&code, store.as_ref()).map_err(|error| {
                NodeError::Runtime(format!("failed to preload {name} actor {code}: {error}"))
            })?;
            tracing::debug!(
                actor = name,
                %code,
                bytes,
                elapsed = ?preload_started.elapsed(),
                "preloaded actor Wasm"
            );
        }
        drop(engine);

        let initial_hash = synthetic_genesis_hash(
            config.chain_id(),
            initial.block_number,
            initial.timestamp,
            config.block_gas_limit(),
            initial.base_fee_per_gas,
            &initial.current_root,
        );
        let initial_block = Block {
            hash: initial_hash,
            parent_hash: B256::ZERO,
            number: initial.block_number,
            state_root: ethereum_state_root(&initial.current_root),
            timestamp: initial.timestamp,
            gas_limit: config.block_gas_limit(),
            gas_used: 0,
            base_fee_per_gas: initial.base_fee_per_gas,
            transactions: Vec::new(),
            logs: Vec::new(),
        };
        let mut block_numbers_by_hash = HashMap::new();
        block_numbers_by_hash.insert(initial_hash, initial.block_number);

        let runtime = Self {
            store,
            engine_pool,
            network,
            current_root: initial.current_root,
            prepared_root: initial.prepared_root,
            bundle,
            prefunded_accounts: initial.accounts,
            genesis_block_number: initial.block_number,
            block_gas_limit: config.block_gas_limit(),
            base_fee_per_gas: initial.base_fee_per_gas,
            blocks: vec![CanonicalBlock { block: initial_block, root: initial.current_root }],
            block_numbers_by_hash,
            transactions: HashMap::new(),
            receipts: HashMap::new(),
            gas_breakdowns: HashMap::new(),
            gas_traces: HashMap::new(),
            pool: VecDeque::new(),
            automine: config.automine(),
            startup_mining_mode_changed: false,
            next_epoch,
            next_timestamp,
            snapshots: HashMap::new(),
            next_snapshot_id: 1,
        };
        drop(runtime.new_executor_at(
            initial.current_root,
            initial.epoch,
            initial.timestamp,
            false,
        )?);
        Ok(runtime)
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.network.chain_id.into()
    }

    pub(crate) fn actor_bundle_root(&self) -> Cid {
        self.bundle.root
    }

    pub(crate) fn state_roots(&self) -> FilecoinStateRoots {
        FilecoinStateRoots { prepared: self.prepared_root, current: self.current_root }
    }

    pub(crate) fn accounts(&self) -> Vec<EthAddress> {
        self.prefunded_accounts.iter().map(|account| account.ethereum).collect()
    }

    pub(crate) fn initial_timestamp(&self) -> u64 {
        self.blocks[0].block.timestamp
    }

    pub(crate) fn base_fee_per_gas(&self) -> u128 {
        self.base_fee_per_gas
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
        let (root, epoch, timestamp) = self.resolve_state(selector, true)?;
        self.call_at_state(request, root, epoch, timestamp)
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
            BlockSelector::Earliest => self.genesis_block_number,
            BlockSelector::Number(number) => number,
            BlockSelector::Hash(hash) => {
                let Some(number) = self.block_numbers_by_hash.get(&hash).copied() else {
                    return Ok(None);
                };
                number
            }
            BlockSelector::Pending => return Err(NodeError::UnsupportedSelector("pending")),
        };
        Ok(self.canonical_record(number).map(|record| record.block.clone()))
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
        let Some(actor) = self.actor_at(root, address)? else {
            return Ok(0);
        };
        if actor.code == self.bundle.code("evm")? {
            Ok(self.evm_state_v18(address, &actor)?.nonce())
        } else {
            Ok(actor.sequence)
        }
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
            .get(&state.bytecode())
            .map_err(|error| NodeError::Execution(error.to_string()))?
            .ok_or_else(|| {
                NodeError::Execution(format!("missing EVM bytecode block {}", state.bytecode()))
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
        let value = state.storage(self.store.as_ref(), slot.to_be_bytes()).map_err(|error| {
            NodeError::MalformedEvmStorage {
                address,
                root: Box::new(state.contract_state()),
                detail: error.to_string(),
            }
        })?;
        Ok(B256::from(value))
    }

    pub(crate) fn set_balance(
        &mut self,
        address: EthAddress,
        balance: U256,
    ) -> Result<(), NodeError> {
        let mut tree = self.current_state_tree()?;
        let (actor_id, mut actor) = self.actor_for_state_mutation(&mut tree, address)?;
        actor.balance = u256_to_token_amount(balance);
        tree.set_actor(actor_id, actor);
        self.publish_state_tree(tree)
    }

    pub(crate) fn add_balance(
        &mut self,
        address: EthAddress,
        amount: U256,
    ) -> Result<(), NodeError> {
        let current = self.balance(address, BlockSelector::Latest)?;
        self.set_balance(address, current.saturating_add(amount))
    }

    pub(crate) fn set_nonce(&mut self, address: EthAddress, nonce: U256) -> Result<(), NodeError> {
        let nonce = u64::try_from(nonce).unwrap_or(u64::MAX);
        let mut tree = self.current_state_tree()?;
        let (actor_id, mut actor) = self.actor_for_state_mutation(&mut tree, address)?;
        if actor.code == self.bundle.code("evm")? {
            let mut state = self.evm_state_v18(address, &actor)?;
            state.set_nonce(nonce);
            actor.state = state.flush(self.store.as_ref(), &actor.state).map_err(|error| {
                NodeError::MalformedEvmState {
                    address,
                    state: Box::new(actor.state),
                    detail: error.to_string(),
                }
            })?;
        } else {
            actor.sequence = nonce;
        }
        tree.set_actor(actor_id, actor);
        self.publish_state_tree(tree)
    }

    pub(crate) fn set_code(&mut self, address: EthAddress, code: Bytes) -> Result<(), NodeError> {
        let mut tree = self.current_state_tree()?;
        let (actor_id, mut actor) = self.actor_for_state_mutation(&mut tree, address)?;
        let evm_code = self.bundle.code("evm")?;
        let mut state = if actor.code == evm_code {
            let mut state = self.evm_state_v18(address, &actor)?;
            state.replace_bytecode(self.store.as_ref(), code.as_ref()).map_err(|error| {
                NodeError::MalformedEvmState {
                    address,
                    state: Box::new(actor.state),
                    detail: error.to_string(),
                }
            })?;
            state
        } else {
            EvmStateV18::create(self.store.as_ref(), code.as_ref(), actor.sequence).map_err(
                |error| NodeError::MalformedEvmState {
                    address,
                    state: Box::new(actor.state),
                    detail: error.to_string(),
                },
            )?
        };
        state.clear_tombstone();
        actor.code = evm_code;
        actor.state = state.flush(self.store.as_ref(), &actor.state).map_err(|error| {
            NodeError::MalformedEvmState {
                address,
                state: Box::new(actor.state),
                detail: error.to_string(),
            }
        })?;
        tree.set_actor(actor_id, actor);
        self.publish_state_tree(tree)
    }

    pub(crate) fn set_storage(
        &mut self,
        address: EthAddress,
        slot: U256,
        value: B256,
    ) -> Result<(), NodeError> {
        let mut tree = self.current_state_tree()?;
        let (actor_id, mut actor) = self.actor_for_state_mutation(&mut tree, address)?;
        let evm_code = self.bundle.code("evm")?;
        let mut state = if actor.code == evm_code {
            self.evm_state_v18(address, &actor)?
        } else {
            EvmStateV18::create(self.store.as_ref(), &[], actor.sequence).map_err(|error| {
                NodeError::MalformedEvmState {
                    address,
                    state: Box::new(actor.state),
                    detail: error.to_string(),
                }
            })?
        };
        state.replace_storage_slot(self.store.as_ref(), slot.to_be_bytes(), value.0).map_err(
            |error| NodeError::MalformedEvmStorage {
                address,
                root: Box::new(state.contract_state()),
                detail: error.to_string(),
            },
        )?;
        actor.code = evm_code;
        actor.state = state.flush(self.store.as_ref(), &actor.state).map_err(|error| {
            NodeError::MalformedEvmState {
                address,
                state: Box::new(actor.state),
                detail: error.to_string(),
            }
        })?;
        tree.set_actor(actor_id, actor);
        self.publish_state_tree(tree)
    }

    pub(crate) fn deal_erc20(
        &mut self,
        account: EthAddress,
        token: EthAddress,
        balance: U256,
    ) -> Result<(), NodeError> {
        let calldata = erc20_balance_calldata(account);
        let slot = self.find_erc20_storage_slot(token, calldata, balance, "balance", |base| {
            solidity_address_mapping_slot(account, U256::from(base))
        })?;
        if let Some(slot) = slot {
            self.set_storage(token, U256::from_be_bytes(slot.0), B256::from(balance))?;
        }
        Ok(())
    }

    pub(crate) fn set_erc20_allowance(
        &mut self,
        owner: EthAddress,
        spender: EthAddress,
        token: EthAddress,
        amount: U256,
    ) -> Result<(), NodeError> {
        let calldata = erc20_allowance_calldata(owner, spender);
        let slot = self.find_erc20_storage_slot(token, calldata, amount, "allowance", |base| {
            let inner = solidity_address_mapping_slot(owner, U256::from(base));
            solidity_address_mapping_slot(spender, U256::from_be_bytes(inner.0))
        })?;
        if let Some(slot) = slot {
            self.set_storage(token, U256::from_be_bytes(slot.0), B256::from(amount))?;
        }
        Ok(())
    }

    pub(crate) fn logs(&self, filter: LogFilter) -> Result<Vec<Log>, NodeError> {
        let records = if let Some(hash) = filter.block_hash {
            if filter.from_block.is_some() || filter.to_block.is_some() {
                return Err(NodeError::ConflictingLogFilterBlocks);
            }
            let Some(number) = self.block_numbers_by_hash.get(&hash).copied() else {
                return Ok(Vec::new());
            };
            let Some(index) = self.canonical_index(number) else {
                return Ok(Vec::new());
            };
            &self.blocks[index..=index]
        } else {
            let from = self.log_range_endpoint(filter.from_block, self.block_number())?;
            let to = self.log_range_endpoint(filter.to_block, self.block_number())?;
            if from > to {
                return Err(NodeError::InvalidLogBlockRange { from, to });
            }
            if to < self.genesis_block_number || from > self.block_number() {
                return Ok(Vec::new());
            }
            let from = from.max(self.genesis_block_number);
            let to = to.min(self.block_number());
            let from_index =
                self.canonical_index(from).expect("clamped canonical log range start must exist");
            let to_index =
                self.canonical_index(to).expect("clamped canonical log range end must exist");
            &self.blocks[from_index..=to_index]
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
        self.startup_mining_mode_changed = true;
        Ok(())
    }

    pub(crate) fn startup_mining_mode_changed(&self) -> bool {
        self.startup_mining_mode_changed
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
            gas_traces: self.gas_traces.clone(),
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
        self.gas_traces = snapshot.gas_traces;
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

    pub(crate) fn filecoin_gas_trace(&self, hash: B256) -> Option<FilecoinGasTrace> {
        self.gas_traces.get(&hash).cloned()
    }

    fn validate_pool_admission(&self, transaction: &BridgedTransaction) -> Result<(), NodeError> {
        let hash = transaction.tx_hash();
        if self.transactions.contains_key(&hash)
            || self.pool.iter().any(|pending| pending.transaction.hash == hash)
        {
            return Err(NodeError::DuplicateTransaction(hash));
        }
        if transaction.gas_limit() == 0 || transaction.gas_limit() > self.block_gas_limit {
            return Err(NodeError::InvalidGasLimit {
                actual: transaction.gas_limit(),
                maximum: self.block_gas_limit,
            });
        }
        let pending_gas = self
            .pool
            .iter()
            .fold(0_u64, |total, pending| total.saturating_add(pending.bridged.gas_limit()));
        let total_gas = pending_gas.saturating_add(transaction.gas_limit());
        if total_gas > self.block_gas_limit {
            return Err(NodeError::PendingBlockGasLimit {
                actual: total_gas,
                maximum: self.block_gas_limit,
            });
        }
        if transaction.max_priority_fee_per_gas() > transaction.max_fee_per_gas() {
            return Err(NodeError::PriorityFeeAboveCap {
                priority_fee: transaction.max_priority_fee_per_gas(),
                fee_cap: transaction.max_fee_per_gas(),
            });
        }
        if transaction.max_fee_per_gas() < self.base_fee_per_gas {
            return Err(NodeError::FeeCapTooLow {
                fee_cap: transaction.max_fee_per_gas(),
                base_fee: self.base_fee_per_gas,
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
        let committed = match self.actor_at(self.current_root, address)? {
            Some(actor) if actor.code == self.bundle.code("evm")? => {
                self.evm_state_v18(address, &actor)?.nonce()
            }
            Some(actor) => actor.sequence,
            None => 0,
        };
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
        self.simulate_at(request, root, epoch, timestamp)
    }

    fn simulate_at(
        &self,
        request: CallRequest,
        root: Cid,
        epoch: i64,
        timestamp: u64,
    ) -> Result<Simulation, NodeError> {
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
            gas_limit: request.gas_limit.unwrap_or(self.block_gas_limit),
            nonce,
            max_fee_per_gas: U256::from(request.max_fee_per_gas.unwrap_or(self.base_fee_per_gas)),
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
        let mut executor = self.new_executor_at(execution_root, epoch, timestamp, false)?;
        let apply = executor
            .execute_message(message, ApplyKind::Explicit, serialized_length)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let _unpublished_root =
            executor.flush().map_err(|error| NodeError::Execution(error.to_string()))?;
        Ok(Simulation { apply, to })
    }

    fn call_at_state(
        &self,
        request: CallRequest,
        root: Cid,
        epoch: i64,
        timestamp: u64,
    ) -> Result<Bytes, NodeError> {
        let simulation = self.simulate_at(request, root, epoch, timestamp)?;
        if !simulation.apply.msg_receipt.exit_code.is_success() {
            return Err(call_failure(&simulation.apply)?);
        }
        let kind = execution_kind(simulation.to);
        Ok(decode_execution_return(kind, &simulation.apply.msg_receipt.return_data)?.output)
    }

    fn find_erc20_storage_slot(
        &self,
        token: EthAddress,
        calldata: Bytes,
        requested: U256,
        field: &'static str,
        slot_for_base: impl Fn(u64) -> B256,
    ) -> Result<Option<B256>, NodeError> {
        let request = CallRequest { to: Some(token), input: calldata, ..CallRequest::default() };
        let (root, epoch, timestamp) = self.resolve_state(BlockSelector::Latest, true)?;
        let current = decode_erc20_uint(
            token,
            field,
            &self.call_at_state(request.clone(), root, epoch, timestamp)?,
        )?;
        if current == requested {
            return Ok(None);
        }

        for base in 0..=ERC20_MAX_BASE_SLOT {
            let slot = slot_for_base(base);
            let candidate_root =
                self.storage_override_root(root, token, slot, B256::from(requested))?;
            let Ok(output) = self.call_at_state(request.clone(), candidate_root, epoch, timestamp)
            else {
                continue;
            };
            if decode_erc20_uint(token, field, &output).is_ok_and(|value| value == requested) {
                return Ok(Some(slot));
            }
        }
        Err(NodeError::Erc20StorageSlotNotFound { token, field })
    }

    fn commit_blocks(
        &mut self,
        count: u64,
        interval: Option<u64>,
        timestamp: Option<u64>,
    ) -> Result<Vec<Block>, NodeError> {
        let head_timestamp = self.blocks.last().map_or(0, |record| record.block.timestamp);
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

        let pending_len = self.pool.len();
        let mut applications = Vec::with_capacity(pending_len);
        let root = if self.pool.is_empty() {
            self.current_root
        } else {
            let mut executor =
                self.new_executor_at(self.current_root, self.next_epoch, first_timestamp, true)?;
            for transaction in &self.pool {
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
            self.pool.iter().map(|transaction| transaction.transaction.hash).collect();
        let block_hash =
            synthetic_block_hash(parent_hash, number, first_timestamp, &root, &transaction_hashes);

        let mut transactions = Vec::with_capacity(pending_len);
        let mut receipts = Vec::with_capacity(pending_len);
        let mut gas_breakdowns = Vec::with_capacity(pending_len);
        let mut gas_traces = Vec::with_capacity(pending_len);
        let mut block_logs = Vec::new();
        let mut cumulative_gas_used = 0_u64;
        let mut next_log_index = 0_u64;
        let mut emitter_addresses = HashMap::new();
        for (index, (pending, apply)) in self.pool.iter().zip(&applications).enumerate() {
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
                let emitter = if let Some(address) = emitter_addresses.get(&event.emitter) {
                    *address
                } else {
                    let address = self.emitter_address(root, event.emitter)?;
                    emitter_addresses.insert(event.emitter, address);
                    address
                };
                let Some(emitter) = emitter else {
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
            let trace = filecoin_gas_trace(apply);
            let effective_gas_price = effective_gas_price(
                pending.bridged.max_fee_per_gas(),
                pending.bridged.max_priority_fee_per_gas(),
                self.base_fee_per_gas,
            );
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
            gas_traces.push(trace);
        }

        let first_block = Block {
            hash: block_hash,
            parent_hash,
            number,
            state_root: ethereum_state_root(&root),
            timestamp: first_timestamp,
            gas_limit: self.block_gas_limit,
            gas_used: cumulative_gas_used,
            base_fee_per_gas: self.base_fee_per_gas,
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
                gas_limit: self.block_gas_limit,
                gas_used: 0,
                base_fee_per_gas: self.base_fee_per_gas,
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
        for (((transaction, receipt), gas), trace) in
            transactions.into_iter().zip(receipts).zip(gas_breakdowns).zip(gas_traces)
        {
            let hash = transaction.hash;
            self.transactions.insert(hash, transaction);
            self.receipts.insert(hash, receipt);
            self.gas_breakdowns.insert(hash, gas);
            self.gas_traces.insert(hash, trace);
        }
        for _ in 0..pending_len {
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
        let record = self.canonical_record(number).ok_or(NodeError::UnknownBlockNumber(number))?;
        let epoch = i64::try_from(record.block.number).map_err(|_| {
            NodeError::Execution("canonical block exceeds the FVM epoch range".into())
        })?;
        let root = if selector == BlockSelector::Latest { self.current_root } else { record.root };
        Ok((root, epoch, record.block.timestamp))
    }

    fn resolve_block_number(&self, selector: BlockSelector) -> Result<u64, NodeError> {
        match selector {
            BlockSelector::Latest => Ok(self.block_number()),
            BlockSelector::Earliest => Ok(self.genesis_block_number),
            BlockSelector::Number(number)
                if number >= self.genesis_block_number && number <= self.block_number() =>
            {
                Ok(number)
            }
            BlockSelector::Number(number) => Err(NodeError::UnknownBlockNumber(number)),
            BlockSelector::Hash(hash) => self
                .block_numbers_by_hash
                .get(&hash)
                .copied()
                .ok_or(NodeError::UnknownBlockHash(hash)),
            BlockSelector::Pending => Err(NodeError::UnsupportedSelector("pending")),
        }
    }

    fn canonical_index(&self, number: u64) -> Option<usize> {
        let offset = number.checked_sub(self.genesis_block_number)?;
        let index = usize::try_from(offset).ok()?;
        (index < self.blocks.len()).then_some(index)
    }

    fn canonical_record(&self, number: u64) -> Option<&CanonicalBlock> {
        self.canonical_index(number).and_then(|index| self.blocks.get(index))
    }

    fn log_range_endpoint(
        &self,
        selector: Option<BlockSelector>,
        default: u64,
    ) -> Result<u64, NodeError> {
        match selector {
            None | Some(BlockSelector::Latest | BlockSelector::Pending) => Ok(default),
            Some(BlockSelector::Earliest) => Ok(self.genesis_block_number),
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
        EvmStateV18::load(self.store.as_ref(), &actor.state).map_err(|error| {
            NodeError::MalformedEvmState {
                address,
                state: Box::new(actor.state),
                detail: error.to_string(),
            }
        })
    }

    fn storage_override_root(
        &self,
        root: Cid,
        token: EthAddress,
        slot: B256,
        value: B256,
    ) -> Result<Cid, NodeError> {
        let delegated = FvmAddress::new_delegated(EAM_ACTOR_ID, token.as_slice())
            .map_err(|error| NodeError::InvalidDelegatedAddress(error.to_string()))?;
        let mut tree = StateTree::new_from_root(Rc::clone(&self.store), &root)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        let Some(actor_id) =
            tree.lookup_id(&delegated).map_err(|error| NodeError::Execution(error.to_string()))?
        else {
            return Err(NodeError::Erc20TokenNotEvm { token });
        };
        let Some(mut actor) =
            tree.get_actor(actor_id).map_err(|error| NodeError::Execution(error.to_string()))?
        else {
            return Err(NodeError::Erc20TokenNotEvm { token });
        };
        if actor.code != self.bundle.code("evm")? {
            return Err(NodeError::Erc20TokenNotEvm { token });
        }
        let mut state = self.evm_state_v18(token, &actor)?;
        state.replace_storage_slot(self.store.as_ref(), slot.0, value.0).map_err(|error| {
            NodeError::MalformedEvmStorage {
                address: token,
                root: Box::new(state.contract_state()),
                detail: error.to_string(),
            }
        })?;
        actor.state = state.flush(self.store.as_ref(), &actor.state).map_err(|error| {
            NodeError::MalformedEvmState {
                address: token,
                state: Box::new(actor.state),
                detail: error.to_string(),
            }
        })?;
        tree.set_actor(actor_id, actor);
        tree.flush().map_err(|error| NodeError::Execution(error.to_string()))
    }

    fn current_state_tree(&self) -> Result<StateTree<Rc<RuntimeBlockstore>>, NodeError> {
        StateTree::new_from_root(Rc::clone(&self.store), &self.current_root)
            .map_err(|error| NodeError::Execution(error.to_string()))
    }

    fn actor_for_state_mutation(
        &self,
        tree: &mut StateTree<Rc<RuntimeBlockstore>>,
        address: EthAddress,
    ) -> Result<(u64, ActorState), NodeError> {
        let delegated = FvmAddress::new_delegated(EAM_ACTOR_ID, address.as_slice())
            .map_err(|error| NodeError::InvalidDelegatedAddress(error.to_string()))?;
        let actor_id = match tree
            .lookup_id(&delegated)
            .map_err(|error| NodeError::Execution(error.to_string()))?
        {
            Some(actor_id) => actor_id,
            None => tree
                .register_new_address(&delegated)
                .map_err(|error| NodeError::Execution(error.to_string()))?,
        };
        if let Some(actor) =
            tree.get_actor(actor_id).map_err(|error| NodeError::Execution(error.to_string()))?
        {
            return Ok((actor_id, actor));
        }
        let state = self
            .store
            .put_cbor(&[0_u8; 32], Code::Blake2b256)
            .map_err(|error| NodeError::Execution(error.to_string()))?;
        Ok((
            actor_id,
            ActorState {
                code: self.bundle.code("placeholder")?,
                state,
                sequence: 0,
                balance: TokenAmount::from_atto(0),
                delegated_address: Some(delegated),
            },
        ))
    }

    fn publish_state_tree(
        &mut self,
        mut tree: StateTree<Rc<RuntimeBlockstore>>,
    ) -> Result<(), NodeError> {
        self.current_root =
            tree.flush().map_err(|error| NodeError::Execution(error.to_string()))?;
        Ok(())
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
        tracing: bool,
    ) -> Result<RuntimeExecutor, NodeError> {
        let mut context = self.network.for_epoch(epoch, timestamp, root);
        context.set_base_fee(TokenAmount::from_atto(self.base_fee_per_gas));
        if tracing {
            context.enable_tracing();
        }
        let machine = DefaultMachine::new(&context, Rc::clone(&self.store), DeterministicExterns)
            .map_err(|error| NodeError::Runtime(error.to_string()))?;
        DefaultExecutor::new(self.engine_pool.clone(), machine)
            .map_err(|error| NodeError::Runtime(error.to_string()))
    }
}

fn load_prepared_state(path: &std::path::Path) -> Result<PreparedInput, NodeError> {
    let archive = ForestCar::open(path).map_err(|error| NodeError::PreparedState {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    let metadata = validate_prepared_archive(&archive, archive.roots()).map_err(|error| {
        NodeError::PreparedState { path: path.to_path_buf(), detail: error.to_string() }
    })?;
    Ok(PreparedInput { archive, metadata })
}

fn initialize_prepared_state(
    config: &NodeConfig,
    store: &Rc<RuntimeBlockstore>,
    bundle: &BundleArtifacts,
    metadata: PreparedStateMetadata,
) -> Result<InitialState, NodeError> {
    let expected_network: u32 = config.network_version().into();
    if metadata.network_version != expected_network {
        return Err(prepared_configuration_error(
            config,
            format!(
                "archive network version {} does not match node network version {expected_network}",
                metadata.network_version
            ),
        ));
    }
    if metadata.actor_manifest_data != bundle.manifest_data {
        return Err(prepared_configuration_error(
            config,
            format!(
                "archive actor manifest data {} does not match configured bundle manifest {}",
                metadata.actor_manifest_data, bundle.manifest_data
            ),
        ));
    }
    let block_number = u64::try_from(metadata.epoch).map_err(|_| {
        prepared_configuration_error(
            config,
            format!("archive epoch {} cannot be represented as a block number", metadata.epoch),
        )
    })?;
    let base_fee = token_amount_to_u256(&metadata.base_fee)
        .map_err(|error| prepared_configuration_error(config, error.to_string()))?;
    let base_fee_per_gas = u128::try_from(base_fee)
        .map_err(|_| prepared_configuration_error(config, "archive base fee exceeds 128 bits"))?;
    let prepared_root = metadata.state_root;
    let GenesisOutput { root: current_root, accounts } =
        fund_development_accounts(store, &prepared_root, bundle, config.genesis_accounts())
            .map_err(|error| prepared_configuration_error(config, error.to_string()))?;
    Ok(InitialState {
        prepared_root: Some(prepared_root),
        current_root,
        block_number,
        epoch: metadata.epoch,
        timestamp: metadata.timestamp,
        base_fee_per_gas,
        accounts,
    })
}

fn prepared_configuration_error(config: &NodeConfig, detail: impl Into<String>) -> NodeError {
    NodeError::PreparedState {
        path: config
            .prepared_state()
            .expect("prepared configuration errors require a prepared-state path")
            .to_path_buf(),
        detail: detail.into(),
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

fn filecoin_gas_trace(apply: &ApplyRet) -> FilecoinGasTrace {
    let mut gas_charges = Vec::new();
    let mut calls = Vec::new();
    let mut stack = Vec::<FilecoinCallTrace>::new();

    for event in &apply.exec_trace {
        match event {
            ExecutionEvent::GasCharge(charge) => {
                let charge = FilecoinGasCharge {
                    name: charge.name.to_string(),
                    compute_milligas: charge.compute_gas.as_milligas(),
                    other_milligas: charge.other_gas.as_milligas(),
                    total_milligas: charge.total().as_milligas(),
                };
                if let Some(call) = stack.last_mut() {
                    call.gas_charges.push(charge);
                } else {
                    gas_charges.push(charge);
                }
            }
            ExecutionEvent::Call { from, to, method, gas_limit, read_only, .. } => {
                stack.push(FilecoinCallTrace {
                    from_actor_id: *from,
                    to: to.to_string(),
                    method: *method,
                    gas_limit: *gas_limit,
                    read_only: *read_only,
                    invoked_actor_id: None,
                    exit_code: None,
                    syscall_error: None,
                    gas_charges: Vec::new(),
                    subcalls: Vec::new(),
                })
            }
            ExecutionEvent::CallReturn(exit_code, _) => {
                if let Some(call) = stack.last_mut() {
                    call.exit_code = Some(exit_code.value());
                }
                finish_trace_call(&mut stack, &mut calls);
            }
            ExecutionEvent::CallError(error) => {
                if let Some(call) = stack.last_mut() {
                    call.syscall_error = Some(error.to_string());
                }
                finish_trace_call(&mut stack, &mut calls);
            }
            ExecutionEvent::InvokeActor { id, .. } => {
                if let Some(call) = stack.last_mut() {
                    call.invoked_actor_id = Some(*id);
                }
            }
            _ => {}
        }
    }
    while !stack.is_empty() {
        finish_trace_call(&mut stack, &mut calls);
    }

    FilecoinGasTrace {
        gas_used: apply.msg_receipt.gas_used,
        exit_code: apply.msg_receipt.exit_code.value(),
        failure_info: apply.failure_info.as_ref().map(ToString::to_string),
        gas_charges,
        calls,
    }
}

fn finish_trace_call(stack: &mut Vec<FilecoinCallTrace>, calls: &mut Vec<FilecoinCallTrace>) {
    let Some(call) = stack.pop() else {
        return;
    };
    if let Some(parent) = stack.last_mut() {
        parent.subcalls.push(call);
    } else {
        calls.push(call);
    }
}

fn effective_gas_price(
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    base_fee_per_gas: u128,
) -> U256 {
    U256::from(max_fee_per_gas)
        .min(U256::from(base_fee_per_gas) + U256::from(max_priority_fee_per_gas))
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

fn erc20_balance_calldata(account: EthAddress) -> Bytes {
    let mut calldata = [0_u8; 36];
    calldata[..4].copy_from_slice(&ERC20_BALANCE_OF_SELECTOR);
    calldata[16..].copy_from_slice(account.as_slice());
    Bytes::copy_from_slice(&calldata)
}

fn erc20_allowance_calldata(owner: EthAddress, spender: EthAddress) -> Bytes {
    let mut calldata = [0_u8; 68];
    calldata[..4].copy_from_slice(&ERC20_ALLOWANCE_SELECTOR);
    calldata[16..36].copy_from_slice(owner.as_slice());
    calldata[48..].copy_from_slice(spender.as_slice());
    Bytes::copy_from_slice(&calldata)
}

fn solidity_address_mapping_slot(address: EthAddress, base: U256) -> B256 {
    let mut preimage = [0_u8; 64];
    preimage[12..32].copy_from_slice(address.as_slice());
    preimage[32..].copy_from_slice(&base.to_be_bytes::<32>());
    keccak256(preimage)
}

fn decode_erc20_uint(
    token: EthAddress,
    field: &'static str,
    output: &[u8],
) -> Result<U256, NodeError> {
    if output.len() != 32 {
        return Err(NodeError::MalformedErc20Return { token, field, length: output.len() });
    }
    Ok(U256::from_be_slice(output))
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

fn synthetic_genesis_hash(
    chain_id: u64,
    number: u64,
    timestamp: u64,
    gas_limit: u64,
    base_fee_per_gas: u128,
    root: &Cid,
) -> B256 {
    let mut hasher = Keccak256::new();
    hasher.update(b"fvm-anvil/genesis/v2");
    hasher.update(chain_id.to_be_bytes());
    hasher.update(number.to_be_bytes());
    hasher.update(timestamp.to_be_bytes());
    hasher.update(gas_limit.to_be_bytes());
    hasher.update(base_fee_per_gas.to_be_bytes());
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
    use alloy_primitives::{Address, B256, Bytes, U256};

    use super::{MiningSchedule, effective_gas_price, log_matches_filter};
    use crate::error::NodeError;
    use crate::types::{Log, LogFilter};

    #[test]
    fn effective_price_uses_eip_1559_cap_and_priority() {
        assert_eq!(effective_gas_price(200, 10, 100), U256::from(110_u64));
        assert_eq!(effective_gas_price(105, 10, 100), U256::from(105_u64));
        assert_eq!(effective_gas_price(u128::MAX, u128::MAX, u128::MAX), U256::from(u128::MAX));
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
