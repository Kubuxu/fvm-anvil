use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use alloy_primitives::{Address, B256, Bytes, U256};
use cid::Cid;
use tokio::sync::{mpsc, oneshot};

use crate::config::NodeConfig;
use crate::error::NodeError;
use crate::runtime::NodeRuntime;
use crate::types::{
    Block, BlockSelector, CallRequest, FilecoinGasBreakdown, Log, LogFilter, Transaction,
    TransactionReceipt,
};

const COMMAND_CAPACITY: usize = 64;
const WORKER_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Cloneable asynchronous access to the worker-owned node state.
#[derive(Clone)]
pub struct NodeHandle {
    commands: mpsc::Sender<WorkerCommand>,
}

impl NodeHandle {
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
}

impl NodeStartupInfo {
    pub const fn actor_bundle_root(&self) -> Cid {
        self.actor_bundle_root
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
    let startup_info = NodeStartupInfo { actor_bundle_root: runtime.actor_bundle_root() };
    if ready.send(Ok(startup_info)).is_err() {
        return;
    }

    while let Some(command) = commands.blocking_recv() {
        match command {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alloy_primitives::{Address, B256, Bytes, U256, hex};

    use super::NodeService;
    use crate::config::{
        ActorBundle, DEFAULT_CHAIN_ID, DEFAULT_PREFUNDED_ETH_ADDRESS as PREFUNDED_ETH_ADDRESS,
        NodeConfig,
    };
    use crate::error::NodeError;
    use crate::types::{BlockSelector, CallRequest, LogFilter};

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
        let gas = handle.filecoin_gas_breakdown(hash).await.unwrap().unwrap();
        assert_eq!(block.logs, receipt.logs);
        assert_eq!(block.transactions[0].effective_gas_price, Some(receipt.effective_gas_price));
        assert_eq!(
            receipt.effective_gas_price,
            (gas.base_fee_burn + gas.over_estimation_burn + gas.miner_tip)
                / U256::from(receipt.gas_used)
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
}
