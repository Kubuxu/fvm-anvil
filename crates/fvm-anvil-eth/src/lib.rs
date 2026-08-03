//! Pure Ethereum-to-Filecoin protocol conversion for fvm-anvil.
//!
//! Mutable FVM state, RPC wire types, and runtime execution do not belong in this crate.

use alloy_consensus::{TxEip1559, TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::{Decodable2718, Eip2718Error};
use alloy_primitives::{Address as EthereumAddress, B256, Bytes, TxKind, U256};
use fvm_ipld_encoding::{
    BytesDe, IPLD_RAW, RawBytes, from_slice, strict_bytes, to_vec,
    tuple::{Deserialize_tuple, serde_tuple},
};
use fvm_shared::{
    address::Address as FilecoinAddress,
    bigint::{BigInt, Sign},
    econ::TokenAmount,
    event::ActorEvent,
    message::Message,
};
use serde::{Serialize, Serializer, ser::SerializeTuple};
use thiserror::Error;

/// Actor ID of the Ethereum Address Manager actor.
pub const EAM_ACTOR_ID: u64 = 10;
/// EAM method used by externally signed Ethereum contract creations.
pub const EAM_CREATE_EXTERNAL_METHOD: u64 = 4;
/// EVM actor method number for `InvokeEVM`.
pub const EVM_INVOKE_CONTRACT_METHOD: u64 = 3_844_450_837;
/// Filecoin signature type byte assigned to delegated signatures by FIP-0091.
pub const DELEGATED_SIGNATURE_TYPE: u8 = 3;

const EIP_1559_TRANSACTION_TYPE: u8 = 2;
const DELEGATED_SIGNATURE_LENGTH: usize = 65;
/// FEVM method whose return value is being projected into Ethereum semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionKind {
    /// EAM `CreateExternal`, used for an Ethereum contract-creation transaction or call.
    CreateExternal,
    /// EVM `InvokeEVM`, used for a call to an existing Ethereum contract.
    InvokeContract,
}

/// Why an unsigned Ethereum call could not be mapped to a Filecoin message.
#[derive(Debug, Error)]
pub enum CallError {
    /// The FVM represents gas limits as a signed 64-bit quantity internally.
    #[error("Ethereum call gas limit {gas_limit} exceeds the FVM maximum")]
    GasLimitOutOfRange {
        /// The invalid Ethereum gas limit.
        gas_limit: u64,
    },
    /// The sender or recipient could not be represented as an EAM delegated address.
    #[error("failed to construct a delegated Filecoin call address")]
    Address {
        /// The Filecoin address validation error.
        #[source]
        source: fvm_shared::address::Error,
    },
    /// The calldata could not be encoded as the strict CBOR byte string FEVM expects.
    #[error("failed to encode Ethereum calldata as a CBOR byte string")]
    Calldata {
        /// The DAG-CBOR encoding error.
        #[source]
        source: fvm_ipld_encoding::Error,
    },
}

/// Why an FVM actor event cannot represent an Ethereum log.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EventError {
    /// Ethereum log entries must use the raw IPLD codec.
    #[error("event entry {key:?} uses codec 0x{codec:x}, expected IPLD_RAW")]
    UnsupportedCodec {
        /// Entry key.
        key: String,
        /// Unexpected multicodec.
        codec: u64,
    },
    /// An Ethereum topic must be exactly one EVM word.
    #[error("event topic {key:?} has {actual} bytes, expected 32")]
    InvalidTopicLength {
        /// Topic entry key.
        key: String,
        /// Actual byte length.
        actual: usize,
    },
    /// The event contains a topic position more than once.
    #[error("event contains duplicate topic t{index}")]
    DuplicateTopic {
        /// One-based Ethereum topic index.
        index: usize,
    },
    /// Ethereum topics must be contiguous from `t1`.
    #[error("event is missing topic t{index}")]
    MissingTopic {
        /// One-based missing topic index.
        index: usize,
    },
    /// The event contains the Ethereum data entry more than once.
    #[error("event contains duplicate data entries")]
    DuplicateData,
}

