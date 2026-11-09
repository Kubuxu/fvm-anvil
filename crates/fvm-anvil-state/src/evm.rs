// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared builtin-actors v18/v19 EVM state mutation and inspection.

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
const EVM_ACTOR_CODE_V19: &str = "bafk2bzaceaufbdpwqvu6r7bbfpx4w4fw3cub27lm45wapyjfaiwesgvkrw5lm";
const MAX_CODE_SIZE: usize = 24 << 10;
// Adapted from builtin-actors v18.0.0 and v19.0.1 (Protocol Labs, MIT OR Apache-2.0):
// actors/evm/src/{state.rs,interpreter/system.rs} and actors/evm/shared/src/uints.rs.
// Both releases use the same tuple, zero-trimmed big-endian words, and unhashed 32-byte KAMT keys.
const EVM_STATE_KAMT_CONFIG: KamtConfig =
    KamtConfig { min_data_depth: 0, bit_width: 5, max_array_width: 1 };
static EVM_ACTOR_CODES: LazyLock<[Cid; 2]> = LazyLock::new(|| {
    [EVM_ACTOR_CODE_V18, EVM_ACTOR_CODE_V19]
        .map(|code| Cid::from_str(code).expect("builtin-actors v18/v19 EVM code CID is valid"))
});

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct EvmWord(U256);

impl Serialize for EvmWord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = self.0.to_be_bytes::<32>();
        let first_nonzero = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len());
        serializer.serialize_bytes(&bytes[first_nonzero..])
    }
}

impl<'de> Deserialize<'de> for EvmWord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct WordVisitor;

        impl serde::de::Visitor<'_> for WordVisitor {
            type Value = EvmWord;

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
                Ok(EvmWord(U256::from_be_slice(bytes)))
            }
        }

        deserializer.deserialize_bytes(WordVisitor)
    }
}

struct StateHashAlgorithm;

impl AsHashedKey<EvmWord, 32> for StateHashAlgorithm {
    fn as_hashed_key(key: &EvmWord) -> Cow<'_, HashedKey<32>> {
        Cow::Owned(key.0.to_be_bytes())
    }
}

type EvmStateKamt<BS> = Kamt<BS, EvmWord, EvmWord, StateHashAlgorithm>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
struct BytecodeHash(#[serde(with = "fvm_ipld_encoding::strict_bytes")] [u8; 32]);

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct TransientData {
    transient_data_state: Cid,
    transient_data_lifespan: TransientDataLifespan,
}

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct TransientDataLifespan {
    origin: u64,
    nonce: u64,
}

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
struct Tombstone {
    origin: u64,
    nonce: u64,
}

#[derive(Clone, Debug, Deserialize_tuple, Serialize_tuple)]
pub struct EvmState {
    bytecode: Cid,
    bytecode_hash: BytecodeHash,
    contract_state: Cid,
    transient_data: Option<TransientData>,
    nonce: u64,
    tombstone: Option<Tombstone>,
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

/// One bytecode replacement applied to builtin-actors v18/v19 EVM state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvmBytecodeUpdate {
    pub previous: Cid,
    pub previous_hash: [u8; 32],
    pub replacement: Cid,
    pub replacement_hash: [u8; 32],
}

/// One storage replacement applied to builtin-actors v18/v19 EVM state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvmStorageUpdate {
    pub key: [u8; 32],
    pub previous: [u8; 32],
    pub replacement: [u8; 32],
}

/// Errors while reading or mutating builtin-actors v18/v19 EVM state.
#[derive(Debug, Error)]
pub enum EvmPatchError {
    #[error("actor code {0} is not builtin-actors v18/v19 EVM code")]
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

impl EvmState {
    /// Loads builtin-actors v18/v19 EVM state from its shared state tuple.
    pub fn load<BS: Blockstore>(store: &BS, state_root: &Cid) -> Result<Self, EvmPatchError> {
        store
            .get_cbor::<Self>(state_root)
            .map_err(|error| EvmPatchError::State {
                state: Box::new(*state_root),
                message: error.to_string(),
            })?
            .ok_or_else(|| EvmPatchError::MissingState(Box::new(*state_root)))
    }

