use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use alloy_primitives::{Address, B256, Bytes, U256};
use cid::Cid;
use tokio::sync::{mpsc, oneshot};

use crate::config::NodeConfig;
use crate::error::NodeError;
use crate::native::{HeadChange, NativeTipset};
use crate::runtime::NodeRuntime;
use crate::types::{
    Block, BlockSelector, CallRequest, FilecoinGasBreakdown, FilecoinGasTrace, FilecoinStateRoots,
    Log, LogFilter, Transaction, TransactionReceipt,
};

const COMMAND_CAPACITY: usize = 64;
const WORKER_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Cloneable asynchronous access to the worker-owned node state.
#[derive(Clone)]
pub struct NodeHandle {
    commands: mpsc::Sender<WorkerCommand>,
}

impl NodeHandle {
    /// Empty keys select the current head; nonempty keys must be canonical local keys.
    pub async fn native_tipset(&self, key: Vec<Cid>) -> Result<NativeTipset, NodeError> {
        self.request(|reply| WorkerCommand::NativeTipset { key, reply }).await
    }

    pub async fn native_tipset_by_height(
        &self,
        height: i64,
        key: Vec<Cid>,
    ) -> Result<NativeTipset, NodeError> {
        self.request(|reply| WorkerCommand::NativeTipsetByHeight { height, key, reply }).await
    }

    /// Returns the same raw beacon digest supplied to ref-FVM, bounded by the selected history.
    pub async fn beacon_digest(&self, epoch: i64, key: Vec<Cid>) -> Result<[u8; 32], NodeError> {
        self.request(|reply| WorkerCommand::BeaconDigest { epoch, key, reply }).await
    }

    /// Registration atomically enqueues Current before any later canonical head changes.
    pub async fn subscribe_heads(&self) -> Result<mpsc::Receiver<Vec<HeadChange>>, NodeError> {
        self.request(|reply| WorkerCommand::SubscribeHeads { reply }).await
    }

    pub async fn chain_id(&self) -> Result<u64, NodeError> {
        self.request(|reply| WorkerCommand::ChainId { reply }).await
    }

    pub async fn accounts(&self) -> Result<Vec<Address>, NodeError> {
        self.request(|reply| WorkerCommand::Accounts { reply }).await
    }

    pub async fn submit_raw_transaction(&self, raw: Bytes) -> Result<B256, NodeError> {
        self.request(|reply| WorkerCommand::SubmitRawTransaction { raw, reply }).await
    }

    pub async fn call(
        &self,
        request: CallRequest,
        selector: BlockSelector,
    ) -> Result<Bytes, NodeError> {
        self.request(|reply| WorkerCommand::Call { request, selector, reply }).await
    }

    pub async fn estimate_gas(
        &self,
        request: CallRequest,
        selector: BlockSelector,
    ) -> Result<u64, NodeError> {
        self.request(|reply| WorkerCommand::EstimateGas { request, selector, reply }).await
    }

    pub async fn block_number(&self) -> Result<u64, NodeError> {
        self.request(|reply| WorkerCommand::BlockNumber { reply }).await
    }

    pub async fn transaction_by_hash(&self, hash: B256) -> Result<Option<Transaction>, NodeError> {
        self.request(|reply| WorkerCommand::TransactionByHash { hash, reply }).await
    }

    pub async fn transaction_receipt(
        &self,
        hash: B256,
    ) -> Result<Option<TransactionReceipt>, NodeError> {
        self.request(|reply| WorkerCommand::TransactionReceipt { hash, reply }).await
    }

    pub async fn block_by_number(
        &self,
        selector: BlockSelector,
    ) -> Result<Option<Block>, NodeError> {
        self.request(|reply| WorkerCommand::BlockByNumber { selector, reply }).await
    }

    pub async fn balance(
        &self,
        address: Address,
        selector: BlockSelector,
    ) -> Result<U256, NodeError> {
        self.request(|reply| WorkerCommand::Balance { address, selector, reply }).await
    }

    pub async fn transaction_count(
        &self,
        address: Address,
        selector: BlockSelector,
    ) -> Result<u64, NodeError> {
        self.request(|reply| WorkerCommand::TransactionCount { address, selector, reply }).await
    }

    pub async fn code(
        &self,
        address: Address,
        selector: BlockSelector,
    ) -> Result<Bytes, NodeError> {
        self.request(|reply| WorkerCommand::Code { address, selector, reply }).await
    }

    pub async fn storage(
        &self,
        address: Address,
        slot: U256,
        selector: BlockSelector,
    ) -> Result<B256, NodeError> {
        self.request(|reply| WorkerCommand::Storage { address, slot, selector, reply }).await
    }