/// Errors returned while converting an Ethereum envelope into a Filecoin message.
#[derive(Debug, Error)]
pub enum BridgeError {
    /// The supplied envelope had no bytes.
    #[error("Ethereum transaction envelope is empty")]
    EmptyEnvelope,
    /// Legacy and EIP-155 envelopes are reserved for a later bridge slice.
    #[error("legacy Ethereum transactions are not supported")]
    UnsupportedLegacy,
    /// The envelope uses a typed transaction format other than EIP-1559.
    #[error("Ethereum transaction type 0x{transaction_type:02x} is not supported")]
    UnsupportedTransactionType {
        /// The EIP-2718 transaction type byte.
        transaction_type: u8,
    },
    /// The EIP-1559 envelope is not canonical or cannot be decoded.
    #[error("malformed EIP-1559 transaction envelope")]
    MalformedEnvelope {
        /// The underlying EIP-2718/RLP error.
        #[source]
        source: Eip2718Error,
    },
    /// The transaction is signed for a different Ethereum chain.
    #[error("wrong Ethereum chain ID: expected {expected}, got {actual}")]
    WrongChainId {
        /// Chain ID required by the caller.
        expected: u64,
        /// Chain ID committed to by the transaction signature.
        actual: u64,
    },
    /// FIP-0091 only permits an empty EIP-1559 access list.
    #[error("EIP-1559 access lists are not supported ({entries} entries)")]
    UnsupportedAccessList {
        /// Number of entries in the access list.
        entries: usize,
    },
    /// The secp256k1 signature is malformed, non-canonical, or unrecoverable.
    #[error("invalid EIP-1559 signature")]
    InvalidSignature {
        /// The signature recovery error.
        #[source]
        source: alloy_consensus::crypto::RecoveryError,
    },
    /// An Ethereum address could not be represented as an EAM delegated address.
    #[error("failed to construct delegated Filecoin address")]
    Address {
        /// The Filecoin address validation error.
        #[source]
        source: fvm_shared::address::Error,
    },
    /// Filecoin parameter or signed-message CBOR serialization failed.
    #[error("failed to serialize Filecoin message")]
    Serialization {
        /// The DAG-CBOR encoding error.
        #[source]
        source: fvm_ipld_encoding::Error,
    },
    /// An unsigned Ethereum call contains a field FEVM cannot represent.
    #[error("invalid unsigned Ethereum call")]
    MalformedCall {
        /// The typed call-conversion failure.
        #[source]
        source: CallError,
    },
    /// Successful FEVM return bytes do not match the invoked actor's return schema.
    #[error("malformed {kind:?} return data")]
    MalformedReturn {
        /// FEVM method whose return schema was expected.
        kind: ExecutionKind,
        /// The DAG-CBOR decoding error.
        #[source]
        source: fvm_ipld_encoding::Error,
    },
    /// Failed FEVM receipt data is not a strict CBOR byte string.
    #[error("malformed FEVM revert data")]
    MalformedRevert {
        /// The DAG-CBOR decoding error.
        #[source]
        source: fvm_ipld_encoding::Error,
    },
    /// An FVM actor event violates the Ethereum log-entry convention.
    #[error("malformed Ethereum actor event")]
    MalformedEvent {
        /// The typed event projection failure.
        #[source]
        source: EventError,
    },
}

/// A delegated Filecoin signature carrying an Ethereum `(r, s, y_parity)` signature.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DelegatedSignature {
    bytes: [u8; DELEGATED_SIGNATURE_LENGTH],
}

impl DelegatedSignature {
    /// Returns the signature bytes without the Filecoin signature type byte.
    pub const fn bytes(&self) -> &[u8; DELEGATED_SIGNATURE_LENGTH] {
        &self.bytes
    }

    /// Returns the Filecoin signature type byte.
    pub const fn signature_type(&self) -> u8 {
        DELEGATED_SIGNATURE_TYPE
    }
}

impl Serialize for DelegatedSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut bytes = [0_u8; DELEGATED_SIGNATURE_LENGTH + 1];
        bytes[0] = DELEGATED_SIGNATURE_TYPE;
        bytes[1..].copy_from_slice(&self.bytes);
        serializer.serialize_bytes(&bytes)
    }
}

/// Canonical Filecoin signed-message tuple used for delegated Ethereum messages.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FilecoinSignedMessage {
    message: Message,
    signature: DelegatedSignature,
}

impl FilecoinSignedMessage {
    /// Returns the unsigned Filecoin message.
    pub const fn message(&self) -> &Message {
        &self.message
    }

    /// Returns the delegated Filecoin signature.
    pub const fn signature(&self) -> &DelegatedSignature {
        &self.signature
    }
}

impl Serialize for FilecoinSignedMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tuple = serializer.serialize_tuple(2)?;
        tuple.serialize_element(&self.message)?;
        tuple.serialize_element(&self.signature)?;
        tuple.end()
    }
}
/// Returns the exact on-chain length of a delegated Filecoin signed message.
///
/// Delegated signatures always contain 65 Ethereum signature bytes plus the Filecoin type byte,
/// so their contents do not affect DAG-CBOR length. This helper uses the same canonical
/// [`FilecoinSignedMessage`] serializer as [`decode_signed`] and is suitable for unsigned
/// simulation messages whose inclusion gas still needs the signed-message wire length.
pub fn delegated_chain_length(message: &Message) -> Result<usize, BridgeError> {
    let signed_message = FilecoinSignedMessage {
        message: message.clone(),
        signature: DelegatedSignature { bytes: [0; DELEGATED_SIGNATURE_LENGTH] },
    };
    to_vec(&signed_message)
        .map_err(|source| BridgeError::Serialization { source })
        .map(|encoded| encoded.len())
}

/// A validated Ethereum transaction and its deterministic Filecoin representation.
#[derive(Clone, Debug)]
pub struct BridgedTransaction {
    tx_hash: B256,
    sender: EthereumAddress,
    kind: TxKind,
    value: U256,
    input: Bytes,
    chain_id: u64,
    raw_envelope: Bytes,
    nonce: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signed_message: FilecoinSignedMessage,
    message_serialized_len: usize,
    serialized_len: usize,
}

impl BridgedTransaction {
    /// Returns the Ethereum transaction hash.
    pub const fn tx_hash(&self) -> B256 {
        self.tx_hash
    }

    /// Returns the recovered Ethereum sender.
    pub const fn sender(&self) -> EthereumAddress {
        self.sender
    }
    /// Returns whether this transaction calls a recipient or creates a contract.
    pub const fn kind(&self) -> TxKind {
        self.kind
    }

    /// Returns the recipient, or `None` for a contract-creation transaction.
    pub const fn recipient(&self) -> Option<EthereumAddress> {
        match self.kind {
            TxKind::Create => None,
            TxKind::Call(recipient) => Some(recipient),
        }
    }

    /// Returns the amount of attoFIL transferred by the Ethereum transaction.
    pub const fn value(&self) -> U256 {
        self.value
    }

