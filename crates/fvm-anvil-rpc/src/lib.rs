//! Anvil-compatible JSON-RPC parsing, dispatch, and transport for fvm-anvil.
//!
//! RPC handlers communicate with the node exclusively through its semantic handle.

mod response;
mod wire;

use alloy_primitives::U256;
use anvil_rpc::{
    error::{ErrorCode, RpcError},
    request::{RequestParams, RpcMethodCall},
    response::{ResponseResult, RpcResponse},
};
use anvil_server::RpcHandler;
use async_trait::async_trait;
use fvm_anvil_node::{BlockSelector, LogFilter, NodeError, NodeHandle};
use serde_json::{Value, json};

pub use anvil_server::{ServerConfig, http_router};
pub use wire::{
    AccessListItem, AddressBlockParams, AnvilMineParams, BlockParams, CallParams, EvmMineParams,
    FeeHistoryParams, HashBlockParams, LogFilterParams, One, QuantityU64, QuantityU256, RpcRequest,
    StorageParams, StrictBytes, TransactionRequest,
};

/// The client identifier returned by `web3_clientVersion`.
pub const CLIENT_VERSION: &str = concat!("fvm-anvil/v", env!("CARGO_PKG_VERSION"));

/// A cloneable JSON-RPC service backed by the node's semantic handle.
#[derive(Clone)]
pub struct RpcService {
    node: NodeHandle,
}

impl RpcService {
    /// Creates a service for a running node.
    pub const fn new(node: NodeHandle) -> Self {
        Self { node }
    }

    async fn fee_history(&self, params: FeeHistoryParams) -> ResponseResult {
        let latest = match self.node.block_number().await {
            Ok(latest) => latest,
            Err(error) => return node_error(error),
        };
        let newest = match params.newest_block {
            BlockSelector::Latest | BlockSelector::Pending => latest,
            BlockSelector::Earliest => 0,
            BlockSelector::Number(number) if number <= latest => number,
            BlockSelector::Number(number) => {
                return invalid_params(format!("canonical block number {number} does not exist"));
            }
            BlockSelector::Hash(_) => unreachable!("fee history parser rejects block hashes"),
        };
        let oldest = newest.saturating_sub(params.block_count.saturating_sub(1));
        let mut blocks = Vec::with_capacity(
            usize::try_from(newest - oldest + 1).expect("fee history is limited to 1024 blocks"),
        );
        for number in oldest..=newest {
            let block = match self.node.block_by_number(BlockSelector::Number(number)).await {
                Ok(Some(block)) => block,
                Ok(None) => {
                    return invalid_params(format!(
                        "canonical block number {number} does not exist"
                    ));
                }
                Err(error) => return node_error(error),
            };
            let mut transaction_gas_used = Vec::with_capacity(block.transactions.len());
            for transaction in &block.transactions {
                match self.node.transaction_receipt(transaction.hash).await {
                    Ok(Some(receipt)) => transaction_gas_used.push(receipt.gas_used),
                    Ok(None) => {
                        return internal_error(format!(
                            "canonical transaction {} is missing its receipt",
                            transaction.hash
                        ));
                    }
                    Err(error) => return node_error(error),
                }
            }
            blocks.push(response::FeeHistoryBlock { block, transaction_gas_used });
        }

        let next_base_fee = if newest < latest {
            match self.node.block_by_number(BlockSelector::Number(newest + 1)).await {
                Ok(Some(block)) => U256::from(block.base_fee_per_gas),
                Ok(None) => {
                    return internal_error(format!(
                        "canonical block number {} does not exist",
                        newest + 1
                    ));
                }
                Err(error) => return node_error(error),
            }
        } else {
            let Some(block) = blocks.last() else {
                return internal_error("fee history selected no canonical blocks");
            };
            U256::from(block.block.base_fee_per_gas)
        };
        match response::fee_history(blocks, next_base_fee, &params.reward_percentiles) {
            Ok(history) => ResponseResult::success(history),
            Err(error) => internal_error(error),
        }
    }