    /// Creates new builtin-actors v18/v19 EVM state with empty storage.
    pub fn create<BS: Blockstore>(
        store: &BS,
        bytecode: &[u8],
        nonce: u64,
    ) -> Result<Self, EvmPatchError> {
        let (bytecode, bytecode_hash) = write_bytecode(store, bytecode)?;
        let mut slots = EvmStateKamt::new_with_config(store, EVM_STATE_KAMT_CONFIG.clone());
        let contract_state =
            slots.flush().map_err(|error| EvmPatchError::Storage(error.to_string()))?;
        Ok(Self {
            bytecode,
            bytecode_hash: BytecodeHash(bytecode_hash),
            contract_state,
            transient_data: None,
            nonce,
            tombstone: None,
        })
    }

    pub const fn bytecode(&self) -> Cid {
        self.bytecode
    }

    pub const fn bytecode_hash(&self) -> [u8; 32] {
        self.bytecode_hash.0
    }

    pub const fn contract_state(&self) -> Cid {
        self.contract_state
    }

    pub const fn nonce(&self) -> u64 {
        self.nonce
    }

    pub const fn set_nonce(&mut self, nonce: u64) {
        self.nonce = nonce;
    }

    /// Replaces bytecode without deployment-time validation.
    pub fn replace_bytecode<BS: Blockstore>(
        &mut self,
        store: &BS,
        replacement: &[u8],
    ) -> Result<EvmBytecodeUpdate, EvmPatchError> {
        let previous = self.bytecode;
        let previous_hash = self.bytecode_hash.0;
        let (replacement, replacement_hash) = write_bytecode(store, replacement)?;
        self.bytecode = replacement;
        self.bytecode_hash = BytecodeHash(replacement_hash);
        Ok(EvmBytecodeUpdate { previous, previous_hash, replacement, replacement_hash })
    }

    /// Clears a persisted self-destruct marker when direct code mutation resurrects an account.
    pub fn clear_tombstone(&mut self) {
        self.tombstone = None;
    }

    /// Replaces storage slots and returns their previous values.
    pub fn replace_storage<BS: Blockstore>(
        &mut self,
        store: &BS,
        replacements: &BTreeMap<[u8; 32], [u8; 32]>,
    ) -> Result<Vec<EvmStorageUpdate>, EvmPatchError> {
        if replacements.is_empty() {
            return Ok(Vec::new());
        }
        let mut slots = EvmStateKamt::load_with_config(
            &self.contract_state,
            store,
            EVM_STATE_KAMT_CONFIG.clone(),
        )
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
        let mut updates = Vec::with_capacity(replacements.len());
        for (key, replacement) in replacements {
            updates.push(replace_storage_value(&mut slots, *key, *replacement)?);
        }
        self.contract_state =
            slots.flush().map_err(|error| EvmPatchError::Storage(error.to_string()))?;
        Ok(updates)
    }

    /// Replaces one storage slot without allocating a temporary collection.
    pub fn replace_storage_slot<BS: Blockstore>(
        &mut self,
        store: &BS,
        key: [u8; 32],
        replacement: [u8; 32],
    ) -> Result<EvmStorageUpdate, EvmPatchError> {
        let mut slots = EvmStateKamt::load_with_config(
            &self.contract_state,
            store,
            EVM_STATE_KAMT_CONFIG.clone(),
        )
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
        let update = replace_storage_value(&mut slots, key, replacement)?;
        self.contract_state =
            slots.flush().map_err(|error| EvmPatchError::Storage(error.to_string()))?;
        Ok(update)
    }

    /// Reads one storage slot from this state's current storage root.
    pub fn storage<BS: Blockstore>(
        &self,
        store: &BS,
        key: [u8; 32],
    ) -> Result<[u8; 32], EvmPatchError> {
        read_storage(store, &self.contract_state, key)
    }