    /// Returns the Ethereum initcode or contract calldata.
    pub const fn input(&self) -> &Bytes {
        &self.input
    }

    /// Returns the EIP-155 chain ID committed to by the signature.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the canonical EIP-2718 envelope exactly as submitted.
    pub const fn raw_envelope(&self) -> &Bytes {
        &self.raw_envelope
    }

    /// Returns the Ethereum sender nonce.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Returns the Ethereum gas limit.
    pub const fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    /// Returns the EIP-1559 maximum fee per gas.
    pub const fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    /// Returns the EIP-1559 maximum priority fee per gas.
    pub const fn max_priority_fee_per_gas(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    /// Returns the canonical delegated Filecoin signed message.
    pub const fn signed_message(&self) -> &FilecoinSignedMessage {
        &self.signed_message
    }
    /// Returns the exact DAG-CBOR encoded length of the unsigned Filecoin message.
    pub const fn filecoin_message_len(&self) -> usize {
        self.message_serialized_len
    }

    /// Returns the Filecoin delegated signature payload length, including its type byte.
    pub const fn filecoin_signature_len(&self) -> usize {
        DELEGATED_SIGNATURE_LENGTH + 1
    }

    /// Returns the exact DAG-CBOR encoded length of the Filecoin signed message.
    pub const fn serialized_len(&self) -> usize {
        self.serialized_len
    }
    /// Returns the Filecoin on-chain length charged by the FVM.
    ///
    /// Delegated messages include the signature, so this is the encoded signed-message length.
    pub const fn chain_length(&self) -> usize {
        self.serialized_len
    }
}

/// Decodes and validates one signed EIP-1559 envelope, then deterministically maps it to Filecoin.
///
/// The field and method mapping follows the Forest transaction bridge and FIP-0091. Forest's
/// implementation is Copyright 2019-2026 ChainSafe Systems under Apache-2.0/MIT.
pub fn decode_signed(
    raw: &[u8],
    expected_chain_id: u64,
) -> Result<BridgedTransaction, BridgeError> {
    classify_envelope(raw)?;

    let envelope = TxEnvelope::decode_2718_exact(raw)
        .map_err(|source| BridgeError::MalformedEnvelope { source })?;
    let signed = match envelope {
        TxEnvelope::Eip1559(signed) => signed,
        _ => unreachable!("the EIP-1559 type byte was checked before decoding"),
    };
    let transaction = signed.tx();

    if transaction.chain_id != expected_chain_id {
        return Err(BridgeError::WrongChainId {
            expected: expected_chain_id,
            actual: transaction.chain_id,
        });
    }
    if !transaction.access_list.is_empty() {
        return Err(BridgeError::UnsupportedAccessList { entries: transaction.access_list.len() });
    }

    let sender = SignerRecoverable::recover_signer(&signed)
        .map_err(|source| BridgeError::InvalidSignature { source })?;
    let signed_message = filecoin_signed_message(transaction, sender, signed.signature().as_rsy())?;
    let message_serialized_len = to_vec(signed_message.message())
        .map_err(|source| BridgeError::Serialization { source })?
        .len();
    let serialized_len =
        to_vec(&signed_message).map_err(|source| BridgeError::Serialization { source })?.len();

    Ok(BridgedTransaction {
        tx_hash: *signed.hash(),
        sender,
        kind: transaction.to,
        value: transaction.value,
        input: transaction.input.clone(),
        chain_id: transaction.chain_id,
        raw_envelope: Bytes::copy_from_slice(raw),
        nonce: transaction.nonce,
        gas_limit: transaction.gas_limit,
        max_fee_per_gas: transaction.max_fee_per_gas,
        max_priority_fee_per_gas: transaction.max_priority_fee_per_gas,
        signed_message,
        message_serialized_len,
        serialized_len,
    })
}

fn classify_envelope(raw: &[u8]) -> Result<(), BridgeError> {
    let Some(&transaction_type) = raw.first() else {
        return Err(BridgeError::EmptyEnvelope);
    };
    if transaction_type == EIP_1559_TRANSACTION_TYPE {
        return Ok(());
    }
    if transaction_type >= 0xc0 {
        return Err(BridgeError::UnsupportedLegacy);
    }
    Err(BridgeError::UnsupportedTransactionType { transaction_type })
}

fn filecoin_signed_message(
    transaction: &TxEip1559,
    sender: EthereumAddress,
    signature: [u8; DELEGATED_SIGNATURE_LENGTH],
) -> Result<FilecoinSignedMessage, BridgeError> {
    let (to, method_num) = match transaction.to {
        TxKind::Create => (FilecoinAddress::new_id(EAM_ACTOR_ID), EAM_CREATE_EXTERNAL_METHOD),
        TxKind::Call(recipient) => (delegated_address(recipient)?, EVM_INVOKE_CONTRACT_METHOD),
    };
    let params = encode_calldata_inner(transaction.input.as_ref())
        .map_err(|source| BridgeError::Serialization { source })?;
    let message = Message {
        version: 0,
        from: delegated_address(sender)?,
        to,
        sequence: transaction.nonce,
        value: u256_token_amount(transaction.value),
        method_num,
        params,
        gas_limit: transaction.gas_limit,
        gas_fee_cap: TokenAmount::from_atto(transaction.max_fee_per_gas),
        gas_premium: TokenAmount::from_atto(transaction.max_priority_fee_per_gas),
    };

    Ok(FilecoinSignedMessage { message, signature: DelegatedSignature { bytes: signature } })
}
/// Ethereum transaction fields needed to construct an unsigned FEVM call or gas-estimation
/// message.
///
/// These fields intentionally use Alloy primitives rather than RPC response types. A gas limit of
/// zero is preserved as the FVM gas-estimation sentinel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnsignedCall {
    /// Ethereum caller. The zero address is a valid delegated EAM address.
    pub from: EthereumAddress,
    /// Existing contract recipient, or `None` for EAM `CreateExternal`.
    pub to: Option<EthereumAddress>,
    /// Amount of attoFIL transferred by the call.
    pub value: U256,
    /// Contract calldata or creation initcode.
    pub input: Bytes,
    /// FEVM gas limit; zero selects estimation behavior.
    pub gas_limit: u64,
    /// Sender sequence used when execution requires a specific nonce.
    pub nonce: u64,
    /// Maximum fee per gas in attoFIL.
    pub max_fee_per_gas: U256,
    /// Maximum priority fee per gas in attoFIL.
    pub max_priority_fee_per_gas: U256,
}

