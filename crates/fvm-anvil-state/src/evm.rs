// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builtin-actors v18 EVM state mutation and inspection.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::LazyLock;

use alloy_primitives::{U256, keccak256};
use cid::Cid;
use fvm_ipld_blockstore::{Block, Blockstore, MemoryBlockstore};
use fvm_ipld_encoding::{CborStore, IPLD_RAW, tuple::*};
use fvm_ipld_kamt::{AsHashedKey, Config as KamtConfig, HashedKey, Kamt};
use multihash_codetable::Code;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::prepared::{PreparedBytecodePatch, PreparedStoragePatch};

const EVM_ACTOR_CODE_V18: &str = "bafk2bzacea57ib7fosqleehx5z6uvockk3xk5wqycmge5m4rfq4ptr7xrkfsi";
const MAX_CODE_SIZE: usize = 24 << 10;
const EVM_STATE_KAMT_CONFIG_V18: KamtConfig =
    KamtConfig { min_data_depth: 0, bit_width: 5, max_array_width: 1 };
static EVM_ACTOR_CODE: LazyLock<Cid> = LazyLock::new(|| {
    Cid::from_str(EVM_ACTOR_CODE_V18).expect("builtin-actors v18 EVM code CID is valid")
});

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

struct StateHashAlgorithmV18;

impl AsHashedKey<EvmWordV18, 32> for StateHashAlgorithmV18 {
    fn as_hashed_key(key: &EvmWordV18) -> Cow<'_, HashedKey<32>> {
        Cow::Owned(key.0.to_be_bytes())
    }
}

type EvmStateKamtV18<BS> = Kamt<BS, EvmWordV18, EvmWordV18, StateHashAlgorithmV18>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct BytecodeHashV18(#[serde(with = "fvm_ipld_encoding::strict_bytes")] [u8; 32]);

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct TransientDataV18 {
    transient_data_state: Cid,
    transient_data_lifespan: TransientDataLifespanV18,
}

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct TransientDataLifespanV18 {
    origin: u64,
    nonce: u64,
}

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct TombstoneV18 {
    origin: u64,
    nonce: u64,
}

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct EvmStateV18 {
    bytecode: Cid,
    bytecode_hash: BytecodeHashV18,
    contract_state: Cid,
    transient_data: Option<TransientDataV18>,
    nonce: u64,
    tombstone: Option<TombstoneV18>,
}

struct PatchStore<'a, BS> {
    source: &'a BS,
    output: Rc<MemoryBlockstore>,
}

impl<BS> Clone for PatchStore<'_, BS> {
    fn clone(&self) -> Self {
        Self { source: self.source, output: Rc::clone(&self.output) }
    }
}

impl<BS: Blockstore> Blockstore for PatchStore<'_, BS> {
    fn get(&self, cid: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
        match self.output.get(cid)? {
            Some(block) => Ok(Some(block)),
            None => self.source.get(cid),
        }
    }

    fn put_keyed(&self, cid: &Cid, block: &[u8]) -> anyhow::Result<()> {
        self.output.put_keyed(cid, block)
    }
}

#[derive(Debug)]
pub(crate) struct AppliedEvmPatch {
    pub state: Cid,
    pub runtime_code: Option<PreparedBytecodePatch>,
    pub storage: Vec<PreparedStoragePatch>,
}

pub(crate) struct EvmStateView {
    pub bytecode: Cid,
    pub bytecode_hash: [u8; 32],
    pub contract_state: Cid,
}

#[derive(Debug, Error)]
pub(crate) enum EvmPatchError {
    #[error("actor code {0} is not builtin-actors v18 EVM code")]
    NotEvmActor(Box<Cid>),
    #[error("EVM state block {0} is missing")]
    MissingState(Box<Cid>),
    #[error("failed to load EVM state {state}: {message}")]
    State { state: Box<Cid>, message: String },
    #[error("replacement bytecode is {actual} bytes; maximum is {maximum}")]
    CodeTooLarge { actual: usize, maximum: usize },
    #[error("replacement bytecode starts with reserved byte 0xef")]
    ReservedCodePrefix,
    #[error("failed to write replacement bytecode: {0}")]
    Bytecode(String),
    #[error("failed to patch EVM storage: {0}")]
    Storage(String),
}

