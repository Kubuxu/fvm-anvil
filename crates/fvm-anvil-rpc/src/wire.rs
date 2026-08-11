use std::fmt;

use alloy_primitives::{Address, B256, Bytes, U256, hex};
use fvm_anvil_node::{BLOCK_GAS_LIMIT, BlockSelector, CallRequest as NodeCallRequest};
use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "method")]
pub enum RpcRequest {
    #[serde(rename = "web3_clientVersion")]
    Web3ClientVersion {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "net_version", alias = "eth_networkId")]
    NetVersion {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_chainId")]
    EthChainId {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_accounts", alias = "eth_requestAccounts")]
    EthAccounts {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_syncing")]
    EthSyncing {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_blockNumber")]
    EthBlockNumber {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_getBalance")]
    EthGetBalance { params: AddressBlockParams },
    #[serde(rename = "eth_getTransactionCount")]
    EthGetTransactionCount { params: AddressBlockParams },
    #[serde(rename = "eth_getCode")]
    EthGetCode { params: AddressBlockParams },
    #[serde(rename = "eth_getStorageAt")]
    EthGetStorageAt { params: StorageParams },
    #[serde(rename = "eth_call")]
    EthCall { params: CallParams },
    #[serde(rename = "eth_estimateGas")]
    EthEstimateGas { params: CallParams },
    #[serde(rename = "eth_sendRawTransaction")]
    EthSendRawTransaction { params: One<StrictBytes> },
    #[serde(rename = "eth_getTransactionByHash")]
    EthGetTransactionByHash { params: One<B256> },
    #[serde(rename = "eth_getTransactionReceipt")]
    EthGetTransactionReceipt { params: One<B256> },
    #[serde(rename = "eth_getBlockByNumber")]
    EthGetBlockByNumber { params: BlockParams },
    #[serde(rename = "eth_getBlockByHash")]
    EthGetBlockByHash { params: HashBlockParams },
    #[serde(rename = "eth_getLogs")]
    EthGetLogs { params: One<LogFilterParams> },
    #[serde(rename = "eth_gasPrice")]
    EthGasPrice {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_maxPriorityFeePerGas")]
    EthMaxPriorityFeePerGas {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "eth_feeHistory")]
    EthFeeHistory { params: FeeHistoryParams },
    #[serde(rename = "eth_mining")]
    EthMining {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "anvil_snapshot", alias = "evm_snapshot")]
    Snapshot {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
    #[serde(rename = "anvil_revert", alias = "evm_revert")]
    Revert { params: One<QuantityU256> },
    #[serde(rename = "anvil_mine")]
    AnvilMine {
        #[serde(default)]
        params: AnvilMineParams,
    },
    #[serde(rename = "evm_mine")]
    EvmMine {
        #[serde(default)]
        params: EvmMineParams,
    },
    #[serde(rename = "anvil_setAutomine", alias = "evm_setAutomine")]
    SetAutomine { params: One<bool> },
    #[serde(
        rename = "anvil_setBalance",
        alias = "hardhat_setBalance",
        alias = "tenderly_setBalance"
    )]
    SetBalance { params: Two<Address, AnvilQuantity> },
    #[serde(
        rename = "anvil_addBalance",
        alias = "hardhat_addBalance",
        alias = "tenderly_addBalance"
    )]
    AddBalance { params: Two<Address, AnvilQuantity> },
    #[serde(rename = "anvil_setCode", alias = "hardhat_setCode")]
    SetCode { params: Two<Address, AnvilBytes> },
    #[serde(rename = "anvil_setNonce", alias = "hardhat_setNonce", alias = "evm_setAccountNonce")]
    SetNonce { params: Two<Address, AnvilQuantity> },
    #[serde(rename = "anvil_setStorageAt", alias = "hardhat_setStorageAt")]
    SetStorageAt { params: Three<Address, AnvilQuantity, B256> },
    #[serde(rename = "filecoin_getGasBreakdown")]
    FilecoinGetGasBreakdown { params: One<B256> },
    #[serde(rename = "filecoin_getGasTrace")]
    FilecoinGetGasTrace { params: One<B256> },
    #[serde(rename = "filecoin_getStateRoots")]
    FilecoinGetStateRoots {
        #[serde(default, deserialize_with = "deserialize_empty_params")]
        params: (),
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct One<T>(pub T);

impl<'de, T> Deserialize<'de> for One<T>
where
    T: DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if values.len() != 1 {
            return Err(de::Error::custom(format_args!(
                "expected exactly 1 positional parameter, got {}",
                values.len()
            )));
        }
        deserialize_value(values.into_iter().next().expect("length checked"))
            .map(Self)
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Two<A, B>(pub A, pub B);

impl<'de, A, B> Deserialize<'de> for Two<A, B>
where
    A: DeserializeOwned,
    B: DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        let [first, second]: [Value; 2] = values.try_into().map_err(|values: Vec<Value>| {
            de::Error::custom(format_args!(
                "expected exactly 2 positional parameters, got {}",
                values.len()
            ))
        })?;
        Ok(Self(
            deserialize_value(first).map_err(de::Error::custom)?,
            deserialize_value(second).map_err(de::Error::custom)?,
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Three<A, B, C>(pub A, pub B, pub C);

impl<'de, A, B, C> Deserialize<'de> for Three<A, B, C>
where
    A: DeserializeOwned,
    B: DeserializeOwned,
    C: DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        C: DeserializeOwned,
    {
        let values = params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        let [first, second, third]: [Value; 3] =
            values.try_into().map_err(|values: Vec<Value>| {
                de::Error::custom(format_args!(
                    "expected exactly 3 positional parameters, got {}",
                    values.len()
                ))
            })?;
        Ok(Self(
            deserialize_value(first).map_err(de::Error::custom)?,
            deserialize_value(second).map_err(de::Error::custom)?,
            deserialize_value(third).map_err(de::Error::custom)?,
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddressBlockParams {
    pub address: Address,
    pub block: BlockSelector,
}

impl<'de> Deserialize<'de> for AddressBlockParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut values =
            params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if !(1..=2).contains(&values.len()) {
            return Err(de::Error::custom(format_args!(
                "expected 1 or 2 positional parameters, got {}",
                values.len()
            )));
        }
        let block = if values.len() == 2 {
            parse_optional_block(values.pop().expect("length checked"))?
        } else {
            BlockSelector::Latest
        };
        let address =
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?;
        Ok(Self { address, block })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StorageParams {
    pub address: Address,
    pub slot: U256,
    pub block: BlockSelector,
}

impl<'de> Deserialize<'de> for StorageParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut values =
            params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if !(2..=3).contains(&values.len()) {
            return Err(de::Error::custom(format_args!(
                "expected 2 or 3 positional parameters, got {}",
                values.len()
            )));
        }
        let block = if values.len() == 3 {
            parse_optional_block(values.pop().expect("length checked"))?
        } else {
            BlockSelector::Latest
        };
        let slot = deserialize_value::<QuantityU256>(values.pop().expect("length checked"))
            .map_err(de::Error::custom)?
            .0;
        let address =
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?;
        Ok(Self { address, slot, block })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CallParams {
    pub request: TransactionRequest,
    pub block: BlockSelector,
}

impl<'de> Deserialize<'de> for CallParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut values =
            params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if !(1..=2).contains(&values.len()) {
            return Err(de::Error::custom(format_args!(
                "expected 1 or 2 positional parameters, got {}",
                values.len()
            )));
        }
        let block = if values.len() == 2 {
            parse_optional_block(values.pop().expect("length checked"))?
        } else {
            BlockSelector::Latest
        };
        let request =
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?;
        Ok(Self { request, block })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionRequest {
    #[serde(default)]
    pub from: Option<Address>,
    #[serde(default)]
    pub to: Option<Address>,
    #[serde(default)]
    pub gas: Option<QuantityU64>,
    #[serde(default)]
    pub gas_price: Option<QuantityU256>,
    #[serde(default)]
    pub value: Option<QuantityU256>,
    #[serde(default)]
    pub data: Option<StrictBytes>,
    #[serde(default)]
    pub input: Option<StrictBytes>,
    #[serde(default)]
    pub nonce: Option<QuantityU64>,
    #[serde(default)]
    pub max_fee_per_gas: Option<QuantityU256>,
    #[serde(default)]
    pub max_priority_fee_per_gas: Option<QuantityU256>,
    #[serde(default)]
    pub chain_id: Option<QuantityU64>,
    #[serde(default, rename = "type")]
    pub transaction_type: Option<QuantityU64>,
    #[serde(default)]
    pub access_list: Option<Vec<AccessListItem>>,
}

impl TransactionRequest {
    pub fn into_node(self) -> Result<NodeCallRequest, String> {
        if self.gas_price.is_some()
            && (self.max_fee_per_gas.is_some() || self.max_priority_fee_per_gas.is_some())
        {
            return Err("gasPrice cannot be combined with EIP-1559 fee fields".to_owned());
        }
        if self.access_list.as_ref().is_some_and(|items| !items.is_empty()) {
            return Err("non-empty access lists are not supported".to_owned());
        }
        if self.transaction_type.is_some_and(|kind| kind.0 != 0 && kind.0 != 2) {
            return Err("only legacy and EIP-1559 call types are supported".to_owned());
        }
        if self.gas.is_some_and(|gas| gas.0 == 0 || gas.0 > BLOCK_GAS_LIMIT) {
            return Err(format!("gas limit must be between 1 and {BLOCK_GAS_LIMIT}"));
        }

        let input = match (self.input, self.data) {
            (Some(input), Some(data)) if input != data => {
                return Err("transaction input and data fields disagree".to_owned());
            }
            (Some(input), _) => input.0,
            (_, Some(data)) => data.0,
            (None, None) => Bytes::new(),
        };
        let legacy_fee = self.gas_price.map(|value| value.0);
        let max_fee_per_gas =
            checked_u128(self.max_fee_per_gas.map(|value| value.0).or(legacy_fee))?;
        let max_priority_fee_per_gas =
            checked_u128(self.max_priority_fee_per_gas.map(|value| value.0).or(legacy_fee))?;
        if max_fee_per_gas
            .zip(max_priority_fee_per_gas)
            .is_some_and(|(max_fee, priority_fee)| priority_fee > max_fee)
        {
            return Err("maxPriorityFeePerGas cannot exceed maxFeePerGas".to_owned());
        }
        Ok(NodeCallRequest {
            from: self.from,
            to: self.to,
            value: self.value.map_or(U256::ZERO, |value| value.0),
            input,
            gas_limit: self.gas.map(|value| value.0),
            max_fee_per_gas,
            max_priority_fee_per_gas,
            nonce: self.nonce.map(|value| value.0),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessListItem {
    pub address: Address,
    pub storage_keys: Vec<B256>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockParams {
    pub block: BlockSelector,
    pub full_transactions: bool,
}

impl<'de> Deserialize<'de> for BlockParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut values =
            params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if values.len() != 2 {
            return Err(de::Error::custom(format_args!(
                "expected exactly 2 positional parameters, got {}",
                values.len()
            )));
        }
        let full_transactions =
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?;
        let block = parse_number_block(values.pop().expect("length checked"))?;
        Ok(Self { block, full_transactions })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashBlockParams {
    pub hash: B256,
    pub full_transactions: bool,
}

impl<'de> Deserialize<'de> for HashBlockParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut values =
            params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if values.len() != 2 {
            return Err(de::Error::custom(format_args!(
                "expected exactly 2 positional parameters, got {}",
                values.len()
            )));
        }
        let full_transactions =
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?;
        let hash =
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?;
        Ok(Self { hash, full_transactions })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFilterParams {
    pub block_hash: Option<B256>,
    pub from_block: Option<BlockSelector>,
    pub to_block: Option<BlockSelector>,
    pub addresses: Vec<Address>,
    pub topics: [Option<Vec<B256>>; 4],
}

impl<'de> Deserialize<'de> for LogFilterParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        let block_hash = object
            .remove("blockHash")
            .map(deserialize_value)
            .transpose()
            .map_err(de::Error::custom)?;
        let from_block_present = object.contains_key("fromBlock");
        let to_block_present = object.contains_key("toBlock");
        if block_hash.is_some() && (from_block_present || to_block_present) {
            return Err(de::Error::custom(
                "blockHash cannot be combined with fromBlock or toBlock",
            ));
        }
        let from_block = object
            .remove("fromBlock")
            .map(parse_optional_filter_block)
            .transpose()
            .map_err(de::Error::custom)?
            .flatten();
        let to_block = object
            .remove("toBlock")
            .map(parse_optional_filter_block)
            .transpose()
            .map_err(de::Error::custom)?
            .flatten();
        let addresses = object
            .remove("address")
            .map(parse_filter_addresses)
            .transpose()
            .map_err(de::Error::custom)?
            .unwrap_or_default();
        let topics = object
            .remove("topics")
            .map(parse_filter_topics)
            .transpose()
            .map_err(de::Error::custom)?
            .unwrap_or([None, None, None, None]);
        if !object.is_empty() {
            return Err(de::Error::custom(format_args!(
                "unknown log filter field {:?}",
                object.keys().next().expect("map is not empty")
            )));
        }
        Ok(Self { block_hash, from_block, to_block, addresses, topics })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FeeHistoryParams {
    pub block_count: u64,
    pub newest_block: BlockSelector,
    pub reward_percentiles: Vec<f64>,
}

impl<'de> Deserialize<'de> for FeeHistoryParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut values =
            params_array(Value::deserialize(deserializer)?).map_err(de::Error::custom)?;
        if !(2..=3).contains(&values.len()) {
            return Err(de::Error::custom(format_args!(
                "expected 2 or 3 positional parameters, got {}",
                values.len()
            )));
        }
        let reward_percentiles = if values.len() == 3 {
            deserialize_value(values.pop().expect("length checked")).map_err(de::Error::custom)?
        } else {
            Vec::new()
        };
        validate_reward_percentiles(&reward_percentiles).map_err(de::Error::custom)?;
        let newest_block = parse_number_block(values.pop().expect("length checked"))?;
        let block_count = deserialize_value::<QuantityU64>(values.pop().expect("length checked"))
            .map_err(de::Error::custom)?
            .0;
        if block_count == 0 || block_count > 1024 {
            return Err(de::Error::custom("fee history block count must be between 1 and 1024"));
        }
        Ok(Self { block_count, newest_block, reward_percentiles })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnvilMineParams {
    pub count: u64,
    pub interval: Option<u64>,
}

impl Default for AnvilMineParams {
    fn default() -> Self {
        Self { count: 1, interval: None }
    }
}

impl<'de> Deserialize<'de> for AnvilMineParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value.is_null() {
            return Ok(Self::default());
        }
        let values = params_array(value).map_err(de::Error::custom)?;
        if values.len() > 2 {
            return Err(de::Error::custom(format_args!(
                "expected at most 2 positional parameters, got {}",
                values.len()
            )));
        }
        let count = values
            .first()
            .filter(|value| !value.is_null())
            .map(|value| parse_u64_input(value.clone(), "block count"))
            .transpose()
            .map_err(de::Error::custom)?
            .unwrap_or(1);
        if count == 0 {
            return Err(de::Error::custom("block count must be greater than zero"));
        }
        let interval = values
            .get(1)
            .filter(|value| !value.is_null())
            .map(|value| parse_u64_input(value.clone(), "block interval"))
            .transpose()
            .map_err(de::Error::custom)?;
        Ok(Self { count, interval })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmMineParams {
    pub count: u64,
    pub timestamp: Option<u64>,
}

impl Default for EvmMineParams {
    fn default() -> Self {
        Self { count: 1, timestamp: None }
    }
}

impl<'de> Deserialize<'de> for EvmMineParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value.is_null() {
            return Ok(Self::default());
        }
        let values = params_array(value).map_err(de::Error::custom)?;
        let Some(value) = values.first() else {
            return Ok(Self::default());
        };
        if values.len() != 1 {
            return Err(de::Error::custom(format_args!(
                "expected at most 1 positional parameter, got {}",
                values.len()
            )));
        }
        if value.is_null() {
            return Ok(Self::default());
        }
        match value {
            Value::Object(object) => {
                parse_evm_mine_options(object.clone()).map_err(de::Error::custom)
            }
            value => parse_u64_input(value.clone(), "timestamp")
                .map(|timestamp| Self { count: 1, timestamp: Some(timestamp) })
                .map_err(de::Error::custom),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnvilBytes(pub Bytes);

impl<'de> Deserialize<'de> for AnvilBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() {
            return Ok(Self(Bytes::new()));
        }
        let digits = text
            .strip_prefix("0x")
            .ok_or_else(|| de::Error::custom("bytes must be empty or have a 0x prefix"))?;
        if digits.len() % 2 != 0 {
            return Err(de::Error::custom("bytes must contain an even number of hex digits"));
        }
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(de::Error::custom("bytes contain a non-hex digit"));
        }
        hex::decode(digits).map(Bytes::from).map(Self).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnvilQuantity(pub U256);

impl<'de> Deserialize<'de> for AnvilQuantity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::String(text) => {
                let (digits, radix) =
                    text.strip_prefix("0x").map_or((text.as_str(), 10), |digits| (digits, 16));
                if digits.is_empty() {
                    return Err(de::Error::custom("number must contain at least one digit"));
                }
                U256::from_str_radix(digits, radix).map(Self).map_err(de::Error::custom)
            }
            Value::Number(number) => {
                number.as_u64().map(U256::from).map(Self).ok_or_else(|| {
                    de::Error::custom("JSON number must be an unsigned 64-bit integer")
                })
            }
            _ => Err(de::Error::custom("number must be a hex or decimal string or JSON integer")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StrictBytes(pub Bytes);

impl<'de> Deserialize<'de> for StrictBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let digits = text
            .strip_prefix("0x")
            .ok_or_else(|| de::Error::custom("bytes must have a 0x prefix"))?;
        if digits.len() % 2 != 0 {
            return Err(de::Error::custom("bytes must contain an even number of hex digits"));
        }
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(de::Error::custom("bytes contain a non-hex digit"));
        }
        hex::decode(digits).map(Bytes::from).map(Self).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantityU64(pub u64);

impl<'de> Deserialize<'de> for QuantityU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let quantity = QuantityU256::deserialize(deserializer)?.0;
        u64::try_from(quantity)
            .map(Self)
            .map_err(|_| de::Error::custom("quantity does not fit in u64"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantityU256(pub U256);

impl<'de> Deserialize<'de> for QuantityU256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        parse_quantity(&text).map(Self).map_err(de::Error::custom)
    }
}

fn deserialize_empty_params<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(());
    }
    let values = params_array(value).map_err(de::Error::custom)?;
    if values.is_empty() {
        Ok(())
    } else {
        Err(de::Error::custom(format_args!(
            "expected no positional parameters, got {}",
            values.len()
        )))
    }
}

fn params_array(value: Value) -> Result<Vec<Value>, &'static str> {
    match value {
        Value::Array(values) => Ok(values),
        Value::Null => Err("required positional parameters are missing"),
        Value::Object(_) => Err("named parameters are not supported"),
        _ => Err("parameters must be a positional array"),
    }
}

fn deserialize_value<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value)
}

fn parse_optional_filter_block(value: Value) -> Result<Option<BlockSelector>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let Value::String(text) = value else {
        return Err("filter block must be a tag or quantity string".to_owned());
    };
    let selector = parse_block_string(&text)?;
    if matches!(selector, BlockSelector::Hash(_)) {
        Err("a block hash is not valid in fromBlock or toBlock".to_owned())
    } else {
        Ok(Some(selector))
    }
}

fn parse_filter_addresses(value: Value) -> Result<Vec<Address>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(_) => deserialize_value(value)
            .map(|address| vec![address])
            .map_err(|error| format!("invalid log filter address: {error}")),
        Value::Array(_) => deserialize_value(value)
            .map_err(|error| format!("invalid log filter address list: {error}")),
        _ => Err("log filter address must be an address or array of addresses".to_owned()),
    }
}

fn parse_filter_topics(value: Value) -> Result<[Option<Vec<B256>>; 4], String> {
    let Value::Array(values) = value else {
        return Err("topics must be an array".to_owned());
    };
    if values.len() > 4 {
        return Err("topics cannot contain more than four positions".to_owned());
    }
    let mut topics = [None, None, None, None];
    for (index, value) in values.into_iter().enumerate() {
        topics[index] = match value {
            Value::Null => None,
            Value::String(_) => Some(vec![
                deserialize_value(value)
                    .map_err(|error| format!("invalid topic at position {index}: {error}"))?,
            ]),
            Value::Array(_) => Some(deserialize_value(value).map_err(|error| {
                format!("invalid topic alternatives at position {index}: {error}")
            })?),
            _ => {
                return Err(format!(
                    "topic at position {index} must be null, a hash, or an array of hashes"
                ));
            }
        };
    }
    Ok(topics)
}

fn parse_u64_input(value: Value, field: &str) -> Result<u64, String> {
    match value {
        Value::String(text) => parse_quantity(&text).and_then(|value| {
            u64::try_from(value).map_err(|_| format!("{field} does not fit in u64"))
        }),
        Value::Number(number) => {
            number.as_u64().ok_or_else(|| format!("{field} must be a non-negative integer"))
        }
        _ => Err(format!("{field} must be an integer or quantity string")),
    }
}

fn parse_evm_mine_options(mut object: Map<String, Value>) -> Result<EvmMineParams, String> {
    let timestamp = object
        .remove("timestamp")
        .filter(|value| !value.is_null())
        .map(|value| parse_u64_input(value, "timestamp"))
        .transpose()?;
    let count = object
        .remove("blocks")
        .filter(|value| !value.is_null())
        .map(|value| parse_u64_input(value, "block count"))
        .transpose()?
        .unwrap_or(1);
    if !object.is_empty() {
        return Err(format!(
            "unknown evm_mine option {:?}",
            object.keys().next().expect("map is not empty")
        ));
    }
    if count == 0 {
        return Err("block count must be greater than zero".to_owned());
    }
    Ok(EvmMineParams { count, timestamp })
}

fn parse_optional_block<E>(value: Value) -> Result<BlockSelector, E>
where
    E: de::Error,
{
    if value.is_null() { Ok(BlockSelector::Latest) } else { parse_block(value).map_err(E::custom) }
}

fn parse_number_block<E>(value: Value) -> Result<BlockSelector, E>
where
    E: de::Error,
{
    let Value::String(text) = value else {
        return Err(E::custom("block number must be a tag or quantity string"));
    };
    let selector = parse_block_string(&text).map_err(E::custom)?;
    if matches!(selector, BlockSelector::Hash(_)) {
        Err(E::custom("a block hash is not valid for this method"))
    } else {
        Ok(selector)
    }
}

fn parse_block(value: Value) -> Result<BlockSelector, String> {
    match value {
        Value::String(text) => parse_block_string(&text),
        Value::Object(object) => parse_block_object(object),
        _ => Err("block selector must be a tag, quantity, hash, or EIP-1898 object".to_owned()),
    }
}

fn parse_block_string(text: &str) -> Result<BlockSelector, String> {
    match text {
        "latest" | "safe" | "finalized" => Ok(BlockSelector::Latest),
        "earliest" => Ok(BlockSelector::Earliest),
        "pending" => Ok(BlockSelector::Pending),
        _ if text.len() == 66 => text
            .parse::<B256>()
            .map(BlockSelector::Hash)
            .map_err(|error| format!("invalid block hash: {error}")),
        _ => parse_quantity(text)
            .and_then(|quantity| {
                u64::try_from(quantity).map_err(|_| "block number does not fit in u64".to_owned())
            })
            .map(BlockSelector::Number),
    }
}

fn parse_block_object(mut object: Map<String, Value>) -> Result<BlockSelector, String> {
    let hash = object.remove("blockHash");
    let number = object.remove("blockNumber");
    let canonical = object.remove("requireCanonical");
    if !object.is_empty() {
        return Err("unknown EIP-1898 block selector field".to_owned());
    }
    if canonical.is_some_and(|value| value != Value::Bool(true) && value != Value::Bool(false)) {
        return Err("requireCanonical must be a boolean".to_owned());
    }
    match (hash, number) {
        (Some(hash), None) => deserialize_value(hash)
            .map(BlockSelector::Hash)
            .map_err(|error| format!("invalid block hash: {error}")),
        (None, Some(Value::String(number))) => parse_quantity(&number)
            .and_then(|quantity| {
                u64::try_from(quantity).map_err(|_| "block number does not fit in u64".to_owned())
            })
            .map(BlockSelector::Number),
        (None, Some(_)) => Err("blockNumber must be a quantity string".to_owned()),
        _ => Err("EIP-1898 selector requires exactly one of blockHash or blockNumber".to_owned()),
    }
}

fn parse_quantity(text: &str) -> Result<U256, String> {
    let digits =
        text.strip_prefix("0x").ok_or_else(|| "quantity must have a 0x prefix".to_owned())?;
    if digits.is_empty() {
        return Err("quantity must contain at least one hex digit".to_owned());
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err("quantity must use minimal hexadecimal encoding".to_owned());
    }
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("quantity contains a non-hex digit".to_owned());
    }
    U256::from_str_radix(digits, 16).map_err(|error| format!("invalid quantity: {error}"))
}

fn validate_reward_percentiles(percentiles: &[f64]) -> Result<(), &'static str> {
    let mut previous = None;
    for percentile in percentiles {
        if !percentile.is_finite() || !(0.0..=100.0).contains(percentile) {
            return Err("reward percentile must be finite and between 0 and 100");
        }
        if previous.is_some_and(|value| value > *percentile) {
            return Err("reward percentiles must be sorted in ascending order");
        }
        previous = Some(*percentile);
    }
    Ok(())
}

fn checked_u128(value: Option<U256>) -> Result<Option<u128>, String> {
    value
        .map(|value| {
            u128::try_from(value).map_err(|_| "fee quantity does not fit in u128".to_owned())
        })
        .transpose()
}

impl fmt::Display for StrictBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