/// Encodes Ethereum calldata as the strict CBOR byte string accepted by EAM and EVM actors.
///
/// Forest's `EthCallMessage::convert_data_to_message_params` uses this representation and leaves
/// empty calldata as empty `RawBytes`. Forest is Copyright 2019-2026 ChainSafe Systems under
/// Apache-2.0/MIT.
pub fn encode_calldata(input: &[u8]) -> Result<RawBytes, BridgeError> {
    encode_calldata_inner(input)
        .map_err(|source| BridgeError::MalformedCall { source: CallError::Calldata { source } })
}

/// Maps an unsigned Ethereum call to a pure Filecoin message.
///
/// Recipient calls use EVM `InvokeEVM`; creations use EAM `CreateExternal`, matching Forest's
/// `EthCallMessage -> Message` conversion (Copyright 2019-2026 ChainSafe Systems,
/// Apache-2.0/MIT).
pub fn build_unsigned_message(call: &UnsignedCall) -> Result<Message, BridgeError> {
    if call.gas_limit > i64::MAX as u64 {
        return Err(BridgeError::MalformedCall {
            source: CallError::GasLimitOutOfRange { gas_limit: call.gas_limit },
        });
    }

    let address = |address: EthereumAddress| {
        FilecoinAddress::new_delegated(EAM_ACTOR_ID, address.as_slice())
            .map_err(|source| BridgeError::MalformedCall { source: CallError::Address { source } })
    };
    let (to, method_num) = match call.to {
        Some(recipient) => (address(recipient)?, EVM_INVOKE_CONTRACT_METHOD),
        None => (FilecoinAddress::new_id(EAM_ACTOR_ID), EAM_CREATE_EXTERNAL_METHOD),
    };

    Ok(Message {
        version: 0,
        from: address(call.from)?,
        to,
        sequence: call.nonce,
        value: u256_token_amount(call.value),
        method_num,
        params: encode_calldata(call.input.as_ref())?,
        gas_limit: call.gas_limit,
        gas_fee_cap: u256_token_amount(call.max_fee_per_gas),
        gas_premium: u256_token_amount(call.max_priority_fee_per_gas),
    })
}

/// Ethereum-visible data returned by one successful FEVM execution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionReturn {
    /// ABI return bytes for an invocation. Contract creation has no output bytes.
    pub output: Bytes,
    /// Created Ethereum contract address for EAM `CreateExternal`.
    pub contract_address: Option<EthereumAddress>,
}

impl ExecutionReturn {
    /// Returns the ABI output bytes.
    pub const fn output(&self) -> &Bytes {
        &self.output
    }

    /// Returns the created Ethereum address, if this was EAM `CreateExternal`.
    pub const fn contract_address(&self) -> Option<EthereumAddress> {
        self.contract_address
    }
}

/// Decodes successful FEVM receipt return data into Ethereum-visible bytes or a created address.
///
/// Invoke returns are strict CBOR byte strings. EAM returns are the builtin-actors
/// `CreateExternalReturn` tuple `(actor_id, robust_address, eth_address)`, duplicated locally to
/// keep actor crates out of this crate's production graph. This follows Forest's `eth_call` and
/// receipt conversion (Copyright 2019-2026 ChainSafe Systems, Apache-2.0/MIT) and the
/// builtin-actors EAM wire schema (Protocol Labs and Filecoin Core Devs, Apache-2.0/MIT).
pub fn decode_execution_return(
    kind: ExecutionKind,
    raw: &RawBytes,
) -> Result<ExecutionReturn, BridgeError> {
    match kind {
        ExecutionKind::InvokeContract if raw.is_empty() => Ok(ExecutionReturn::default()),
        ExecutionKind::InvokeContract => {
            let output: BytesDe = from_slice(raw.bytes())
                .map_err(|source| BridgeError::MalformedReturn { kind, source })?;
            Ok(ExecutionReturn { output: Bytes::from(output.0), contract_address: None })
        }
        ExecutionKind::CreateExternal => {
            let output: EamCreateExternalReturn = from_slice(raw.bytes())
                .map_err(|source| BridgeError::MalformedReturn { kind, source })?;
            Ok(ExecutionReturn {
                output: Bytes::new(),
                contract_address: Some(EthereumAddress::from(output.eth_address)),
            })
        }
    }
}
/// Unwraps failed FEVM receipt return data into the exact EVM revert bytes.
///
/// FEVM wraps both Solidity `Error(string)` payloads and custom-error payloads in a strict CBOR
/// byte string. Empty receipt data remains empty. This follows Forest's `decode_revert_reason`
/// payload unwrapping without interpreting or modifying the ABI bytes. Forest is Copyright
/// 2019-2026 ChainSafe Systems under Apache-2.0/MIT.
pub fn decode_revert_data(raw: &RawBytes) -> Result<Bytes, BridgeError> {
    if raw.is_empty() {
        return Ok(Bytes::new());
    }

    let data: BytesDe =
        from_slice(raw.bytes()).map_err(|source| BridgeError::MalformedRevert { source })?;
    Ok(Bytes::from(data.0))
}