pub(crate) fn is_evm_actor_code(code: &Cid) -> bool {
    code == &*EVM_ACTOR_CODE
}

pub(crate) fn validate_runtime_code(bytecode: &[u8]) -> Result<(), EvmPatchError> {
    if bytecode.len() > MAX_CODE_SIZE {
        return Err(EvmPatchError::CodeTooLarge { actual: bytecode.len(), maximum: MAX_CODE_SIZE });
    }
    if bytecode.first() == Some(&0xef) {
        return Err(EvmPatchError::ReservedCodePrefix);
    }
    Ok(())
}

pub(crate) fn apply_evm_patch<BS: Blockstore>(
    source: &BS,
    output: Rc<MemoryBlockstore>,
    actor_code: &Cid,
    state_root: &Cid,
    runtime_code: Option<&[u8]>,
    storage: &BTreeMap<[u8; 32], [u8; 32]>,
) -> Result<AppliedEvmPatch, EvmPatchError> {
    if !is_evm_actor_code(actor_code) {
        return Err(EvmPatchError::NotEvmActor(Box::new(*actor_code)));
    }
    let store = PatchStore { source, output };
    let mut state = load_state(&store, state_root)?;

    let runtime_code = if let Some(replacement) = runtime_code {
        validate_runtime_code(replacement)?;
        let replacement_hash = keccak256(replacement).0;
        let replacement_cid = store
            .put(Code::Blake2b256, &Block::new(IPLD_RAW, replacement))
            .map_err(|error| EvmPatchError::Bytecode(error.to_string()))?;
        let patch = PreparedBytecodePatch {
            source: state.bytecode,
            source_hash: state.bytecode_hash.0.to_vec(),
            replacement: replacement_cid,
            replacement_hash: replacement_hash.to_vec(),
        };
        state.bytecode = replacement_cid;
        state.bytecode_hash = BytecodeHashV18(replacement_hash);
        Some(patch)
    } else {
        None
    };

    let mut applied_storage = Vec::with_capacity(storage.len());
    if !storage.is_empty() {
        let mut slots = EvmStateKamtV18::load_with_config(
            &state.contract_state,
            store.clone(),
            EVM_STATE_KAMT_CONFIG_V18,
        )
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
        for (key, replacement) in storage {
            let key_word = EvmWordV18(U256::from_be_bytes(*key));
            let source = slots
                .get(&key_word)
                .map_err(|error| EvmPatchError::Storage(error.to_string()))?
                .copied()
                .unwrap_or_default();
            let replacement_word = EvmWordV18(U256::from_be_bytes(*replacement));
            if replacement_word.0.is_zero() {
                slots
                    .delete(&key_word)
                    .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
            } else {
                slots
                    .set(key_word, replacement_word)
                    .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
            }
            applied_storage.push(PreparedStoragePatch {
                key: key.to_vec(),
                source: source.0.to_be_bytes::<32>().to_vec(),
                replacement: replacement.to_vec(),
            });
        }
        state.contract_state =
            slots.flush().map_err(|error| EvmPatchError::Storage(error.to_string()))?;
    }

    let state = store.put_cbor(&state, Code::Blake2b256).map_err(|error| EvmPatchError::State {
        state: Box::new(*state_root),
        message: error.to_string(),
    })?;
    Ok(AppliedEvmPatch { state, runtime_code, storage: applied_storage })
}

pub(crate) fn inspect_evm_state(
    store: &impl Blockstore,
    state_root: &Cid,
) -> Result<EvmStateView, EvmPatchError> {
    let state = load_state(store, state_root)?;
    Ok(EvmStateView {
        bytecode: state.bytecode,
        bytecode_hash: state.bytecode_hash.0,
        contract_state: state.contract_state,
    })
}

pub(crate) fn read_storage(
    store: &impl Blockstore,
    state_root: &Cid,
    key: [u8; 32],
) -> Result<[u8; 32], EvmPatchError> {
    let slots = EvmStateKamtV18::load_with_config(state_root, store, EVM_STATE_KAMT_CONFIG_V18)
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
    let value = slots
        .get(&EvmWordV18(U256::from_be_bytes(key)))
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?
        .copied()
        .unwrap_or_default();
    Ok(value.0.to_be_bytes())
}