    /// Sets an account balance at the current canonical head.
    pub async fn set_balance(&self, address: Address, balance: U256) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::SetBalance { address, balance, reply }).await
    }

    /// Saturating-adds to an account balance at the current canonical head.
    pub async fn add_balance(&self, address: Address, amount: U256) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::AddBalance { address, amount, reply }).await
    }

    /// Sets the Ethereum-visible nonce at the current canonical head.
    pub async fn set_nonce(&self, address: Address, nonce: U256) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::SetNonce { address, nonce, reply }).await
    }

    /// Sets deployed EVM bytecode at the current canonical head.
    pub async fn set_code(&self, address: Address, code: Bytes) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::SetCode { address, code, reply }).await
    }

    /// Sets one EVM storage slot at the current canonical head.
    pub async fn set_storage(
        &self,
        address: Address,
        slot: U256,
        value: B256,
    ) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::SetStorage { address, slot, value, reply }).await
    }

    /// Discovers and sets a standard Solidity ERC-20 balance slot.
    pub async fn deal_erc20(
        &self,
        account: Address,
        token: Address,
        balance: U256,
    ) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::DealErc20 { account, token, balance, reply }).await
    }

    /// Discovers and sets a standard Solidity ERC-20 allowance slot.
    pub async fn set_erc20_allowance(
        &self,
        owner: Address,
        spender: Address,
        token: Address,
        amount: U256,
    ) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::SetErc20Allowance {
            owner,
            spender,
            token,
            amount,
            reply,
        })
        .await
    }

    pub async fn logs(&self, filter: LogFilter) -> Result<Vec<Log>, NodeError> {
        self.request(|reply| WorkerCommand::Logs { filter, reply }).await
    }

    pub async fn mine_blocks(
        &self,
        count: u64,
        interval: Option<u64>,
        timestamp: Option<u64>,
    ) -> Result<Vec<Block>, NodeError> {
        self.request(|reply| WorkerCommand::MineBlocks { count, interval, timestamp, reply }).await
    }

    pub async fn automine(&self) -> Result<bool, NodeError> {
        self.request(|reply| WorkerCommand::Automine { reply }).await
    }
    pub async fn startup_mining_mode_changed(&self) -> Result<bool, NodeError> {
        self.request(|reply| WorkerCommand::StartupMiningModeChanged { reply }).await
    }

    pub async fn set_automine(&self, enabled: bool) -> Result<(), NodeError> {
        self.request(|reply| WorkerCommand::SetAutomine { enabled, reply }).await
    }

    pub async fn snapshot(&self) -> Result<U256, NodeError> {
        self.request(|reply| WorkerCommand::Snapshot { reply }).await
    }

    pub async fn revert(&self, id: U256) -> Result<bool, NodeError> {
        self.request(|reply| WorkerCommand::Revert { id, reply }).await
    }

    pub async fn filecoin_gas_breakdown(
        &self,
        hash: B256,
    ) -> Result<Option<FilecoinGasBreakdown>, NodeError> {
        self.request(|reply| WorkerCommand::FilecoinGasBreakdown { hash, reply }).await
    }

    pub async fn filecoin_gas_trace(
        &self,
        hash: B256,
    ) -> Result<Option<FilecoinGasTrace>, NodeError> {
        self.request(|reply| WorkerCommand::FilecoinGasTrace { hash, reply }).await
    }

    pub async fn filecoin_state_roots(&self) -> Result<FilecoinStateRoots, NodeError> {
        self.request(|reply| WorkerCommand::FilecoinStateRoots { reply }).await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, NodeError>>) -> WorkerCommand,
    ) -> Result<T, NodeError> {
        let (reply, response) = oneshot::channel();
        self.commands.send(command(reply)).await.map_err(|_| NodeError::WorkerUnavailable)?;
        response.await.map_err(|_| NodeError::WorkerUnavailable)?
    }
}

/// Immutable information discovered while starting the FVM runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeStartupInfo {
    actor_bundle_root: Cid,
    state_roots: FilecoinStateRoots,
    initial_block_number: u64,
    initial_timestamp: u64,
    base_fee_per_gas: u128,
}

impl NodeStartupInfo {
    pub const fn actor_bundle_root(&self) -> Cid {
        self.actor_bundle_root
    }

    pub const fn state_roots(&self) -> FilecoinStateRoots {
        self.state_roots
    }

    pub const fn initial_block_number(&self) -> u64 {
        self.initial_block_number
    }

    pub const fn initial_timestamp(&self) -> u64 {
        self.initial_timestamp
    }

    pub const fn base_fee_per_gas(&self) -> u128 {
        self.base_fee_per_gas
    }
}

/// Owns the dedicated ref-FVM worker thread and its public handle.
pub struct NodeService {
    handle: NodeHandle,
    startup_info: NodeStartupInfo,
    _worker: JoinHandle<()>,
}

impl NodeService {
    /// Starts the worker and waits until bundle import, genesis, and runtime validation complete.
    pub fn start(config: NodeConfig) -> Result<Self, NodeError> {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (ready, readiness) = std_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("fvm-anvil-worker".to_owned())
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || run_worker(config, receiver, ready))
            .map_err(NodeError::WorkerSpawn)?;

        let startup_info = readiness.recv().map_err(|_| NodeError::WorkerStartup)??;
        Ok(Self { handle: NodeHandle { commands }, startup_info, _worker: worker })
    }

    pub fn handle(&self) -> NodeHandle {
        self.handle.clone()
    }

    pub const fn startup_info(&self) -> &NodeStartupInfo {
        &self.startup_info
    }
}

