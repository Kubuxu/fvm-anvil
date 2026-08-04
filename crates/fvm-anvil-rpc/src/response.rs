use alloy_consensus::{
    Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, TxEnvelope, transaction::Recovered,
};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bloom, Log as PrimitiveLog, U256};
use alloy_rpc_types_eth::{
    Log as RpcLog, Transaction as RpcTransaction, TransactionReceipt as RpcReceipt,
};
use fvm_anvil_node::{
    Block, FilecoinGasBreakdown, FilecoinStateRoots, Log as NodeLog, Transaction,
    TransactionReceipt as NodeTransactionReceipt,
};
use serde_json::{Map, Value, json};

pub const MAX_PRIORITY_FEE_PER_GAS: u128 = 1;

pub fn quantity<T>(value: T) -> String
where
    T: core::fmt::LowerHex,
{
    format!("0x{value:x}")
}

pub fn state_roots(roots: FilecoinStateRoots) -> Value {
    json!({
        "prepared": roots.prepared.map(|root| root.to_string()),
        "current": roots.current.to_string(),
    })
}

pub fn transaction(transaction: Transaction) -> Result<Value, String> {
    let envelope = TxEnvelope::decode_2718_exact(transaction.raw.as_ref())
        .map_err(|error| format!("stored transaction envelope is malformed: {error}"))?;
    let effective_gas_price = transaction.effective_gas_price;
    let response = RpcTransaction {
        inner: Recovered::new_unchecked(envelope, transaction.from),
        block_hash: transaction.block_hash,
        block_number: transaction.block_number,
        transaction_index: transaction.transaction_index,
        effective_gas_price: None,
        block_timestamp: None,
    };
    let mut value = serde_json::to_value(response)
        .map_err(|error| format!("failed to serialize transaction response: {error}"))?;
    if let Some(effective_gas_price) = effective_gas_price {
        value
            .as_object_mut()
            .expect("RPC transaction serializes as an object")
            .insert("effectiveGasPrice".to_owned(), json!(quantity(effective_gas_price)));
    }
    Ok(value)
}

pub fn receipt(receipt: NodeTransactionReceipt) -> Result<Value, String> {
    let logs_bloom = logs_bloom(&receipt.logs);
    let logs = receipt.logs.into_iter().map(rpc_log).collect::<Vec<_>>();
    let effective_gas_price = receipt.effective_gas_price;
    let inner = ReceiptEnvelope::Eip1559(ReceiptWithBloom::new(
        Receipt {
            status: Eip658Value::Eip658(receipt.status),
            cumulative_gas_used: receipt.cumulative_gas_used,
            logs,
        },
        logs_bloom,
    ));
    let response = RpcReceipt {
        inner,
        transaction_hash: receipt.transaction_hash,
        transaction_index: Some(receipt.transaction_index),
        block_hash: Some(receipt.block_hash),
        block_number: Some(receipt.block_number),
        gas_used: receipt.gas_used,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from: receipt.from,
        to: receipt.to,
        contract_address: receipt.contract_address,
    };
    let mut value = serde_json::to_value(response)
        .map_err(|error| format!("failed to serialize transaction receipt response: {error}"))?;
    value
        .as_object_mut()
        .expect("RPC receipt serializes as an object")
        .insert("effectiveGasPrice".to_owned(), json!(quantity(effective_gas_price)));
    Ok(value)
}