fn load_state<BS: Blockstore>(store: &BS, state_root: &Cid) -> Result<EvmStateV18, EvmPatchError> {
    store
        .get_cbor::<EvmStateV18>(state_root)
        .map_err(|error| EvmPatchError::State {
            state: Box::new(*state_root),
            message: error.to_string(),
        })?
        .ok_or_else(|| EvmPatchError::MissingState(Box::new(*state_root)))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_bytecode_and_batches_storage_updates() {
        let source = Rc::new(MemoryBlockstore::default());
        let old_code = b"old runtime";
        let old_code_cid = source.put(Code::Blake2b256, &Block::new(IPLD_RAW, old_code)).unwrap();
        let mut slots =
            EvmStateKamtV18::new_with_config(Rc::clone(&source), EVM_STATE_KAMT_CONFIG_V18);
        let replaced_key = [1_u8; 32];
        let deleted_key = [2_u8; 32];
        slots
            .set(EvmWordV18(U256::from_be_bytes(replaced_key)), EvmWordV18(U256::from(3)))
            .unwrap();
        slots.set(EvmWordV18(U256::from_be_bytes(deleted_key)), EvmWordV18(U256::from(4))).unwrap();
        let contract_state = slots.flush().unwrap();
        let source_state = source
            .put_cbor(
                &EvmStateV18 {
                    bytecode: old_code_cid,
                    bytecode_hash: BytecodeHashV18(keccak256(old_code).0),
                    contract_state,
                    transient_data: None,
                    nonce: 9,
                    tombstone: None,
                },
                Code::Blake2b256,
            )
            .unwrap();
        let replacement_code = [0x60, 0x00];
        let mut storage = BTreeMap::new();
        storage.insert(replaced_key, U256::from(5).to_be_bytes());
        storage.insert(deleted_key, [0_u8; 32]);
        let output = Rc::new(MemoryBlockstore::default());

        let applied = apply_evm_patch(
            source.as_ref(),
            Rc::clone(&output),
            &EVM_ACTOR_CODE,
            &source_state,
            Some(&replacement_code),
            &storage,
        )
        .unwrap();
        let store = PatchStore { source: source.as_ref(), output };
        let state = load_state(&store, &applied.state).unwrap();

        assert_eq!(state.nonce, 9);
        assert_eq!(state.bytecode_hash.0, keccak256(replacement_code).0);
        assert_eq!(store.get(&state.bytecode).unwrap(), Some(replacement_code.to_vec()));
        assert_eq!(
            read_storage(&store, &state.contract_state, replaced_key).unwrap(),
            U256::from(5).to_be_bytes()
        );
        assert_eq!(read_storage(&store, &state.contract_state, deleted_key).unwrap(), [0_u8; 32]);
        assert_eq!(applied.storage.len(), 2);
        assert_eq!(applied.runtime_code.unwrap().source, old_code_cid);
    }

    #[test]
    fn rejects_reserved_bytecode_prefix() {
        let source = Rc::new(MemoryBlockstore::default());
        let bytecode = source.put(Code::Blake2b256, &Block::new(IPLD_RAW, [])).unwrap();
        let slots = EvmStateKamtV18::new_with_config(Rc::clone(&source), EVM_STATE_KAMT_CONFIG_V18)
            .flush()
            .unwrap();
        let state = source
            .put_cbor(
                &EvmStateV18 {
                    bytecode,
                    bytecode_hash: BytecodeHashV18(keccak256([]).0),
                    contract_state: slots,
                    transient_data: None,
                    nonce: 0,
                    tombstone: None,
                },
                Code::Blake2b256,
            )
            .unwrap();
        let error = apply_evm_patch(
            source.as_ref(),
            Rc::new(MemoryBlockstore::default()),
            &EVM_ACTOR_CODE,
            &state,
            Some(&[0xef]),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(error, EvmPatchError::ReservedCodePrefix));
    }
}