/// Ethereum projection of an FVM actor event after its emitter has been resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EthereumLog {
    /// Resolved Ethereum emitter address.
    pub address: EthereumAddress,
    /// Contiguous topics from `t1` through at most `t4`.
    pub topics: Vec<B256>,
    /// Raw bytes from the optional `d` entry.
    pub data: Bytes,
}

impl EthereumLog {
    /// Returns the resolved emitter address.
    pub const fn address(&self) -> EthereumAddress {
        self.address
    }

    /// Returns the projected Ethereum topics.
    pub fn topics(&self) -> &[B256] {
        &self.topics
    }

    /// Returns the projected Ethereum data.
    pub const fn data(&self) -> &Bytes {
        &self.data
    }
}

/// Projects the EVM event-entry convention into an Ethereum log.
///
/// This follows Forest's `eth_log_from_event`: all entries must use `IPLD_RAW`, topic keys are
/// `t1` through `t4`, topics are 32 bytes and contiguous, a single `d` entry supplies data, and
/// unknown raw entries are ignored for forward compatibility. Forest is Copyright 2019-2026
/// ChainSafe Systems under Apache-2.0/MIT.
pub fn project_event(
    emitter: EthereumAddress,
    event: &ActorEvent,
) -> Result<EthereumLog, BridgeError> {
    let mut topics = [None; 4];
    let mut data = None;

    for entry in &event.entries {
        if entry.codec != IPLD_RAW {
            return Err(BridgeError::MalformedEvent {
                source: EventError::UnsupportedCodec { key: entry.key.clone(), codec: entry.codec },
            });
        }

        if let Some(index) = topic_index(&entry.key) {
            let topic: [u8; 32] =
                entry.value.as_slice().try_into().map_err(|_| BridgeError::MalformedEvent {
                    source: EventError::InvalidTopicLength {
                        key: entry.key.clone(),
                        actual: entry.value.len(),
                    },
                })?;
            if topics[index].replace(B256::from(topic)).is_some() {
                return Err(BridgeError::MalformedEvent {
                    source: EventError::DuplicateTopic { index: index + 1 },
                });
            }
        } else if entry.key == "d" && data.replace(Bytes::copy_from_slice(&entry.value)).is_some() {
            return Err(BridgeError::MalformedEvent { source: EventError::DuplicateData });
        }
    }

    let topic_count = topics.iter().rposition(Option::is_some).map_or(0, |index| index + 1);
    let mut projected_topics = Vec::with_capacity(topic_count);
    for (index, topic) in topics.into_iter().take(topic_count).enumerate() {
        projected_topics.push(topic.ok_or_else(|| BridgeError::MalformedEvent {
            source: EventError::MissingTopic { index: index + 1 },
        })?);
    }

    Ok(EthereumLog { address: emitter, topics: projected_topics, data: data.unwrap_or_default() })
}

#[derive(Deserialize_tuple)]
struct EamCreateExternalReturn {
    _actor_id: u64,
    _robust_address: Option<FilecoinAddress>,
    #[serde(with = "strict_bytes")]
    eth_address: [u8; 20],
}

fn topic_index(key: &str) -> Option<usize> {
    match key {
        "t1" => Some(0),
        "t2" => Some(1),
        "t3" => Some(2),
        "t4" => Some(3),
        _ => None,
    }
}

fn encode_calldata_inner(input: &[u8]) -> Result<RawBytes, fvm_ipld_encoding::Error> {
    if input.is_empty() {
        Ok(RawBytes::default())
    } else {
        to_vec(&CborByteString(input)).map(RawBytes::new)
    }
}

fn delegated_address(address: EthereumAddress) -> Result<FilecoinAddress, BridgeError> {
    FilecoinAddress::new_delegated(EAM_ACTOR_ID, address.as_slice())
        .map_err(|source| BridgeError::Address { source })
}

fn u256_token_amount(value: U256) -> TokenAmount {
    TokenAmount::from_atto(BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>()))
}

struct CborByteString<'a>(&'a [u8]);