enum WorkerCommand {
    NativeTipset {
        key: Vec<Cid>,
        reply: Reply<NativeTipset>,
    },
    NativeTipsetByHeight {
        height: i64,
        key: Vec<Cid>,
        reply: Reply<NativeTipset>,
    },
    BeaconDigest {
        epoch: i64,
        key: Vec<Cid>,
        reply: Reply<[u8; 32]>,
    },
    SubscribeHeads {
        reply: Reply<mpsc::Receiver<Vec<HeadChange>>>,
    },
    ChainId {
        reply: Reply<u64>,
    },
    Accounts {
        reply: Reply<Vec<Address>>,
    },
    SubmitRawTransaction {
        raw: Bytes,
        reply: Reply<B256>,
    },
    Call {
        request: CallRequest,
        selector: BlockSelector,
        reply: Reply<Bytes>,
    },
    EstimateGas {
        request: CallRequest,
        selector: BlockSelector,
        reply: Reply<u64>,
    },
    BlockNumber {
        reply: Reply<u64>,
    },
    TransactionByHash {
        hash: B256,
        reply: Reply<Option<Transaction>>,
    },
    TransactionReceipt {
        hash: B256,
        reply: Reply<Option<TransactionReceipt>>,
    },
    BlockByNumber {
        selector: BlockSelector,
        reply: Reply<Option<Block>>,
    },
    Balance {
        address: Address,
        selector: BlockSelector,
        reply: Reply<U256>,
    },
    TransactionCount {
        address: Address,
        selector: BlockSelector,
        reply: Reply<u64>,
    },
    Code {
        address: Address,
        selector: BlockSelector,
        reply: Reply<Bytes>,
    },
    Storage {
        address: Address,
        slot: U256,
        selector: BlockSelector,
        reply: Reply<B256>,
    },
    SetBalance {
        address: Address,
        balance: U256,
        reply: Reply<()>,
    },
    AddBalance {
        address: Address,
        amount: U256,
        reply: Reply<()>,
    },
    SetNonce {
        address: Address,
        nonce: U256,
        reply: Reply<()>,
    },
    SetCode {
        address: Address,
        code: Bytes,
        reply: Reply<()>,
    },
    SetStorage {
        address: Address,
        slot: U256,
        value: B256,
        reply: Reply<()>,
    },
    DealErc20 {
        account: Address,
        token: Address,
        balance: U256,
        reply: Reply<()>,
    },
    SetErc20Allowance {
        owner: Address,
        spender: Address,
        token: Address,
        amount: U256,
        reply: Reply<()>,
    },
    Logs {
        filter: LogFilter,
        reply: Reply<Vec<Log>>,
    },
    MineBlocks {
        count: u64,
        interval: Option<u64>,
        timestamp: Option<u64>,
        reply: Reply<Vec<Block>>,
    },
    Automine {
        reply: Reply<bool>,
    },
    StartupMiningModeChanged {
        reply: Reply<bool>,
    },
    SetAutomine {
        enabled: bool,
        reply: Reply<()>,
    },
    Snapshot {
        reply: Reply<U256>,
    },
    Revert {
        id: U256,
        reply: Reply<bool>,
    },
    FilecoinGasBreakdown {
        hash: B256,
        reply: Reply<Option<FilecoinGasBreakdown>>,
    },
    FilecoinGasTrace {
        hash: B256,
        reply: Reply<Option<FilecoinGasTrace>>,
    },
    FilecoinStateRoots {
        reply: Reply<FilecoinStateRoots>,
    },
}

type Reply<T> = oneshot::Sender<Result<T, NodeError>>;