pub fn block(block: Block, full_transactions: bool) -> Result<Value, String> {
    let block_logs_bloom = logs_bloom(&block.logs);
    let transactions = if full_transactions {
        block.transactions.into_iter().map(transaction).collect::<Result<Vec<_>, _>>()?
    } else {
        block.transactions.iter().map(|tx| json!(tx.hash)).collect()
    };
    let mut response = Map::new();
    response.insert("hash".to_owned(), json!(block.hash));
    response.insert("parentHash".to_owned(), json!(block.parent_hash));
    response.insert(
        "sha3Uncles".to_owned(),
        json!("0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
    );
    response.insert("miner".to_owned(), json!(Address::ZERO));
    response.insert("stateRoot".to_owned(), json!(block.state_root));
    response.insert("transactionsRoot".to_owned(), json!(B256::ZERO));
    response.insert("receiptsRoot".to_owned(), json!(B256::ZERO));
    response.insert("logsBloom".to_owned(), json!(block_logs_bloom));
    response.insert("difficulty".to_owned(), json!("0x0"));
    response.insert("totalDifficulty".to_owned(), json!("0x0"));
    response.insert("number".to_owned(), json!(quantity(block.number)));
    response.insert("gasLimit".to_owned(), json!(quantity(block.gas_limit)));
    response.insert("gasUsed".to_owned(), json!(quantity(block.gas_used)));
    response.insert("timestamp".to_owned(), json!(quantity(block.timestamp)));
    response.insert("extraData".to_owned(), json!("0x"));
    response.insert("mixHash".to_owned(), json!(B256::ZERO));
    response.insert("nonce".to_owned(), json!("0x0000000000000000"));
    response.insert("baseFeePerGas".to_owned(), json!(quantity(block.base_fee_per_gas)));
    response.insert("size".to_owned(), json!("0x0"));
    response.insert("transactions".to_owned(), Value::Array(transactions));
    response.insert("uncles".to_owned(), Value::Array(Vec::new()));
    Ok(Value::Object(response))
}

pub fn optional_block(block: Option<Block>, full_transactions: bool) -> Result<Value, String> {
    match block {
        Some(block) => self::block(block, full_transactions),
        None => Ok(Value::Null),
    }
}

pub struct FeeHistoryBlock {
    pub block: Block,
    pub transaction_gas_used: Vec<u64>,
}

pub fn fee_history(
    blocks: Vec<FeeHistoryBlock>,
    next_base_fee: U256,
    reward_percentiles: &[f64],
) -> Result<Value, String> {
    let oldest = blocks.first().map_or(0, |entry| entry.block.number);
    let mut base_fee_per_gas = Vec::with_capacity(blocks.len() + 1);
    let mut gas_used_ratio = Vec::with_capacity(blocks.len());
    let mut rewards = (!reward_percentiles.is_empty()).then(|| Vec::with_capacity(blocks.len()));

    for entry in blocks {
        if entry.block.transactions.len() != entry.transaction_gas_used.len() {
            return Err(format!(
                "block {} transaction and receipt counts disagree",
                entry.block.number
            ));
        }
        base_fee_per_gas.push(quantity(entry.block.base_fee_per_gas));
        gas_used_ratio.push(if entry.block.gas_limit == 0 {
            0.0
        } else {
            entry.block.gas_used as f64 / entry.block.gas_limit as f64
        });
        if let Some(rewards) = rewards.as_mut() {
            rewards.push(block_rewards(
                &entry.block,
                &entry.transaction_gas_used,
                reward_percentiles,
            )?);
        }
    }
    base_fee_per_gas.push(quantity(next_base_fee));

    let mut response = Map::new();
    response.insert("oldestBlock".to_owned(), json!(quantity(oldest)));
    response.insert("baseFeePerGas".to_owned(), json!(base_fee_per_gas));
    response.insert("gasUsedRatio".to_owned(), json!(gas_used_ratio));
    if let Some(rewards) = rewards {
        response.insert("reward".to_owned(), json!(rewards));
    }
    Ok(Value::Object(response))
}

fn block_rewards(
    block: &Block,
    transaction_gas_used: &[u64],
    reward_percentiles: &[f64],
) -> Result<Vec<String>, String> {
    let base_fee = U256::from(block.base_fee_per_gas);
    let mut weighted_rewards = Vec::with_capacity(block.transactions.len());
    for (transaction, gas_used) in block.transactions.iter().zip(transaction_gas_used) {
        if *gas_used == 0 {
            continue;
        }
        let effective_gas_price = transaction.effective_gas_price.ok_or_else(|| {
            format!("mined transaction {} is missing its effective gas price", transaction.hash)
        })?;
        let priority_reward = effective_gas_price.checked_sub(base_fee).unwrap_or_default();
        weighted_rewards.push((priority_reward, *gas_used));
    }
    if weighted_rewards.is_empty() {
        return Ok(vec![quantity(U256::ZERO); reward_percentiles.len()]);
    }

    weighted_rewards.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let total_gas =
        weighted_rewards.iter().map(|(_, gas_used)| u128::from(*gas_used)).sum::<u128>();
    let mut index = 0;
    let mut cumulative_gas = u128::from(weighted_rewards[0].1);
    let mut rewards = Vec::with_capacity(reward_percentiles.len());
    for percentile in reward_percentiles {
        let target_gas = total_gas as f64 * *percentile / 100.0;
        while index + 1 < weighted_rewards.len() && (cumulative_gas as f64) < target_gas {
            index += 1;
            cumulative_gas += u128::from(weighted_rewards[index].1);
        }
        rewards.push(quantity(weighted_rewards[index].0));
    }
    Ok(rewards)
}

pub fn gas_breakdown(breakdown: FilecoinGasBreakdown) -> Value {
    json!({
        "baseFeeBurn": quantity(breakdown.base_fee_burn),
        "overEstimationBurn": quantity(breakdown.over_estimation_burn),
        "minerPenalty": quantity(breakdown.miner_penalty),
        "minerTip": quantity(breakdown.miner_tip),
        "refund": quantity(breakdown.refund),
        "gasRefund": quantity(breakdown.gas_refund),
        "gasBurned": quantity(breakdown.gas_burned),
    })
}

pub fn logs(mut logs: Vec<NodeLog>) -> Result<Value, String> {
    logs.sort_unstable_by_key(|log| (log.block_number, log.transaction_index, log.log_index));
    serde_json::to_value(logs.into_iter().map(rpc_log).collect::<Vec<_>>())
        .map_err(|error| format!("failed to serialize log response: {error}"))
}

fn rpc_log(log: NodeLog) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(log.address, log.topics, log.data),
        block_hash: Some(log.block_hash),
        block_number: Some(log.block_number),
        block_timestamp: None,
        transaction_hash: Some(log.transaction_hash),
        transaction_index: Some(log.transaction_index),
        log_index: Some(log.log_index),
        removed: log.removed,
    }
}
pub const fn anvil_mine() -> Value {
    Value::Null
}

pub fn evm_mine() -> Value {
    json!("0x0")
}

fn logs_bloom(logs: &[NodeLog]) -> Bloom {
    let mut bloom = Bloom::ZERO;
    for log in logs {
        bloom.accrue_raw_log(log.address, &log.topics);
    }
    bloom
}

pub fn gas_price(base_fee_per_gas: u128) -> U256 {
    U256::from(base_fee_per_gas.saturating_add(MAX_PRIORITY_FEE_PER_GAS))
}