    async fn dispatch(&self, request: RpcRequest) -> ResponseResult {
        if let Some(value) = static_response(&request) {
            return ResponseResult::success(value);
        }

        match request {
            RpcRequest::NetVersion { .. } => match self.node.chain_id().await {
                Ok(chain_id) => ResponseResult::success(chain_id.to_string()),
                Err(error) => node_error(error),
            },
            RpcRequest::EthChainId { .. } => match self.node.chain_id().await {
                Ok(chain_id) => ResponseResult::success(response::quantity(chain_id)),
                Err(error) => node_error(error),
            },
            RpcRequest::EthAccounts { .. } => match self.node.accounts().await {
                Ok(accounts) => ResponseResult::success(accounts),
                Err(error) => node_error(error),
            },
            RpcRequest::EthBlockNumber { .. } => match self.node.block_number().await {
                Ok(number) => ResponseResult::success(response::quantity(number)),
                Err(error) => node_error(error),
            },
            RpcRequest::EthGetBalance { params } => {
                match self.node.balance(params.address, params.block).await {
                    Ok(balance) => ResponseResult::success(response::quantity(balance)),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetTransactionCount { params } => {
                match self.node.transaction_count(params.address, params.block).await {
                    Ok(nonce) => ResponseResult::success(response::quantity(nonce)),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetCode { params } => {
                match self.node.code(params.address, params.block).await {
                    Ok(code) => ResponseResult::success(code),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetStorageAt { params } => {
                match self.node.storage(params.address, params.slot, params.block).await {
                    Ok(value) => ResponseResult::success(value),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthCall { params } => {
                let call = match params.request.into_node() {
                    Ok(call) => call,
                    Err(error) => return invalid_params(error),
                };
                match self.node.call(call, params.block).await {
                    Ok(output) => ResponseResult::success(output),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthEstimateGas { params } => {
                let call = match params.request.into_node() {
                    Ok(call) => call,
                    Err(error) => return invalid_params(error),
                };
                match self.node.estimate_gas(call, params.block).await {
                    Ok(gas) => ResponseResult::success(response::quantity(gas)),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthSendRawTransaction { params } => {
                match self.node.submit_raw_transaction(params.0.0).await {
                    Ok(hash) => ResponseResult::success(hash),
                    Err(error) => transaction_error(error),
                }
            }
            RpcRequest::EthGetTransactionByHash { params } => {
                match self.node.transaction_by_hash(params.0).await {
                    Ok(Some(transaction)) => match response::transaction(transaction) {
                        Ok(transaction) => ResponseResult::success(transaction),
                        Err(error) => internal_error(error),
                    },
                    Ok(None) => ResponseResult::success(Value::Null),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetTransactionReceipt { params } => {
                match self.node.transaction_receipt(params.0).await {
                    Ok(Some(receipt)) => match response::receipt(receipt) {
                        Ok(receipt) => ResponseResult::success(receipt),
                        Err(error) => internal_error(error),
                    },
                    Ok(None) => ResponseResult::success(Value::Null),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetBlockByNumber { params } => {
                match self.node.block_by_number(params.block).await {
                    Ok(block) => match response::optional_block(block, params.full_transactions) {
                        Ok(block) => ResponseResult::success(block),
                        Err(error) => internal_error(error),
                    },
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetBlockByHash { params } => {
                match self.node.block_by_number(BlockSelector::Hash(params.hash)).await {
                    Ok(block) => match response::optional_block(block, params.full_transactions) {
                        Ok(block) => ResponseResult::success(block),
                        Err(error) => internal_error(error),
                    },
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGetLogs { params } => {
                let params = params.0;
                let filter = LogFilter {
                    block_hash: params.block_hash,
                    from_block: params.from_block,
                    to_block: params.to_block,
                    addresses: params.addresses,
                    topics: params.topics,
                };
                match self.node.logs(filter).await {
                    Ok(logs) => match response::logs(logs) {
                        Ok(logs) => ResponseResult::success(logs),
                        Err(error) => internal_error(error),
                    },
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthGasPrice { .. } => {
                match self.node.block_by_number(BlockSelector::Latest).await {
                    Ok(Some(block)) => ResponseResult::success(response::quantity(
                        response::gas_price(block.base_fee_per_gas),
                    )),
                    Ok(None) => internal_error("canonical latest block does not exist"),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EthFeeHistory { params } => self.fee_history(params).await,
            RpcRequest::EthMining { .. } => match self.node.automine().await {
                Ok(automine) => ResponseResult::success(automine),
                Err(error) => node_error(error),
            },
            RpcRequest::Snapshot { .. } => match self.node.snapshot().await {
                Ok(snapshot) => ResponseResult::success(response::quantity(snapshot)),
                Err(error) => node_error(error),
            },
            RpcRequest::Revert { params } => match self.node.revert(params.0.0).await {
                Ok(reverted) => ResponseResult::success(reverted),
                Err(error) => node_error(error),
            },
            RpcRequest::AnvilMine { params } => {
                match self.node.mine_blocks(params.count, params.interval, None).await {
                    Ok(_) => ResponseResult::success(response::anvil_mine()),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::EvmMine { params } => {
                match self.node.mine_blocks(params.count, None, params.timestamp).await {
                    Ok(_) => ResponseResult::success(response::evm_mine()),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::SetAutomine { params } => match self.node.set_automine(params.0).await {
                Ok(()) => ResponseResult::success(Value::Null),
                Err(error) => node_error(error),
            },
            RpcRequest::FilecoinGetGasBreakdown { params } => {
                match self.node.filecoin_gas_breakdown(params.0).await {
                    Ok(Some(breakdown)) => {
                        ResponseResult::success(response::gas_breakdown(breakdown))
                    }
                    Ok(None) => ResponseResult::success(Value::Null),
                    Err(error) => node_error(error),
                }
            }
            RpcRequest::Web3ClientVersion { .. }
            | RpcRequest::EthSyncing { .. }
            | RpcRequest::EthMaxPriorityFeePerGas { .. } => {
                unreachable!("static requests return before node dispatch")
            }
        }
    }
}

#[async_trait]
impl RpcHandler for RpcService {
    type Request = RpcRequest;

    async fn on_request(&self, request: Self::Request) -> ResponseResult {
        self.dispatch(request).await
    }

    async fn on_call(&self, call: RpcMethodCall) -> RpcResponse {
        let RpcMethodCall { method, params, id, .. } = call;
        let response = match parse_request(&method, params) {
            Ok(request) => self.on_request(request).await,
            Err(error) => ResponseResult::error(error),
        };
        RpcResponse::new(id, response)
    }
}

fn parse_request(method: &str, params: RequestParams) -> Result<RpcRequest, RpcError> {
    if !is_known_method(method) {
        return Err(RpcError::method_not_found());
    }

    serde_json::from_value(json!({
        "method": method,
        "params": Value::from(params),
    }))
    .map_err(|error| RpcError::invalid_params(error.to_string()))
}

fn is_known_method(method: &str) -> bool {
    matches!(
        method,
        "web3_clientVersion"
            | "net_version"
            | "eth_networkId"
            | "eth_chainId"
            | "eth_accounts"
            | "eth_requestAccounts"
            | "eth_syncing"
            | "eth_blockNumber"
            | "eth_getBalance"
            | "eth_getTransactionCount"
            | "eth_getCode"
            | "eth_getStorageAt"
            | "eth_call"
            | "eth_estimateGas"
            | "eth_sendRawTransaction"
            | "eth_getTransactionByHash"
            | "eth_getTransactionReceipt"
            | "eth_getBlockByNumber"
            | "eth_getBlockByHash"
            | "eth_getLogs"
            | "eth_gasPrice"
            | "eth_maxPriorityFeePerGas"
            | "eth_feeHistory"
            | "eth_mining"
            | "anvil_snapshot"
            | "evm_snapshot"
            | "anvil_revert"
            | "evm_revert"
            | "anvil_mine"
            | "evm_mine"
            | "anvil_setAutomine"
            | "evm_setAutomine"
            | "filecoin_getGasBreakdown"
    )
}

fn static_response(request: &RpcRequest) -> Option<Value> {
    match request {
        RpcRequest::Web3ClientVersion { .. } => Some(json!(CLIENT_VERSION)),
        RpcRequest::EthSyncing { .. } => Some(json!(false)),
        RpcRequest::EthMaxPriorityFeePerGas { .. } => {
            Some(json!(response::quantity(response::MAX_PRIORITY_FEE_PER_GAS)))
        }
        RpcRequest::NetVersion { .. }
        | RpcRequest::EthChainId { .. }
        | RpcRequest::EthAccounts { .. }
        | RpcRequest::EthBlockNumber { .. }
        | RpcRequest::EthGetBalance { .. }
        | RpcRequest::EthGetTransactionCount { .. }
        | RpcRequest::EthGetCode { .. }
        | RpcRequest::EthGetStorageAt { .. }
        | RpcRequest::EthCall { .. }
        | RpcRequest::EthEstimateGas { .. }
        | RpcRequest::EthSendRawTransaction { .. }
        | RpcRequest::EthGetTransactionByHash { .. }
        | RpcRequest::EthGetTransactionReceipt { .. }
        | RpcRequest::EthGetBlockByNumber { .. }
        | RpcRequest::EthGetBlockByHash { .. }
        | RpcRequest::EthGetLogs { .. }
        | RpcRequest::EthGasPrice { .. }
        | RpcRequest::EthFeeHistory { .. }
        | RpcRequest::EthMining { .. }
        | RpcRequest::Snapshot { .. }
        | RpcRequest::Revert { .. }
        | RpcRequest::AnvilMine { .. }
        | RpcRequest::EvmMine { .. }
        | RpcRequest::SetAutomine { .. }
        | RpcRequest::FilecoinGetGasBreakdown { .. } => None,
    }
}

fn invalid_params(error: impl Into<String>) -> ResponseResult {
    ResponseResult::error(RpcError::invalid_params(error))
}

fn node_error(error: NodeError) -> ResponseResult {
    let detail = error.to_string();
    match error {
        NodeError::UnknownBlockNumber(_)
        | NodeError::UnknownBlockHash(_)
        | NodeError::UnsupportedSelector(_)
        | NodeError::ConflictingLogFilterBlocks
        | NodeError::InvalidLogBlockRange { .. }
        | NodeError::InvalidMineCount(_)
        | NodeError::MineArithmeticOverflow { .. }
        | NodeError::MineCapacity { .. }
        | NodeError::InvalidMineTimestamp { .. }
        | NodeError::InvalidSnapshotId(_) => invalid_params(detail),
        NodeError::CallFailed { data, .. } => ResponseResult::error(RpcError {
            code: ErrorCode::ExecutionError,
            message: detail.into(),
            data: Some(json!(data)),
        }),
        _ => internal_error(detail),
    }
}

fn transaction_error(error: NodeError) -> ResponseResult {
    match error {
        validation @ (NodeError::Bridge(_)
        | NodeError::DuplicateTransaction(_)
        | NodeError::StaleNonce { .. }
        | NodeError::FutureNonce { .. }
        | NodeError::FeeCapTooLow { .. }
        | NodeError::PriorityFeeAboveCap { .. }
        | NodeError::InvalidGasLimit { .. }
        | NodeError::PendingBlockGasLimit { .. }
        | NodeError::InsufficientFunds { .. }) => {
            ResponseResult::error(RpcError::transaction_rejected(validation.to_string()))
        }
        error => node_error(error),
    }
}

fn internal_error(error: impl Into<String>) -> ResponseResult {
    ResponseResult::error(RpcError::internal_error_with(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256, address, b256, hex};
    use anvil_rpc::request::{Id, Version};
    use fvm_anvil_node::{Block, FilecoinGasBreakdown, Log, Transaction, TransactionReceipt};
    use serde_json::Map;

    const CREATION: &[u8] = &hex!(
        "02f85d827a69078405f5e10084773594008307a120802a8460006000c080a09d77b26d8db24c230a554a1466f98ac8cb4f0173bb026e7596579c27ff6487f4a05cfd5ce9321e094100bd6f57b284bd2e5168268a74db24ef81c80ca722912b80"
    );

    fn parse_json(value: Value) -> Result<RpcRequest, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn parses_every_public_method_and_alias() {
        let address = "0x0000000000000000000000000000000000000001";
        let hash = format!("{:#x}", B256::ZERO);
        let cases = [
            ("web3_clientVersion", json!([])),
            ("net_version", json!([])),
            ("eth_networkId", json!([])),
            ("eth_chainId", json!([])),
            ("eth_accounts", json!([])),
            ("eth_requestAccounts", json!([])),
            ("eth_syncing", json!([])),
            ("eth_blockNumber", json!([])),
            ("eth_getBalance", json!([address, "latest"])),
            ("eth_getTransactionCount", json!([address, "pending"])),
            ("eth_getCode", json!([address, null])),
            ("eth_getStorageAt", json!([address, "0x1", "latest"])),
            ("eth_call", json!([{ "to": address, "input": "0x" }, "latest"])),
            ("eth_estimateGas", json!([{ "from": address, "data": "0x00" }])),
            ("eth_sendRawTransaction", json!(["0x02"])),
            ("eth_getTransactionByHash", json!([hash])),
            ("eth_getTransactionReceipt", json!([hash])),
            ("eth_getBlockByNumber", json!(["latest", false])),
            ("eth_getBlockByHash", json!([hash, true])),
            ("eth_getLogs", json!([{}])),
            ("eth_gasPrice", json!([])),
            ("eth_maxPriorityFeePerGas", json!([])),
            ("eth_feeHistory", json!(["0x1", "latest", [10.0, 50.0]])),
            ("eth_mining", json!([])),
            ("anvil_snapshot", json!([])),
            ("evm_snapshot", json!([])),
            ("anvil_revert", json!(["0x1"])),
            ("evm_revert", json!(["0x1"])),
            ("anvil_mine", json!(["0x1"])),
            ("evm_mine", json!([])),
            ("anvil_setAutomine", json!([false])),
            ("evm_setAutomine", json!([true])),
            ("filecoin_getGasBreakdown", json!([hash])),
        ];

        for (method, params) in cases {
            let params = RequestParams::Array(
                params.as_array().expect("test case params are positional").clone(),
            );
            assert!(parse_request(method, params).is_ok(), "failed to parse {method}");
        }
    }

    #[test]
    fn preserves_empty_null_omitted_and_required_param_semantics() {
        for params in [None, Some(Value::Null), Some(json!([]))] {
            let mut request =
                Map::from_iter([("method".to_owned(), Value::String("eth_chainId".to_owned()))]);
            if let Some(params) = params {
                request.insert("params".to_owned(), params);
            }
            assert!(parse_json(Value::Object(request)).is_ok());
        }

        for params in [Value::Null, json!([]), json!({}), json!([null]), json!([1])] {
            assert!(
                parse_json(json!({
                    "method": "eth_getTransactionReceipt",
                    "params": params
                }))
                .is_err(),
                "unexpectedly accepted {params}"
            );
        }
    }

    #[test]
    fn storage_and_log_filters_parse_exact_public_shapes() {
        let first = address!("1111111111111111111111111111111111111111");
        let second = address!("2222222222222222222222222222222222222222");
        let topic0 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let topic1 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let topic2 = b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");

        let request = parse_json(json!({
            "method": "eth_getStorageAt",
            "params": [first, "0x2a", "earliest"]
        }))
        .unwrap();
        let RpcRequest::EthGetStorageAt { params } = request else {
            panic!("parsed the wrong request");
        };
        assert_eq!(params.address, first);
        assert_eq!(params.slot, U256::from(42));
        assert_eq!(params.block, BlockSelector::Earliest);
        assert_eq!(serde_json::to_value(B256::ZERO).unwrap().as_str().unwrap().len(), 66);

        let request = parse_json(json!({
            "method": "eth_getLogs",
            "params": [{
                "fromBlock": "0x2",
                "toBlock": "latest",
                "address": [first, second],
                "topics": [topic0, [topic1, topic2], null, []]
            }]
        }))
        .unwrap();
        let RpcRequest::EthGetLogs { params: One(filter) } = request else {
            panic!("parsed the wrong request");
        };
        assert_eq!(filter.block_hash, None);
        assert_eq!(filter.from_block, Some(BlockSelector::Number(2)));
        assert_eq!(filter.to_block, Some(BlockSelector::Latest));
        assert_eq!(filter.addresses, vec![first, second]);
        assert_eq!(filter.topics[0], Some(vec![topic0]));
        assert_eq!(filter.topics[1], Some(vec![topic1, topic2]));
        assert_eq!(filter.topics[2], None);
        assert_eq!(filter.topics[3], Some(Vec::new()));

        let block_hash = b256!("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
        assert!(
            parse_json(json!({
                "method": "eth_getLogs",
                "params": [{ "blockHash": block_hash }]
            }))
            .is_ok()
        );
        for filter in [
            json!({ "blockHash": block_hash, "fromBlock": "latest" }),
            json!({ "blockHash": block_hash, "toBlock": null }),
            json!({ "topics": [null, null, null, null, null] }),
            json!({ "unknown": true }),
        ] {
            let error =
                parse_request("eth_getLogs", RequestParams::Array(vec![filter])).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidParams);
        }
    }

    #[test]
    fn mining_methods_keep_anvil_and_ganache_params_distinct() {
        assert_eq!(
            parse_json(json!({ "method": "anvil_mine" })).unwrap(),
            RpcRequest::AnvilMine { params: AnvilMineParams::default() }
        );
        for request in [
            json!({ "method": "evm_mine" }),
            json!({ "method": "evm_mine", "params": [] }),
            json!({ "method": "evm_mine", "params": [null] }),
        ] {
            assert_eq!(
                parse_json(request).unwrap(),
                RpcRequest::EvmMine { params: EvmMineParams::default() }
            );
        }

        let RpcRequest::AnvilMine { params } =
            parse_json(json!({ "method": "anvil_mine", "params": ["0x3", "0x2"] })).unwrap()
        else {
            panic!("parsed the wrong request");
        };
        assert_eq!(params, AnvilMineParams { count: 3, interval: Some(2) });

        let RpcRequest::EvmMine { params } =
            parse_json(json!({ "method": "evm_mine", "params": [1234] })).unwrap()
        else {
            panic!("parsed the wrong request");
        };
        assert_eq!(params, EvmMineParams { count: 1, timestamp: Some(1234) });

        let RpcRequest::EvmMine { params } = parse_json(json!({
            "method": "evm_mine",
            "params": [{ "timestamp": "0x4d2", "blocks": 3 }]
        }))
        .unwrap() else {
            panic!("parsed the wrong request");
        };
        assert_eq!(params, EvmMineParams { count: 3, timestamp: Some(1234) });

        for request in [
            json!({ "method": "anvil_mine", "params": ["0x0"] }),
            json!({ "method": "anvil_mine", "params": ["0x1", "0x2", "0x3"] }),
            json!({ "method": "evm_mine", "params": [1, 2] }),
            json!({ "method": "evm_mine", "params": [{ "blocks": 0 }] }),
            json!({ "method": "evm_mine", "params": [{ "interval": 1 }] }),
        ] {
            assert!(parse_json(request).is_err());
        }
    }

    #[test]
    fn rejects_noncanonical_quantities_bytes_and_ranges() {
        for request in [
            json!({ "method": "anvil_revert", "params": ["0x01"] }),
            json!({ "method": "anvil_revert", "params": ["1"] }),
            json!({ "method": "eth_sendRawTransaction", "params": ["0x0"] }),
            json!({ "method": "eth_sendRawTransaction", "params": ["02"] }),
            json!({ "method": "eth_getBlockByNumber", "params": [format!("{:#x}", B256::ZERO), false] }),
            json!({ "method": "eth_feeHistory", "params": ["0x0", "latest", []] }),
            json!({ "method": "eth_feeHistory", "params": ["0x1", "latest", [50.0, 10.0]] }),
        ] {
            assert!(parse_json(request).is_err());
        }
    }

    #[test]
    fn call_conversion_rejects_user_limits_before_node_dispatch() {
        let address = address!("1111111111111111111111111111111111111111");
        for call in [
            json!({ "to": address, "gas": "0x0" }),
            json!({
                "to": address,
                "maxFeePerGas": "0x1",
                "maxPriorityFeePerGas": "0x2"
            }),
        ] {
            let RpcRequest::EthCall { params } = parse_json(json!({
                "method": "eth_call",
                "params": [call, "latest"]
            }))
            .unwrap() else {
                panic!("parsed the wrong request");
            };
            assert!(params.request.into_node().is_err());
        }
    }

    #[test]
    fn classifies_unknown_methods_and_bad_params() {
        let unknown = parse_request("eth_unknown", RequestParams::None).unwrap_err();
        assert_eq!(unknown.code, ErrorCode::MethodNotFound);

        let invalid =
            parse_request("eth_chainId", RequestParams::Array(vec![Value::from(1)])).unwrap_err();
        assert_eq!(invalid.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn static_dispatch_shapes_are_exact() {
        assert_eq!(
            static_response(&RpcRequest::Web3ClientVersion { params: () }),
            Some(json!(CLIENT_VERSION))
        );
        assert_eq!(static_response(&RpcRequest::EthSyncing { params: () }), Some(json!(false)));
        assert_eq!(response::quantity(0), "0x0");
        assert_eq!(response::quantity(31_337), "0x7a69");
        assert_eq!(response::gas_price(200), U256::from(201));
        assert_eq!(
            static_response(&RpcRequest::EthMaxPriorityFeePerGas { params: () }),
            Some(json!("0x1"))
        );
        assert_eq!(response::anvil_mine(), Value::Null);
        assert_eq!(response::evm_mine(), json!("0x0"));
        assert_eq!(response::optional_block(None, false).unwrap(), Value::Null);
    }

    #[test]
    fn receipt_logs_and_contract_address_have_canonical_shapes() {
        let transaction_hash =
            b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let block_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let topic = b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let contract = address!("4444444444444444444444444444444444444444");
        let value = response::receipt(TransactionReceipt {
            transaction_hash,
            transaction_index: 3,
            block_hash,
            block_number: 7,
            from: Address::ZERO,
            to: None,
            contract_address: Some(contract),
            cumulative_gas_used: 40_000,
            gas_used: 40_000,
            effective_gas_price: U256::from(101),
            status: true,
            exit_code: 0,
            output: Bytes::new(),
            logs: vec![Log {
                address: contract,
                topics: vec![topic],
                data: Bytes::from(vec![0xde, 0xad]),
                block_hash,
                block_number: 7,
                transaction_hash,
                transaction_index: 3,
                log_index: 4,
                removed: false,
            }],
        })
        .unwrap();

        assert_eq!(value["contractAddress"], json!(contract));
        assert_eq!(value["transactionIndex"], "0x3");
        assert_eq!(value["blockNumber"], "0x7");
        assert_eq!(value["status"], "0x1");
        assert_eq!(value["effectiveGasPrice"], "0x65");
        assert_eq!(value["logs"][0]["topics"], json!([topic]));
        assert_eq!(value["logs"][0]["data"], "0xdead");
        assert_eq!(value["logs"][0]["transactionIndex"], "0x3");
        assert_eq!(value["logs"][0]["logIndex"], "0x4");
        assert_ne!(value["logsBloom"], json!(Bloom::ZERO));
    }

    #[test]
    fn transaction_and_block_indexes_agree_for_hash_and_full_forms() {
        let block_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let mut transaction = Transaction {
            hash: B256::ZERO,
            chain_id: 31_337,
            from: address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266"),
            to: None,
            nonce: 7,
            value: U256::from(42),
            input: Bytes::from_static(&[0x60, 0x00, 0x60, 0x00]),
            gas_limit: 500_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_500_000_000,
            effective_gas_price: Some(U256::from(777)),
            raw: Bytes::from_static(CREATION),
            block_hash: Some(block_hash),
            block_number: Some(9),
            transaction_index: Some(0),
        };
        let transaction_value = response::transaction(transaction.clone()).unwrap();
        assert_eq!(transaction_value["blockHash"], json!(block_hash));
        assert_eq!(transaction_value["blockNumber"], "0x9");
        assert_eq!(transaction_value["transactionIndex"], "0x0");
        assert_eq!(transaction_value["to"], Value::Null);
        assert_eq!(transaction_value["effectiveGasPrice"], "0x309");
        let hash = transaction_value["hash"].as_str().unwrap().parse::<B256>().unwrap();
        transaction.hash = hash;
        let emitter = transaction.from;
        let topic = b256!("abababababababababababababababababababababababababababababababab");
        let log = Log {
            address: emitter,
            topics: vec![topic],
            data: Bytes::new(),
            block_hash,
            block_number: 9,
            transaction_hash: hash,
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        let mut expected_bloom = Bloom::ZERO;
        expected_bloom.accrue_raw_log(emitter, &[topic]);

        let block = Block {
            hash: block_hash,
            parent_hash: B256::ZERO,
            number: 9,
            state_root: B256::ZERO,
            timestamp: 1,
            gas_limit: 10_000_000_000,
            gas_used: 40_000,
            base_fee_per_gas: 100,
            logs: vec![log],
            transactions: vec![transaction],
        };
        let hashes = response::block(block.clone(), false).unwrap();
        assert_eq!(hashes["transactions"], json!([hash]));
        assert_eq!(hashes["logsBloom"], json!(expected_bloom));
        let full = response::block(block, true).unwrap();
        assert_eq!(full["transactions"][0]["hash"], json!(hash));
        assert_eq!(full["transactions"][0]["blockHash"], json!(block_hash));
        assert_eq!(full["transactions"][0]["transactionIndex"], "0x0");
        assert_eq!(full["logsBloom"], json!(expected_bloom));
    }

    #[test]
    fn log_responses_are_in_canonical_chain_order() {
        let log = |block_number, transaction_index, log_index| Log {
            address: Address::ZERO,
            topics: Vec::new(),
            data: Bytes::new(),
            block_hash: B256::ZERO,
            block_number,
            transaction_hash: B256::ZERO,
            transaction_index,
            log_index,
            removed: false,
        };
        let logs = response::logs(vec![log(2, 0, 0), log(1, 1, 0), log(1, 0, 2)]).unwrap();
        assert_eq!(
            logs.as_array()
                .unwrap()
                .iter()
                .map(|log| (
                    log["blockNumber"].as_str().unwrap(),
                    log["transactionIndex"].as_str().unwrap(),
                    log["logIndex"].as_str().unwrap(),
                ))
                .collect::<Vec<_>>(),
            vec![("0x1", "0x0", "0x2"), ("0x1", "0x1", "0x0"), ("0x2", "0x0", "0x0")]
        );
    }

    #[test]
    fn fee_history_uses_canonical_gas_and_weighted_priority_rewards() {
        let transaction = |hash: B256, effective_gas_price: u64| Transaction {
            hash,
            chain_id: 31_337,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            nonce: 0,
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 500,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas: 1_000,
            effective_gas_price: Some(U256::from(effective_gas_price)),
            raw: Bytes::new(),
            block_hash: Some(B256::ZERO),
            block_number: Some(7),
            transaction_index: Some(0),
        };
        let block = Block {
            hash: B256::ZERO,
            parent_hash: B256::ZERO,
            number: 7,
            state_root: B256::ZERO,
            timestamp: 1,
            gas_limit: 1_000,
            gas_used: 500,
            base_fee_per_gas: 100,
            logs: Vec::new(),
            transactions: vec![
                transaction(B256::from([1; 32]), 120),
                transaction(B256::from([2; 32]), 110),
            ],
        };
        let history = response::fee_history(
            vec![response::FeeHistoryBlock { block, transaction_gas_used: vec![300, 200] }],
            U256::from(101),
            &[0.0, 40.0, 40.1, 100.0],
        )
        .unwrap();
        assert_eq!(history["oldestBlock"], "0x7");
        assert_eq!(history["baseFeePerGas"], json!(["0x64", "0x65"]));
        assert_eq!(history["gasUsedRatio"], json!([0.5]));
        assert_eq!(history["reward"], json!([["0xa", "0xa", "0x14", "0x14"]]));

        let breakdown = response::gas_breakdown(FilecoinGasBreakdown {
            base_fee_burn: U256::from(1),
            over_estimation_burn: U256::from(2),
            miner_penalty: U256::from(3),
            miner_tip: U256::from(4),
            refund: U256::from(5),
            gas_refund: 6,
            gas_burned: 7,
        });
        assert_eq!(
            breakdown,
            json!({
                "baseFeeBurn": "0x1",
                "overEstimationBurn": "0x2",
                "minerPenalty": "0x3",
                "minerTip": "0x4",
                "refund": "0x5",
                "gasRefund": "0x6",
                "gasBurned": "0x7",
            })
        );
    }

    #[test]
    fn call_errors_serialize_as_json_rpc_errors() {
        let call = RpcMethodCall {
            jsonrpc: Version::V2,
            method: "eth_chainId".to_owned(),
            params: RequestParams::Object(Map::new()),
            id: Id::Number(7),
        };
        let RpcMethodCall { method, params, id, .. } = call;
        let error = parse_request(&method, params).unwrap_err();
        let response = RpcResponse::new(id, error.clone());

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "error": {
                    "code": -32602,
                    "message": error.message,
                }
            })
        );
    }

    #[test]
    fn node_errors_keep_client_and_execution_failures_out_of_internal_error() {
        let client_errors = [
            NodeError::UnknownBlockHash(B256::ZERO),
            NodeError::ConflictingLogFilterBlocks,
            NodeError::InvalidLogBlockRange { from: 2, to: 1 },
            NodeError::InvalidMineCount(0),
            NodeError::MineArithmeticOverflow { field: "timestamp" },
            NodeError::MineCapacity { count: u64::MAX, detail: "too many blocks".to_owned() },
            NodeError::InvalidMineTimestamp { minimum: 2, actual: 1 },
            NodeError::InvalidSnapshotId(U256::from(99)),
        ];
        for (index, error) in client_errors.into_iter().enumerate() {
            let response = RpcResponse::new(Id::Number(index as i64), node_error(error));
            let response = serde_json::to_value(response).unwrap();
            assert_eq!(response["error"]["code"], -32602);
        }

        let reverted = RpcResponse::new(
            Id::Number(9),
            node_error(NodeError::CallFailed {
                exit_code: 33,
                detail: "contract reverted".to_owned(),
                data: Bytes::from_static(&[0xde, 0xad]),
            }),
        );
        let reverted = serde_json::to_value(reverted).unwrap();
        assert_eq!(reverted["error"]["code"], 3);
        assert_eq!(reverted["error"]["data"], "0xdead");

        let rejected = RpcResponse::new(
            Id::Number(10),
            transaction_error(NodeError::InvalidGasLimit { actual: 0, maximum: 1 }),
        );
        let rejected = serde_json::to_value(rejected).unwrap();
        assert_eq!(rejected["error"]["code"], -32003);

        let unavailable =
            RpcResponse::new(Id::Number(11), transaction_error(NodeError::WorkerUnavailable));
        let unavailable = serde_json::to_value(unavailable).unwrap();
        assert_eq!(unavailable["error"]["code"], -32603);
    }
}