fn run_worker(
    config: NodeConfig,
    mut commands: mpsc::Receiver<WorkerCommand>,
    ready: std_mpsc::SyncSender<Result<NodeStartupInfo, NodeError>>,
) {
    let mut runtime = match NodeRuntime::new(config) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let startup_info = NodeStartupInfo {
        actor_bundle_root: runtime.actor_bundle_root(),
        state_roots: runtime.state_roots(),
        initial_block_number: runtime.block_number(),
        initial_timestamp: runtime.initial_timestamp(),
        base_fee_per_gas: runtime.base_fee_per_gas(),
    };
    if ready.send(Ok(startup_info)).is_err() {
        return;
    }

    while let Some(command) = commands.blocking_recv() {
        match command {
            WorkerCommand::NativeTipset { key, reply } => {
                let _ = reply.send(runtime.native_tipset(&key));
            }
            WorkerCommand::NativeTipsetByHeight { height, key, reply } => {
                let _ = reply.send(runtime.native_tipset_by_height(height, &key));
            }
            WorkerCommand::BeaconDigest { epoch, key, reply } => {
                let _ = reply.send(runtime.beacon_digest(epoch, &key));
            }
            WorkerCommand::SubscribeHeads { reply } => {
                let _ = reply.send(Ok(runtime.subscribe_heads()));
            }
            WorkerCommand::ChainId { reply } => {
                let _ = reply.send(Ok(runtime.chain_id()));
            }
            WorkerCommand::Accounts { reply } => {
                let _ = reply.send(Ok(runtime.accounts()));
            }
            WorkerCommand::SubmitRawTransaction { raw, reply } => {
                let _ = reply.send(runtime.submit_raw_transaction(raw));
            }
            WorkerCommand::Call { request, selector, reply } => {
                let _ = reply.send(runtime.call(request, selector));
            }
            WorkerCommand::EstimateGas { request, selector, reply } => {
                let _ = reply.send(runtime.estimate_gas(request, selector));
            }
            WorkerCommand::BlockNumber { reply } => {
                let _ = reply.send(Ok(runtime.block_number()));
            }
            WorkerCommand::TransactionByHash { hash, reply } => {
                let _ = reply.send(Ok(runtime.transaction_by_hash(hash)));
            }
            WorkerCommand::TransactionReceipt { hash, reply } => {
                let _ = reply.send(Ok(runtime.transaction_receipt(hash)));
            }
            WorkerCommand::BlockByNumber { selector, reply } => {
                let _ = reply.send(runtime.block_by_number(selector));
            }
            WorkerCommand::Balance { address, selector, reply } => {
                let _ = reply.send(runtime.balance(address, selector));
            }
            WorkerCommand::TransactionCount { address, selector, reply } => {
                let _ = reply.send(runtime.transaction_count(address, selector));
            }
            WorkerCommand::Code { address, selector, reply } => {
                let _ = reply.send(runtime.code(address, selector));
            }
            WorkerCommand::Storage { address, slot, selector, reply } => {
                let _ = reply.send(runtime.storage(address, slot, selector));
            }
            WorkerCommand::SetBalance { address, balance, reply } => {
                let _ = reply.send(runtime.set_balance(address, balance));
            }
            WorkerCommand::AddBalance { address, amount, reply } => {
                let _ = reply.send(runtime.add_balance(address, amount));
            }
            WorkerCommand::SetNonce { address, nonce, reply } => {
                let _ = reply.send(runtime.set_nonce(address, nonce));
            }
            WorkerCommand::SetCode { address, code, reply } => {
                let _ = reply.send(runtime.set_code(address, code));
            }
            WorkerCommand::SetStorage { address, slot, value, reply } => {
                let _ = reply.send(runtime.set_storage(address, slot, value));
            }
            WorkerCommand::DealErc20 { account, token, balance, reply } => {
                let _ = reply.send(runtime.deal_erc20(account, token, balance));
            }
            WorkerCommand::SetErc20Allowance { owner, spender, token, amount, reply } => {
                let _ = reply.send(runtime.set_erc20_allowance(owner, spender, token, amount));
            }
            WorkerCommand::Logs { filter, reply } => {
                let _ = reply.send(runtime.logs(filter));
            }
            WorkerCommand::MineBlocks { count, interval, timestamp, reply } => {
                let _ = reply.send(runtime.mine_blocks(count, interval, timestamp));
            }
            WorkerCommand::Automine { reply } => {
                let _ = reply.send(Ok(runtime.automine()));
            }
            WorkerCommand::StartupMiningModeChanged { reply } => {
                let _ = reply.send(Ok(runtime.startup_mining_mode_changed()));
            }
            WorkerCommand::SetAutomine { enabled, reply } => {
                let _ = reply.send(runtime.set_automine(enabled));
            }
            WorkerCommand::Snapshot { reply } => {
                let _ = reply.send(Ok(runtime.snapshot()));
            }
            WorkerCommand::Revert { id, reply } => {
                let _ = reply.send(Ok(runtime.revert(id)));
            }
            WorkerCommand::FilecoinGasBreakdown { hash, reply } => {
                let _ = reply.send(Ok(runtime.filecoin_gas_breakdown(hash)));
            }
            WorkerCommand::FilecoinGasTrace { hash, reply } => {
                let _ = reply.send(Ok(runtime.filecoin_gas_trace(hash)));
            }
            WorkerCommand::FilecoinStateRoots { reply } => {
                let _ = reply.send(Ok(runtime.state_roots()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::TxKind;
    use alloy_primitives::{Address, B256, Bytes, U256, hex};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    use super::NodeService;
    use crate::config::{
        ActorBundle, DEFAULT_CHAIN_ID, DEFAULT_PREFUNDED_ETH_ADDRESS as PREFUNDED_ETH_ADDRESS,
        NodeConfig,
    };
    use crate::error::NodeError;
    use crate::types::{BlockSelector, CallRequest, FilecoinCallTrace, LogFilter};

    #[test]
    fn startup_reports_bundle_failure_before_returning_service() {
        let missing = PathBuf::from("target/fvm-anvil-node/missing-v18-bundle.car");
        let config =
            NodeConfig::new(DEFAULT_CHAIN_ID, ActorBundle::local(missing).unwrap()).unwrap();

        assert!(matches!(NodeService::start(config), Err(NodeError::Bundle(_))));
    }

    const CREATION_NONCE_0: &[u8] = &hex!(
        "02f863827a698064843b9aca00843b9aca0080808d6001600c60003960016000f300c001a0d8396ba73ed5c89c8746f03f029f018957d171440450065a74bd156c41385556a05a951b6475392aa73d7dfc719a332f2ffd971c6f92a8cc818de681cc31f63850"
    );
    const CREATION_NONCE_1: &[u8] = &hex!(
        "02f863827a690164843b9aca00843b9aca0080808d6001600c60003960016000f300c001a07a8a0e4d5fe82c4cc903a266f9d10055d432a61c2b7c3fe9374a06a9ee0c6d95a0794d22de3b85c89806c1945386afabdb70000745c7726d746ec00b67404d5b25"
    );

    fn real_service() -> NodeService {
        let path = std::env::var_os("FVM_ANVIL_ACTOR_BUNDLE")
            .expect("set FVM_ANVIL_ACTOR_BUNDLE to the builtin-actors v18 mainnet CAR");
        let config = NodeConfig::new(DEFAULT_CHAIN_ID, ActorBundle::local(path).unwrap()).unwrap();
        NodeService::start(config).unwrap()
    }

    fn call_trace_counts(calls: &[FilecoinCallTrace]) -> (usize, usize) {
        calls.iter().fold((0, 0), |(call_count, charge_count), call| {
            let (subcall_count, subcall_charge_count) = call_trace_counts(&call.subcalls);
            (
                call_count + 1 + subcall_count,
                charge_count + call.gas_charges.len() + subcall_charge_count,
            )
        })
    }

    fn signed_transfer(nonce: u64, gas_limit: u64, cap: u128, premium: u128) -> Bytes {
        let signer: PrivateKeySigner =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap();
        let transaction = TxEip1559 {
            chain_id: DEFAULT_CHAIN_ID,
            nonce,
            gas_limit,
            max_fee_per_gas: cap,
            max_priority_fee_per_gas: premium,
            to: TxKind::Call(PREFUNDED_ETH_ADDRESS),
            ..Default::default()
        };
        let signature = signer.sign_hash_sync(&transaction.signature_hash()).unwrap();
        transaction.into_signed(signature).encoded_2718().into()
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn replacement_reservations_identity_and_snapshot_ownership() {
        let service = real_service();
        let node = service.handle();
        node.set_automine(false).await.unwrap();
        let old = node
            .submit_raw_transaction(signed_transfer(0, 6_000_000_000, 1000, 100))
            .await
            .unwrap();
        let successor = node
            .submit_raw_transaction(signed_transfer(1, 4_000_000_000, 1000, 100))
            .await
            .unwrap();
        assert!(matches!(
            node.submit_raw_transaction(signed_transfer(0, 6_000_000_000, 1100, 109)).await,
            Err(NodeError::ReplacementUnderpriced)
        ));
        assert!(matches!(
            node.submit_raw_transaction(signed_transfer(0, 6_000_000_001, 1100, 110)).await,
            Err(NodeError::PendingBlockGasLimit { .. })
        ));
        node.set_balance(PREFUNDED_ETH_ADDRESS, U256::from(10_000_000_000_000_u64)).await.unwrap();
        assert!(matches!(
            node.submit_raw_transaction(signed_transfer(0, 6_000_000_000, 1100, 110)).await,
            Err(NodeError::InsufficientFunds { .. })
        ));
        assert!(node.transaction_by_hash(old).await.unwrap().is_some());
        node.set_balance(PREFUNDED_ETH_ADDRESS, U256::from(10_600_000_000_000_u64)).await.unwrap();
        let replacement = node
            .submit_raw_transaction(signed_transfer(0, 6_000_000_000, 1100, 110))
            .await
            .unwrap();
        assert!(node.transaction_by_hash(old).await.unwrap().is_none());
        assert_eq!(node.transaction_by_hash(replacement).await.unwrap().unwrap().block_hash, None);
        assert_eq!(
            node.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Pending).await.unwrap(),
            2
        );
        let snapshot = node.snapshot().await.unwrap();
        let blocks = node.mine_blocks(1, None, None).await.unwrap();
        assert_eq!(
            blocks[0].transactions.iter().map(|tx| tx.hash).collect::<Vec<_>>(),
            vec![replacement, successor]
        );
        assert!(node.revert(snapshot).await.unwrap());
        assert!(node.transaction_by_hash(replacement).await.unwrap().is_none());
        assert!(node.transaction_receipt(replacement).await.unwrap().is_none());
        assert_eq!(
            node.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Pending).await.unwrap(),
            0
        );
        // A mined-away pool entry is not resurrected by reverting canonical state.
        node.submit_raw_transaction(signed_transfer(0, 6_000_000_000, 1100, 110)).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn native_history_notifications_and_raw_beacon_precompile() {
        let service = real_service();
        let node = service.handle();
        let mut heads = node.subscribe_heads().await.unwrap();
        let genesis = node.native_tipset(Vec::new()).await.unwrap();
        assert_eq!(heads.recv().await.unwrap(), vec![crate::HeadChange::Current(genesis.clone())]);
        let snapshot = node.snapshot().await.unwrap();
        node.mine_blocks(3, None, None).await.unwrap();
        let changes = heads.recv().await.unwrap();
        assert_eq!(changes.len(), 3);
        let first = node.native_tipset_by_height(1, Vec::new()).await.unwrap();
        assert_eq!(first.parents, vec![genesis.cid]);
        assert!(matches!(&changes[0], crate::HeadChange::Apply(head) if head == &first));
        assert!(node.beacon_digest(2, vec![first.cid]).await.is_err());
        let digest = node.beacon_digest(2, Vec::new()).await.unwrap();
        let address = Address::repeat_byte(0x77);
        // Copy the epoch from calldata and STATICCALL the FIP-0095 beacon precompile.
        node.set_code(address, Bytes::from_static(&hex!(
            "600035600052602060006020600073fe000000000000000000000000000000000000065afa5060206000f3"
        ))).await.unwrap();
        let output = node
            .call(
                CallRequest {
                    to: Some(address),
                    input: Bytes::copy_from_slice(&U256::from(2).to_be_bytes::<32>()),
                    ..CallRequest::default()
                },
                BlockSelector::Latest,
            )
            .await
            .unwrap();
        assert_eq!(output.as_ref(), digest.as_slice());
        assert!(node.revert(snapshot).await.unwrap());
        let changes = heads.recv().await.unwrap();
        assert_eq!(changes.len(), 4);
        assert!(matches!(&changes[0], crate::HeadChange::Revert(head) if head.height == 3));
        assert!(matches!(&changes[2], crate::HeadChange::Revert(head) if head.height == 1));
        assert_eq!(changes[3], crate::HeadChange::Apply(genesis.clone()));
        assert!(node.native_tipset(vec![first.cid]).await.is_err());
        drop(heads);
        let mut reconnected = node.subscribe_heads().await.unwrap();
        assert_eq!(reconnected.recv().await.unwrap(), vec![crate::HeadChange::Current(genesis)]);
        // Backpressure closes a lagging subscription rather than blocking the worker.
        for _ in 0..65 {
            node.mine_blocks(1, None, None).await.unwrap();
        }
        let mut buffered = 0;
        while reconnected.recv().await.is_some() {
            buffered += 1;
        }
        assert_eq!(buffered, 64);
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn native_parent_state_uses_direct_override_execution_root() {
        let service = real_service();
        let node = service.handle();
        let original = node.filecoin_state_roots().await.unwrap().current;
        let account = node.accounts().await.unwrap()[0];
        node.set_balance(account, U256::from(123_456)).await.unwrap();
        let overridden = node.filecoin_state_roots().await.unwrap().current;
        assert_ne!(original, overridden);
        node.mine_blocks(2, None, None).await.unwrap();
        let first = node.native_tipset_by_height(1, Vec::new()).await.unwrap();
        let second = node.native_tipset_by_height(2, Vec::new()).await.unwrap();
        assert_eq!(first.parent_state_root, overridden);
        assert_eq!(second.parent_state_root, overridden);
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn instant_mining_commits_exactly_one_transaction_block() {
        let service = real_service();
        let handle = service.handle();
        assert_eq!(handle.block_number().await.unwrap(), 0);

        let hash =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_0)).await.unwrap();

        assert_eq!(handle.block_number().await.unwrap(), 1);
        let block = handle.block_by_number(BlockSelector::Latest).await.unwrap().unwrap();
        assert_eq!(block.transactions.len(), 1);
        assert_eq!(block.transactions[0].hash, hash);
        let receipt = handle.transaction_receipt(hash).await.unwrap().unwrap();
        assert!(
            receipt.status,
            "creation failed: exit_code={}, output={:?}",
            receipt.exit_code, receipt.output
        );
        assert_eq!(receipt.block_hash, block.hash);
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Latest).await.unwrap(),
            1
        );
        assert!(handle.filecoin_gas_breakdown(hash).await.unwrap().is_some());
        assert_eq!(block.logs, receipt.logs);
        assert_eq!(block.transactions[0].effective_gas_price, Some(receipt.effective_gas_price));
        let transaction = &block.transactions[0];
        assert_eq!(
            receipt.effective_gas_price,
            U256::from(transaction.max_fee_per_gas).min(
                U256::from(block.base_fee_per_gas)
                    + U256::from(transaction.max_priority_fee_per_gas)
            )
        );
        let trace = handle.filecoin_gas_trace(hash).await.unwrap().unwrap();
        assert_eq!(trace.gas_used, receipt.gas_used);
        assert_eq!(trace.exit_code, receipt.exit_code);
        assert_eq!(trace.failure_info, None);
        let (call_count, call_charge_count) = call_trace_counts(&trace.calls);
        assert!(call_count > 1, "contract creation should contain nested actor calls");
        assert!(
            trace.gas_charges.len() + call_charge_count > 0,
            "a traced transaction should retain its gas charges"
        );
        assert_eq!(
            handle.filecoin_gas_trace(B256::ZERO).await.unwrap(),
            None,
            "unknown transaction hashes do not have traces"
        );
        let contract = receipt.contract_address.unwrap();
        assert_eq!(
            handle.storage(contract, U256::ZERO, BlockSelector::Latest).await.unwrap(),
            B256::ZERO
        );
        assert_eq!(
            handle.storage(Address::ZERO, U256::ZERO, BlockSelector::Earliest).await.unwrap(),
            B256::ZERO
        );
        assert_eq!(
            handle
                .logs(LogFilter { block_hash: Some(block.hash), ..LogFilter::default() })
                .await
                .unwrap(),
            block.logs
        );
        assert!(matches!(
            handle
                .logs(LogFilter {
                    block_hash: Some(block.hash),
                    from_block: Some(BlockSelector::Earliest),
                    ..LogFilter::default()
                })
                .await,
            Err(NodeError::ConflictingLogFilterBlocks)
        ));
        assert_eq!(handle.block_number().await.unwrap(), 1);
        assert_eq!(handle.block_by_number(BlockSelector::Latest).await.unwrap().unwrap(), block);
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn manual_mining_is_fifo_and_simulations_do_not_publish() {
        let service = real_service();
        let handle = service.handle();
        handle.set_automine(false).await.unwrap();
        let first =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_0)).await.unwrap();
        let second =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_1)).await.unwrap();
        assert_eq!(handle.block_number().await.unwrap(), 0);
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Pending).await.unwrap(),
            2
        );

        assert!(matches!(
            handle.mine_blocks(0, None, None).await,
            Err(NodeError::InvalidMineCount(0))
        ));
        assert!(matches!(
            handle.mine_blocks(u64::MAX, None, None).await,
            Err(NodeError::MineArithmeticOverflow { .. })
        ));
        assert!(matches!(
            handle.mine_blocks(2, Some(u64::MAX), None).await,
            Err(NodeError::MineArithmeticOverflow { .. })
        ));
        assert_eq!(handle.block_number().await.unwrap(), 0);

        let mined_blocks = handle.mine_blocks(3, Some(7), Some(10)).await.unwrap();
        let mined = &mined_blocks[0];
        assert_eq!(
            mined.transactions.iter().map(|transaction| transaction.hash).collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(
            mined_blocks.iter().map(|block| block.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            mined_blocks.iter().map(|block| block.timestamp).collect::<Vec<_>>(),
            vec![10, 17, 24]
        );
        assert!(mined_blocks[1..].iter().all(|block| block.transactions.is_empty()));
        assert!(mined_blocks[1..].iter().all(|block| block.logs.is_empty()));
        assert!(mined_blocks.windows(2).all(|blocks| blocks[1].parent_hash == blocks[0].hash));
        let first_receipt = handle.transaction_receipt(first).await.unwrap().unwrap();
        assert_eq!(first_receipt.transaction_index, 0);
        assert_eq!(handle.transaction_receipt(second).await.unwrap().unwrap().transaction_index, 1);
        let contract = first_receipt.contract_address.unwrap();
        assert_eq!(handle.code(contract, BlockSelector::Latest).await.unwrap().as_ref(), &[0]);
        assert_eq!(handle.code(contract, BlockSelector::Pending).await.unwrap().as_ref(), &[0]);
        assert!(handle.code(contract, BlockSelector::Earliest).await.unwrap().is_empty());
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Earliest).await.unwrap(),
            0
        );
        let before = handle.block_by_number(BlockSelector::Latest).await.unwrap().unwrap();
        let request =
            CallRequest { from: Some(Address::ZERO), to: Some(contract), ..CallRequest::default() };
        assert!(handle.call(request.clone(), BlockSelector::Pending).await.is_ok());
        assert!(handle.call(request.clone(), BlockSelector::Pending).await.is_ok());
        assert!(handle.estimate_gas(request.clone(), BlockSelector::Pending).await.is_ok());
        let failing = CallRequest { gas_limit: Some(1), ..request };
        let failure = handle.call(failing, BlockSelector::Pending).await.unwrap_err();
        assert!(matches!(
            failure,
            NodeError::CallFailed { exit_code, detail, data: _ }
                if exit_code != 0 && !detail.is_empty()
        ));
        assert_eq!(handle.block_by_number(BlockSelector::Latest).await.unwrap().unwrap(), before);
        assert_eq!(handle.block_number().await.unwrap(), 3);
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn snapshot_revert_keeps_current_controls_and_revalidates_only_current_pool() {
        let service = real_service();
        let handle = service.handle();
        let genesis = handle.block_by_number(BlockSelector::Latest).await.unwrap().unwrap();
        let snapshot = handle.snapshot().await.unwrap();

        handle.set_automine(false).await.unwrap();
        let first =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_0)).await.unwrap();
        handle.mine_blocks(1, None, None).await.unwrap();
        let second =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_1)).await.unwrap();
        assert!(handle.transaction_receipt(first).await.unwrap().is_some());
        assert!(handle.transaction_by_hash(second).await.unwrap().is_some());

        assert!(handle.revert(snapshot).await.unwrap());
        assert!(!handle.revert(snapshot).await.unwrap());
        assert!(!handle.automine().await.unwrap());
        assert_eq!(handle.block_number().await.unwrap(), 0);
        assert_eq!(
            handle.block_by_number(BlockSelector::Latest).await.unwrap().unwrap().state_root,
            genesis.state_root
        );
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Latest).await.unwrap(),
            0
        );
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Pending).await.unwrap(),
            0
        );
        assert!(handle.transaction_receipt(first).await.unwrap().is_none());
        assert!(handle.transaction_receipt(second).await.unwrap().is_none());
        assert!(handle.transaction_by_hash(first).await.unwrap().is_none());
        assert!(handle.transaction_by_hash(second).await.unwrap().is_none());
        assert!(handle.block_by_number(BlockSelector::Number(1)).await.unwrap().is_none());

        let current_pool_snapshot = handle.snapshot().await.unwrap();
        let restored_first =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_0)).await.unwrap();
        let restored_second =
            handle.submit_raw_transaction(Bytes::copy_from_slice(CREATION_NONCE_1)).await.unwrap();
        assert!(handle.revert(current_pool_snapshot).await.unwrap());
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Pending).await.unwrap(),
            2
        );
        assert!(handle.transaction_by_hash(restored_first).await.unwrap().is_some());
        assert!(handle.transaction_by_hash(restored_second).await.unwrap().is_some());

        let snapshot_with_pool = handle.snapshot().await.unwrap();
        let mined = handle.mine_blocks(1, None, None).await.unwrap();
        assert_eq!(
            mined[0].transactions.iter().map(|transaction| transaction.hash).collect::<Vec<_>>(),
            vec![restored_first, restored_second]
        );
        handle.set_automine(true).await.unwrap();
        assert!(handle.transaction_receipt(restored_first).await.unwrap().is_some());
        assert!(handle.transaction_receipt(restored_second).await.unwrap().is_some());

        assert!(handle.revert(snapshot_with_pool).await.unwrap());
        assert!(handle.automine().await.unwrap());
        assert_eq!(
            handle.transaction_count(PREFUNDED_ETH_ADDRESS, BlockSelector::Pending).await.unwrap(),
            0
        );
        assert!(handle.transaction_by_hash(restored_first).await.unwrap().is_none());
        assert!(handle.transaction_by_hash(restored_second).await.unwrap().is_none());
        assert!(handle.transaction_receipt(restored_first).await.unwrap().is_none());
        assert!(handle.transaction_receipt(restored_second).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires FVM_ANVIL_ACTOR_BUNDLE to point to the official builtin-actors v18 mainnet CAR"]
    async fn direct_state_modifications_are_evm_visible_and_snapshot_aware() {
        let service = real_service();
        let handle = service.handle();
        let target = Address::from([0x11; 20]);
        let snapshot = handle.snapshot().await.unwrap();
        let initial_root = handle.filecoin_state_roots().await.unwrap().current;

        handle.set_balance(target, U256::from(10)).await.unwrap();
        handle.add_balance(target, U256::MAX).await.unwrap();
        assert_eq!(handle.balance(target, BlockSelector::Latest).await.unwrap(), U256::MAX);

        let code = Bytes::from_static(&hex!("60005460005260206000f3"));
        handle.set_code(target, code.clone()).await.unwrap();
        handle.set_nonce(target, U256::from(7)).await.unwrap();
        let stored = B256::from(U256::from(42));
        handle.set_storage(target, U256::ZERO, stored).await.unwrap();

        assert_eq!(handle.code(target, BlockSelector::Latest).await.unwrap(), code);
        assert_eq!(handle.transaction_count(target, BlockSelector::Latest).await.unwrap(), 7);
        assert_eq!(handle.transaction_count(target, BlockSelector::Pending).await.unwrap(), 7);
        assert_eq!(
            handle.storage(target, U256::ZERO, BlockSelector::Latest).await.unwrap(),
            stored
        );
        let output = handle
            .call(CallRequest { to: Some(target), ..CallRequest::default() }, BlockSelector::Latest)
            .await
            .unwrap();
        assert_eq!(output.as_ref(), stored.as_slice());
        assert_ne!(handle.filecoin_state_roots().await.unwrap().current, initial_root);
        assert!(handle.code(target, BlockSelector::Earliest).await.unwrap().is_empty());

        assert!(handle.revert(snapshot).await.unwrap());
        assert_eq!(handle.filecoin_state_roots().await.unwrap().current, initial_root);
        assert_eq!(handle.balance(target, BlockSelector::Latest).await.unwrap(), U256::ZERO);
        assert!(handle.code(target, BlockSelector::Latest).await.unwrap().is_empty());
        assert_eq!(
            handle.storage(target, U256::ZERO, BlockSelector::Latest).await.unwrap(),
            B256::ZERO
        );
    }
}