    /// Persists the state tuple and returns its new root.
    pub fn flush<BS: Blockstore>(
        &self,
        store: &BS,
        previous_root: &Cid,
    ) -> Result<Cid, EvmPatchError> {
        store.put_cbor(self, Code::Blake2b256).map_err(|error| EvmPatchError::State {
            state: Box::new(*previous_root),
            message: error.to_string(),
        })
    }
}

fn replace_storage_value<BS: Blockstore>(
    slots: &mut EvmStateKamt<BS>,
    key: [u8; 32],
    replacement: [u8; 32],
) -> Result<EvmStorageUpdate, EvmPatchError> {
    let key_word = EvmWord(U256::from_be_bytes(key));
    let previous = slots
        .get(&key_word)
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?
        .copied()
        .unwrap_or_default()
        .0
        .to_be_bytes();
    let replacement_word = EvmWord(U256::from_be_bytes(replacement));
    if replacement_word.0.is_zero() {
        slots.delete(&key_word).map_err(|error| EvmPatchError::Storage(error.to_string()))?;
    } else {
        slots
            .set(key_word, replacement_word)
            .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
    }
    Ok(EvmStorageUpdate { key, previous, replacement })
}

fn write_bytecode<BS: Blockstore>(
    store: &BS,
    bytecode: &[u8],
) -> Result<(Cid, [u8; 32]), EvmPatchError> {
    let hash = keccak256(bytecode).0;
    let cid = store
        .put(Code::Blake2b256, &Block::new(IPLD_RAW, bytecode))
        .map_err(|error| EvmPatchError::Bytecode(error.to_string()))?;
    Ok((cid, hash))
}

pub(crate) fn is_evm_actor_code(code: &Cid) -> bool {
    EVM_ACTOR_CODES.contains(code)
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
    let mut state = EvmState::load(&store, state_root)?;

    let runtime_code = runtime_code
        .map(|replacement| {
            validate_runtime_code(replacement)?;
            let update = state.replace_bytecode(&store, replacement)?;
            Ok(PreparedBytecodePatch {
                source: update.previous,
                source_hash: update.previous_hash.to_vec(),
                replacement: update.replacement,
                replacement_hash: update.replacement_hash.to_vec(),
            })
        })
        .transpose()?;
    let storage = state
        .replace_storage(&store, storage)?
        .into_iter()
        .map(|update| PreparedStoragePatch {
            key: update.key.to_vec(),
            source: update.previous.to_vec(),
            replacement: update.replacement.to_vec(),
        })
        .collect();
    let state = state.flush(&store, state_root)?;
    Ok(AppliedEvmPatch { state, runtime_code, storage })
}

pub(crate) fn inspect_evm_state(
    store: &impl Blockstore,
    state_root: &Cid,
) -> Result<EvmStateView, EvmPatchError> {
    let state = EvmState::load(store, state_root)?;
    Ok(EvmStateView {
        bytecode: state.bytecode(),
        bytecode_hash: state.bytecode_hash(),
        contract_state: state.contract_state(),
    })
}

pub(crate) fn read_storage(
    store: &impl Blockstore,
    state_root: &Cid,
    key: [u8; 32],
) -> Result<[u8; 32], EvmPatchError> {
    let slots = EvmStateKamt::load_with_config(state_root, store, EVM_STATE_KAMT_CONFIG)
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?;
    let value = slots
        .get(&EvmWord(U256::from_be_bytes(key)))
        .map_err(|error| EvmPatchError::Storage(error.to_string()))?
        .copied()
        .unwrap_or_default();
    Ok(value.0.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fvm_ipld_encoding::{BytesSer, from_slice, to_vec};

    #[test]
    fn word_encoding_and_kamt_keys_match_shared_actor_schema() {
        for (value, encoded) in
            [(U256::ZERO, vec![0x40]), (U256::from(0x0102), vec![0x42, 0x01, 0x02])]
        {
            assert_eq!(to_vec(&EvmWord(value)).unwrap(), encoded);
            assert_eq!(from_slice::<EvmWord>(&encoded).unwrap().0, value);
        }
        let key = [0xab; 32];
        let word = EvmWord(U256::from_be_bytes(key));
        assert_eq!(to_vec(&word).unwrap(), to_vec(&BytesSer(&key)).unwrap());
        assert_eq!(StateHashAlgorithm::as_hashed_key(&word).as_ref(), &key);
        let oversized = to_vec(&BytesSer(&[0_u8; 33])).unwrap();
        assert!(from_slice::<EvmWord>(&oversized).is_err());
    }

    #[test]
    fn shared_state_tuple_round_trips_transient_data_and_tombstone() {
        let store = MemoryBlockstore::default();
        let bytecode = store.put(Code::Blake2b256, &Block::new(IPLD_RAW, [])).unwrap();
        let hash = keccak256([]).0;
        let slots = EvmStateKamt::new_with_config(&store, EVM_STATE_KAMT_CONFIG).flush().unwrap();
        let root = store
            .put_cbor(
                &(
                    bytecode,
                    BytesSer(&hash),
                    slots,
                    Some((slots, (100_u64, 7_u64))),
                    9_u64,
                    Some((101_u64, 8_u64)),
                ),
                Code::Blake2b256,
            )
            .unwrap();
        let state = EvmState::load(&store, &root).unwrap();
        assert_eq!(state.bytecode(), bytecode);
        assert_eq!(state.bytecode_hash(), hash);
        assert_eq!(state.contract_state(), slots);
        assert_eq!(state.nonce(), 9);
        assert_eq!(state.flush(&store, &root).unwrap(), root);
    }

    #[test]
    fn rejects_unrelated_actor_code_without_patching_state() {
        let source = MemoryBlockstore::default();
        let state = EvmState::create(&source, &[0x00], 0).unwrap();
        let root = source.put_cbor(&state, Code::Blake2b256).unwrap();
        let unrelated = source.put(Code::Blake2b256, &Block::new(IPLD_RAW, b"not EVM")).unwrap();
        let error = apply_evm_patch(
            &source,
            Rc::new(MemoryBlockstore::default()),
            &unrelated,
            &root,
            Some(&[0x60, 0x00]),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(error, EvmPatchError::NotEvmActor(code) if *code == unrelated));
        assert_eq!(EvmState::load(&source, &root).unwrap().bytecode(), state.bytecode());
    }

    #[test]
    fn replaces_bytecode_and_batches_storage_updates() {
        for actor_code in EVM_ACTOR_CODES.iter() {
            assert_replaces_bytecode_and_storage(actor_code);
        }
    }

    fn assert_replaces_bytecode_and_storage(actor_code: &Cid) {
        let source = Rc::new(MemoryBlockstore::default());
        let old_code = b"old runtime";
        let old_code_cid = source.put(Code::Blake2b256, &Block::new(IPLD_RAW, old_code)).unwrap();
        let mut slots = EvmStateKamt::new_with_config(Rc::clone(&source), EVM_STATE_KAMT_CONFIG);
        let replaced_key = [1_u8; 32];
        let deleted_key = [2_u8; 32];
        slots.set(EvmWord(U256::from_be_bytes(replaced_key)), EvmWord(U256::from(3))).unwrap();
        slots.set(EvmWord(U256::from_be_bytes(deleted_key)), EvmWord(U256::from(4))).unwrap();
        let contract_state = slots.flush().unwrap();
        let source_state = source
            .put_cbor(
                &EvmState {
                    bytecode: old_code_cid,
                    bytecode_hash: BytecodeHash(keccak256(old_code).0),
                    contract_state,
                    transient_data: Some(TransientData {
                        transient_data_state: contract_state,
                        transient_data_lifespan: TransientDataLifespan { origin: 100, nonce: 7 },
                    }),
                    nonce: 9,
                    tombstone: Some(Tombstone { origin: 101, nonce: 8 }),
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
            actor_code,
            &source_state,
            Some(&replacement_code),
            &storage,
        )
        .unwrap();
        let store = PatchStore { source: source.as_ref(), output };
        let state = EvmState::load(&store, &applied.state).unwrap();

        assert_eq!(state.nonce, 9);
        let transient = state.transient_data.as_ref().unwrap();
        assert_eq!(transient.transient_data_state, contract_state);
        assert_eq!(transient.transient_data_lifespan.origin, 100);
        assert_eq!(transient.transient_data_lifespan.nonce, 7);
        assert_eq!(state.tombstone.as_ref().unwrap().origin, 101);
        assert_eq!(state.tombstone.as_ref().unwrap().nonce, 8);
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
        let slots = EvmStateKamt::new_with_config(Rc::clone(&source), EVM_STATE_KAMT_CONFIG)
            .flush()
            .unwrap();
        let state = source
            .put_cbor(
                &EvmState {
                    bytecode,
                    bytecode_hash: BytecodeHash(keccak256([]).0),
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
            &EVM_ACTOR_CODES[0],
            &state,
            Some(&[0xef]),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(error, EvmPatchError::ReservedCodePrefix));
    }
}