impl Serialize for CborByteString<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, hex};
    use fvm_shared::{
        address::Payload,
        event::{ActorEvent, Entry, Flags},
    };

    // Vector adapted from Forest's `eth::transaction` tests (Apache-2.0/MIT).
    const INVOCATION: &[u8] = &hex!(
        "02f901368304cb2f8201e68459682f008459682f7884023b53a794eb4a9cdb9f42d3a503d580a39b6e3736eb21fffd80b8c4383487be000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000660d4d120000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000003b6261666b726569656f6f75326d36356276376561786e7767656d7562723675787269696867366474646e6c7a663469616f37686c6e6a6d647372750000000000c001a0b9f0afb3fa8821fa414bac6056e613c61a8263ca341b59539096dbbc8600f530a0114a6a032347e132f115accc7664ccc61549be28f5b844c3fc170006feb72f24"
    );
    // Generated with Foundry `cast mktx` from its first documented local development key.
    const CREATION: &[u8] = &hex!(
        "02f85d827a69078405f5e10084773594008307a120802a8460006000c080a09d77b26d8db24c230a554a1466f98ac8cb4f0173bb026e7596579c27ff6487f4a05cfd5ce9321e094100bd6f57b284bd2e5168268a74db24ef81c80ca722912b80"
    );

    #[test]
    fn invocation_recovers_fixed_sender_and_hash() {
        let bridged = decode_signed(INVOCATION, 314_159).unwrap();

        assert_eq!(bridged.sender(), address!("4fda4174d5d07c906395bfb77806287cc65fd129"));
        assert_eq!(
            bridged.tx_hash(),
            b256!("9f2e70d5737c6b798eccea14895893fb48091ab3c59d0fe95508dc7efdae2e5f")
        );
        assert_eq!(bridged.nonce(), 486);
        assert_eq!(bridged.gas_limit(), 37_442_471);
        assert_eq!(bridged.max_fee_per_gas(), 1_500_000_120);
        assert_eq!(bridged.max_priority_fee_per_gas(), 1_500_000_000);
    }
    #[test]
    fn exposes_signed_transaction_facts_without_redecoding() {
        let invocation = decode_signed(INVOCATION, 314_159).unwrap();
        assert_eq!(
            invocation.kind(),
            TxKind::Call(address!("eb4a9cdb9f42d3a503d580a39b6e3736eb21fffd"))
        );
        assert_eq!(
            invocation.recipient(),
            Some(address!("eb4a9cdb9f42d3a503d580a39b6e3736eb21fffd"))
        );
        assert_eq!(invocation.value(), U256::ZERO);
        assert_eq!(invocation.input().as_ref(), &INVOCATION[50..246]);
        assert_eq!(invocation.chain_id(), 314_159);
        assert_eq!(invocation.raw_envelope().as_ref(), INVOCATION);

        let creation = decode_signed(CREATION, 31_337).unwrap();
        assert_eq!(creation.kind(), TxKind::Create);
        assert_eq!(creation.recipient(), None);
        assert_eq!(creation.value(), U256::from(42));
        assert_eq!(creation.input().as_ref(), hex!("60006000"));
        assert_eq!(creation.chain_id(), 31_337);
        assert_eq!(creation.raw_envelope().as_ref(), CREATION);
    }

    #[test]
    fn invocation_maps_exact_filecoin_fields() {
        let bridged = decode_signed(INVOCATION, 314_159).unwrap();
        let message = bridged.signed_message().message();

        assert_delegated(message.from, address!("4fda4174d5d07c906395bfb77806287cc65fd129"));
        assert_delegated(message.to, address!("eb4a9cdb9f42d3a503d580a39b6e3736eb21fffd"));
        assert_eq!(message.version, 0);
        assert_eq!(message.sequence, 486);
        assert_eq!(message.value, TokenAmount::from_atto(0));
        assert_eq!(message.method_num, EVM_INVOKE_CONTRACT_METHOD);
        assert_eq!(message.gas_limit, 37_442_471);
        assert_eq!(message.gas_fee_cap, TokenAmount::from_atto(1_500_000_120_u64));
        assert_eq!(message.gas_premium, TokenAmount::from_atto(1_500_000_000_u64));
        assert_eq!(message.params.bytes(), invocation_params());
    }

    #[test]
    fn creation_routes_to_eam_create_external() {
        let bridged = decode_signed(CREATION, 31_337).unwrap();
        let message = bridged.signed_message().message();

        assert_eq!(bridged.sender(), address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266"));
        assert_eq!(
            bridged.tx_hash(),
            b256!("500067898036a7cfe8c2abdaceae617f566a9154a19efa51622735b6864a82f8")
        );
        assert_eq!(message.to, FilecoinAddress::new_id(EAM_ACTOR_ID));
        assert_eq!(message.method_num, EAM_CREATE_EXTERNAL_METHOD);
        assert_eq!(message.sequence, 7);
        assert_eq!(message.value, TokenAmount::from_atto(42));
        assert_eq!(message.gas_limit, 500_000);
        assert_eq!(message.gas_fee_cap, TokenAmount::from_atto(2_000_000_000_u64));
        assert_eq!(message.gas_premium, TokenAmount::from_atto(100_000_000_u64));
        assert_eq!(message.params.bytes(), hex!("4460006000"));
    }

    #[test]
    fn delegated_signature_is_exact_r_s_y_parity() {
        let bridged = decode_signed(CREATION, 31_337).unwrap();
        let signature = bridged.signed_message().signature();

        assert_eq!(signature.signature_type(), DELEGATED_SIGNATURE_TYPE);
        assert_eq!(
            signature.bytes(),
            &hex!(
                "9d77b26d8db24c230a554a1466f98ac8cb4f0173bb026e7596579c27ff6487f45cfd5ce9321e094100bd6f57b284bd2e5168268a74db24ef81c80ca722912b8000"
            )
        );
    }

    #[test]
    fn rejects_wrong_chain_id() {
        assert!(matches!(
            decode_signed(CREATION, 1),
            Err(BridgeError::WrongChainId { expected: 1, actual: 31_337 })
        ));
    }

    #[test]
    fn rejects_malformed_envelope_and_signature() {
        assert!(matches!(decode_signed(&[], 31_337), Err(BridgeError::EmptyEnvelope)));
        assert!(matches!(
            decode_signed(&CREATION[..CREATION.len() - 1], 31_337),
            Err(BridgeError::MalformedEnvelope { .. })
        ));

        let mut malformed_parity = CREATION.to_vec();
        malformed_parity[29] = 2;
        assert!(matches!(
            decode_signed(&malformed_parity, 31_337),
            Err(BridgeError::MalformedEnvelope { .. })
        ));

        let mut invalid_signature = CREATION.to_vec();
        invalid_signature[31..63].copy_from_slice(&hex!(
            "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
        ));
        assert!(matches!(
            decode_signed(&invalid_signature, 31_337),
            Err(BridgeError::InvalidSignature { .. })
        ));
    }

    #[test]
    fn rejects_unsupported_envelope_types() {
        assert!(matches!(
            decode_signed(&hex!("01c0"), 31_337),
            Err(BridgeError::UnsupportedTransactionType { transaction_type: 1 })
        ));
        assert!(matches!(decode_signed(&hex!("c0"), 31_337), Err(BridgeError::UnsupportedLegacy)));
    }

    #[test]
    fn reports_exact_serialized_signed_message_length() {
        let invocation = decode_signed(INVOCATION, 314_159).unwrap();
        let creation = decode_signed(CREATION, 31_337).unwrap();
        assert_eq!(invocation.filecoin_message_len(), 274);
        assert_eq!(creation.filecoin_message_len(), 56);
        assert_eq!(invocation.filecoin_signature_len(), 66);
        assert_eq!(creation.filecoin_signature_len(), 66);
        assert_eq!(invocation.chain_length(), 343);
        assert_eq!(creation.chain_length(), 125);

        assert_eq!(invocation.serialized_len(), 343);
        assert_eq!(creation.serialized_len(), 125);

        assert_eq!(invocation.serialized_len(), to_vec(invocation.signed_message()).unwrap().len());
        assert_eq!(creation.serialized_len(), to_vec(creation.signed_message()).unwrap().len());
        assert_eq!(
            delegated_chain_length(invocation.signed_message().message()).unwrap(),
            invocation.serialized_len()
        );
        assert_eq!(
            delegated_chain_length(creation.signed_message().message()).unwrap(),
            creation.serialized_len()
        );
    }
    #[test]
    fn encodes_calldata_with_exact_cbor_byte_string_headers() {
        assert!(encode_calldata(&[]).unwrap().is_empty());
        assert_eq!(encode_calldata(&hex!("60006000")).unwrap().bytes(), hex!("4460006000"));
        assert_eq!(
            encode_calldata(&[0xaa; 24]).unwrap().bytes(),
            hex!("5818aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn unsigned_calls_share_create_and_invoke_routing() {
        let invocation = UnsignedCall {
            from: address!("1111111111111111111111111111111111111111"),
            to: Some(address!("2222222222222222222222222222222222222222")),
            value: U256::from(9),
            input: Bytes::from(vec![1, 2, 3, 4]),
            gas_limit: 1_000_000,
            nonce: 7,
            max_fee_per_gas: U256::from(12),
            max_priority_fee_per_gas: U256::from(3),
        };
        let invoke_message = build_unsigned_message(&invocation).unwrap();
        assert_delegated(invoke_message.from, invocation.from);
        assert_delegated(invoke_message.to, invocation.to.unwrap());
        assert_eq!(invoke_message.method_num, EVM_INVOKE_CONTRACT_METHOD);
        assert_eq!(invoke_message.params.bytes(), hex!("4401020304"));
        assert_eq!(invoke_message.value, TokenAmount::from_atto(9));
        assert_eq!(invoke_message.gas_fee_cap, TokenAmount::from_atto(12));
        assert_eq!(invoke_message.gas_premium, TokenAmount::from_atto(3));

        let create_message = build_unsigned_message(&UnsignedCall {
            to: None,
            input: Bytes::from(vec![0x60, 0x00]),
            ..invocation
        })
        .unwrap();
        assert_eq!(create_message.to, FilecoinAddress::new_id(EAM_ACTOR_ID));
        assert_eq!(create_message.method_num, EAM_CREATE_EXTERNAL_METHOD);
        assert_eq!(create_message.params.bytes(), hex!("426000"));

        assert!(matches!(
            build_unsigned_message(&UnsignedCall {
                gas_limit: i64::MAX as u64 + 1,
                ..UnsignedCall::default()
            }),
            Err(BridgeError::MalformedCall { source: CallError::GasLimitOutOfRange { .. } })
        ));
    }

    #[test]
    fn decodes_invoke_and_create_returns_by_wire_schema() {
        let invocation = decode_execution_return(
            ExecutionKind::InvokeContract,
            &RawBytes::new(hex!("44deadbeef").to_vec()),
        )
        .unwrap();
        assert_eq!(invocation.output(), &Bytes::from(hex!("deadbeef").to_vec()));
        assert_eq!(invocation.contract_address(), None);

        let empty =
            decode_execution_return(ExecutionKind::InvokeContract, &RawBytes::default()).unwrap();
        assert!(empty.output().is_empty());

        let creation = decode_execution_return(
            ExecutionKind::CreateExternal,
            &RawBytes::new(hex!("831864f65400112233445566778899aabbccddeeff00112233").to_vec()),
        )
        .unwrap();
        assert!(creation.output().is_empty());
        assert_eq!(
            creation.contract_address(),
            Some(address!("00112233445566778899aabbccddeeff00112233"))
        );

        for (kind, malformed) in [
            (ExecutionKind::InvokeContract, RawBytes::new(vec![0x82, 0x01, 0x02])),
            (ExecutionKind::CreateExternal, RawBytes::default()),
        ] {
            assert!(matches!(
                decode_execution_return(kind, &malformed),
                Err(BridgeError::MalformedReturn { kind: actual, .. }) if actual == kind
            ));
        }
    }
    #[test]
    fn decodes_fixed_error_and_custom_revert_payloads_losslessly() {
        let error = RawBytes::new(
            hex!(
                "586408c379a0"
                "0000000000000000000000000000000000000000000000000000000000000020"
                "0000000000000000000000000000000000000000000000000000000000000004"
                "626f6f6d00000000000000000000000000000000000000000000000000000000"
            )
            .to_vec(),
        );
        assert_eq!(
            decode_revert_data(&error).unwrap().as_ref(),
            hex!(
                "08c379a0"
                "0000000000000000000000000000000000000000000000000000000000000020"
                "0000000000000000000000000000000000000000000000000000000000000004"
                "626f6f6d00000000000000000000000000000000000000000000000000000000"
            )
        );

        let custom = RawBytes::new(hex!("46deadbeef0102").to_vec());
        assert_eq!(decode_revert_data(&custom).unwrap().as_ref(), hex!("deadbeef0102"));
        assert!(decode_revert_data(&RawBytes::default()).unwrap().is_empty());

        for malformed in [
            RawBytes::new(hex!("820102").to_vec()),
            RawBytes::new(hex!("626f6f").to_vec()),
            RawBytes::new(hex!("41ff00").to_vec()),
        ] {
            assert!(matches!(
                decode_revert_data(&malformed),
                Err(BridgeError::MalformedRevert { .. })
            ));
        }
    }

    #[test]
    fn ignores_only_exactly_unknown_topic_keys_after_codec_validation() {
        let event = ActorEvent::from(vec![
            event_entry("t10", vec![0xaa]),
            event_entry("t1-extra", vec![0xbb]),
            event_entry("t1", vec![0x11; 32]),
        ]);
        let log = project_event(EthereumAddress::ZERO, &event).unwrap();
        assert_eq!(log.topics(), &[B256::from([0x11; 32])]);

        let mut non_raw_unknown = event_entry("t10", vec![0xaa]);
        non_raw_unknown.codec = fvm_ipld_encoding::DAG_CBOR;
        assert!(matches!(
            project_event(EthereumAddress::ZERO, &ActorEvent::from(vec![non_raw_unknown])),
            Err(BridgeError::MalformedEvent {
                source: EventError::UnsupportedCodec { key, .. }
            }) if key == "t10"
        ));
    }

    #[test]
    fn projects_only_ethereum_topic_and_data_entries() {
        let emitter = address!("9999999999999999999999999999999999999999");
        let event = ActorEvent::from(vec![
            event_entry("ignored", vec![0xff]),
            event_entry("t2", vec![0x22; 32]),
            event_entry("d", vec![0xde, 0xad, 0xbe, 0xef]),
            event_entry("t1", vec![0x11; 32]),
        ]);
        let log = project_event(emitter, &event).unwrap();
        assert_eq!(log.address(), emitter);
        assert_eq!(log.topics(), &[B256::from([0x11; 32]), B256::from([0x22; 32])]);
        assert_eq!(log.data().as_ref(), hex!("deadbeef"));
    }

    #[test]
    fn rejects_malformed_ethereum_event_conventions() {
        let emitter = EthereumAddress::ZERO;
        let missing_topic =
            ActorEvent::from(vec![event_entry("t1", vec![0; 32]), event_entry("t3", vec![0; 32])]);
        assert!(matches!(
            project_event(emitter, &missing_topic),
            Err(BridgeError::MalformedEvent { source: EventError::MissingTopic { index: 2 } })
        ));

        let short_topic = ActorEvent::from(vec![event_entry("t1", vec![0; 31])]);
        assert!(matches!(
            project_event(emitter, &short_topic),
            Err(BridgeError::MalformedEvent {
                source: EventError::InvalidTopicLength { actual: 31, .. }
            })
        ));

        let duplicate_data =
            ActorEvent::from(vec![event_entry("d", vec![1]), event_entry("d", vec![2])]);
        assert!(matches!(
            project_event(emitter, &duplicate_data),
            Err(BridgeError::MalformedEvent { source: EventError::DuplicateData })
        ));

        let mut non_raw = event_entry("t1", vec![0; 32]);
        non_raw.codec = fvm_ipld_encoding::DAG_CBOR;
        assert!(matches!(
            project_event(emitter, &ActorEvent::from(vec![non_raw])),
            Err(BridgeError::MalformedEvent { source: EventError::UnsupportedCodec { .. } })
        ));
    }

    fn event_entry(key: &str, value: Vec<u8>) -> Entry {
        Entry { flags: Flags::FLAG_INDEXED_ALL, key: key.to_owned(), codec: IPLD_RAW, value }
    }

    fn assert_delegated(filecoin: FilecoinAddress, ethereum: EthereumAddress) {
        let Payload::Delegated(payload) = filecoin.payload() else {
            panic!("expected delegated address");
        };
        assert_eq!(payload.namespace(), EAM_ACTOR_ID);
        assert_eq!(payload.subaddress(), ethereum.as_slice());
    }

    fn invocation_params() -> Vec<u8> {
        let input = &INVOCATION[50..246];
        let mut expected = Vec::with_capacity(input.len() + 2);
        expected.extend_from_slice(&[0x58, 0xc4]);
        expected.extend_from_slice(input);
        expected
    }
}
