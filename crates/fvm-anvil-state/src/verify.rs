// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structural verification for standalone Press state archives.

use std::collections::BTreeSet;

use alloy_primitives::keccak256;
use cid::Cid;
use fvm::state_tree::StateTree;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::IPLD_RAW;
use fvm_ipld_encoding::{CborStore, tuple::*};
use fvm_shared::address::Payload;
use thiserror::Error;

use crate::evm::{inspect_evm_state, is_evm_actor_code, read_storage, validate_runtime_code};
use crate::prepared::{PreparedActorPatch, PreparedStateMetadata, load_prepared_metadata};
use crate::state::parse_actor_selector;
use crate::traversal::traverse_blockstore;

const TEST_ACTOR_ID: u64 = 98;

#[derive(Deserialize_tuple)]
struct SystemState {
    builtin_actors: Cid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationSummary {
    pub actors: usize,
    pub reachable_blocks: u64,
}

#[derive(Debug, Error)]
pub enum VerificationError {
    #[error("archive root is not supported Press state metadata")]
    NotPreparedState,
    #[error("failed to read prepared metadata: {0}")]
    Metadata(String),
    #[error("failed to load prepared state tree: {0}")]
    StateTree(String),
    #[error("metadata contains duplicate actor f0{0}")]
    DuplicateActor(u64),
    #[error("prepared state tree contains actor f098")]
    TestActor,
    #[error("metadata actor set does not match the prepared state tree")]
    ActorSetMismatch,
    #[error("prepared state is missing metadata actor f0{0}")]
    MissingActor(u64),
    #[error("prepared actor f0{id} has {field} {actual}, expected {expected}")]
    ActorMismatch { id: u64, field: &'static str, expected: Box<Cid>, actual: Box<Cid> },
    #[error("prepared actor f0{id} has mismatched {field}")]
    ActorRecordMismatch { id: u64, field: &'static str },
    #[error("prepared actor f0{id} has invalid patch metadata: {message}")]
    InvalidPatch { id: u64, message: String },
    #[error("requested address {address:?} resolves to {actual:?}, expected f0{expected}")]
    AddressMismatch { address: String, expected: u64, actual: Option<u64> },
    #[error("prepared System actor manifest {actual} does not match metadata {expected}")]
    ManifestMismatch { expected: Box<Cid>, actual: Box<Cid> },
    #[error("metadata external actor code list is invalid at {0}")]
    InvalidExternalCode(Box<Cid>),
    #[error("prepared actor DAG is incomplete: {0}")]
    IncompleteDag(String),
}

pub fn verify_prepared_archive(
    store: &impl Blockstore,
    roots: &[Cid],
) -> Result<(PreparedStateMetadata, VerificationSummary), VerificationError> {
    verify_prepared_archive_with_progress(store, roots, |_| {})
}

pub fn verify_prepared_archive_with_progress(
    store: &impl Blockstore,
    roots: &[Cid],
    progress: impl FnMut(u64),
) -> Result<(PreparedStateMetadata, VerificationSummary), VerificationError> {
    verify_prepared_archive_inner(store, roots, true, progress)
}

/// Validates prepared metadata and its state-tree records without traversing every actor-state DAG.
///
/// Archive builders and consumers can use this bounded check before publication or startup. Missing
/// unpatched actor-state blocks then fail on first access unless full reachability is checked too.
pub fn validate_prepared_archive(
    store: &impl Blockstore,
    roots: &[Cid],
) -> Result<PreparedStateMetadata, VerificationError> {
    verify_prepared_archive_inner(store, roots, false, |_| {}).map(|(metadata, _)| metadata)
}

fn verify_prepared_archive_inner(
    store: &impl Blockstore,
    roots: &[Cid],
    verify_reachability: bool,
    mut progress: impl FnMut(u64),
) -> Result<(PreparedStateMetadata, VerificationSummary), VerificationError> {
    let (_, metadata) = load_prepared_metadata(store, roots)
        .map_err(|error| VerificationError::Metadata(error.to_string()))?
        .ok_or(VerificationError::NotPreparedState)?;
    let tree = StateTree::new_from_root(store, &metadata.state_root)
        .map_err(|error| VerificationError::StateTree(error.to_string()))?;

    let mut expected = BTreeSet::new();
    for actor in &metadata.actors {
        if !expected.insert(actor.id) {
            return Err(VerificationError::DuplicateActor(actor.id));
        }
        if actor.id == TEST_ACTOR_ID {
            return Err(VerificationError::TestActor);
        }
        let actual = tree
            .get_actor(actor.id)
            .map_err(|error| VerificationError::StateTree(error.to_string()))?
            .ok_or(VerificationError::MissingActor(actor.id))?;
        if actual.code != actor.code {
            return Err(VerificationError::ActorMismatch {
                id: actor.id,
                field: "code CID",
                expected: Box::new(actor.code),
                actual: Box::new(actual.code),
            });
        }
        if actual.state != actor.state {
            return Err(VerificationError::ActorMismatch {
                id: actor.id,
                field: "state CID",
                expected: Box::new(actor.state),
                actual: Box::new(actual.state),
            });
        }
        if actual.sequence != actor.sequence {
            return Err(VerificationError::ActorRecordMismatch { id: actor.id, field: "sequence" });
        }
        if actual.balance != actor.balance {
            return Err(VerificationError::ActorRecordMismatch { id: actor.id, field: "balance" });
        }
        if actual.delegated_address != actor.delegated_address {
            return Err(VerificationError::ActorRecordMismatch {
                id: actor.id,
                field: "delegated address",
            });
        }
        if let Some(patch) = &actor.patch {
            verify_actor_patch(store, actor.id, &actual, patch)?;
        }
        for address in &actor.requested_addresses {
            let selector = parse_actor_selector(address.clone())
                .map_err(|error| VerificationError::StateTree(error.to_string()))?;
            let actual_id = tree
                .lookup_id(&selector.address)
                .map_err(|error| VerificationError::StateTree(error.to_string()))?;
            if actual_id != Some(actor.id) {
                return Err(VerificationError::AddressMismatch {
                    address: address.clone(),
                    expected: actor.id,
                    actual: actual_id,
                });
            }
        }
    }

    let mut actual = BTreeSet::new();
    tree.for_each_cacheless(|address, _| {
        let Payload::ID(id) = address.payload() else {
            anyhow::bail!("state tree contains a non-ID actor key");
        };
        actual.insert(*id);
        Ok(())
    })
    .map_err(|error| VerificationError::StateTree(error.to_string()))?;
    if actual.contains(&TEST_ACTOR_ID) {
        return Err(VerificationError::TestActor);
    }
    if actual != expected {
        return Err(VerificationError::ActorSetMismatch);
    }

    let system_actor = tree
        .get_actor(0)
        .map_err(|error| VerificationError::StateTree(error.to_string()))?
        .ok_or(VerificationError::MissingActor(0))?;
    let system_state = store
        .get_cbor::<SystemState>(&system_actor.state)
        .map_err(|error| VerificationError::StateTree(error.to_string()))?
        .ok_or_else(|| VerificationError::IncompleteDag(system_actor.state.to_string()))?;
    if system_state.builtin_actors != metadata.actor_manifest_data {
        return Err(VerificationError::ManifestMismatch {
            expected: Box::new(metadata.actor_manifest_data),
            actual: Box::new(system_state.builtin_actors),
        });
    }

    let actor_codes = metadata.actors.iter().map(|actor| actor.code).collect::<BTreeSet<_>>();
    let actor_states = metadata.actors.iter().map(|actor| actor.state).collect::<BTreeSet<_>>();
    let external_codes = metadata.external_actor_codes.iter().copied().collect::<BTreeSet<_>>();
    if external_codes.len() != metadata.external_actor_codes.len() {
        return Err(VerificationError::InvalidExternalCode(Box::new(
            metadata.external_actor_codes[0],
        )));
    }
    if let Some(cid) =
        external_codes.iter().find(|cid| !actor_codes.contains(cid) || actor_states.contains(cid))
    {
        return Err(VerificationError::InvalidExternalCode(Box::new(*cid)));
    }
    if actor_states.contains(&metadata.actor_manifest_data) {
        return Err(VerificationError::InvalidExternalCode(Box::new(metadata.actor_manifest_data)));
    }
    let actors = actual.len();
    if !verify_reachability {
        return Ok((metadata, VerificationSummary { actors, reachable_blocks: 0 }));
    }
    let mut external = external_codes;
    external.insert(metadata.actor_manifest_data);
    let mut verified_blocks = 0_u64;
    let reachable_blocks = traverse_blockstore(
        store,
        &[metadata.state_root],
        |cid| external.contains(cid),
        |_, _| {
            verified_blocks = verified_blocks
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("verified block count overflow"))?;
            if verified_blocks.is_multiple_of(100_000) {
                progress(verified_blocks);
            }
            Ok(())
        },
    )
    .map_err(|error| VerificationError::IncompleteDag(error.to_string()))?;
    if !verified_blocks.is_multiple_of(100_000) {
        progress(verified_blocks);
    }

    let summary = VerificationSummary { actors, reachable_blocks };
    Ok((metadata, summary))
}

fn verify_actor_patch(
    store: &impl Blockstore,
    actor_id: u64,
    actor: &fvm_shared::state::ActorState,
    patch: &PreparedActorPatch,
) -> Result<(), VerificationError> {
    if patch.runtime_code.is_none()
        && patch.storage.is_empty()
        && patch.balance.is_none()
        && patch.sequence.is_none()
    {
        return Err(VerificationError::InvalidPatch {
            id: actor_id,
            message: "patch contains no transformations".into(),
        });
    }
    if patch.balance.as_ref().is_some_and(|value| actor.balance != value.replacement) {
        return Err(VerificationError::InvalidPatch {
            id: actor_id,
            message: "balance replacement does not match the actor record".into(),
        });
    }
    if patch.sequence.is_some_and(|value| actor.sequence != value.replacement) {
        return Err(VerificationError::InvalidPatch {
            id: actor_id,
            message: "sequence replacement does not match the actor record".into(),
        });
    }
    if patch.runtime_code.is_none() && patch.storage.is_empty() {
        return Ok(());
    }
    if !is_evm_actor_code(&actor.code) {
        return Err(VerificationError::InvalidPatch {
            id: actor_id,
            message: format!("actor code {} is not builtin-actors v18/v19 EVM code", actor.code),
        });
    }
    let state = inspect_evm_state(store, &actor.state).map_err(|error| {
        VerificationError::InvalidPatch { id: actor_id, message: error.to_string() }
    })?;
    if let Some(code) = &patch.runtime_code {
        to_word(&code.source_hash, actor_id, "source bytecode hash")?;
        let replacement_hash =
            to_word(&code.replacement_hash, actor_id, "replacement bytecode hash")?;
        if state.bytecode != code.replacement || state.bytecode_hash != replacement_hash {
            return Err(VerificationError::InvalidPatch {
                id: actor_id,
                message: "replacement bytecode CID or hash does not match EVM state".into(),
            });
        }
        if code.replacement.codec() != IPLD_RAW {
            return Err(VerificationError::InvalidPatch {
                id: actor_id,
                message: "replacement bytecode CID is not raw IPLD".into(),
            });
        }
        let bytes = store
            .get(&code.replacement)
            .map_err(|error| VerificationError::InvalidPatch {
                id: actor_id,
                message: error.to_string(),
            })?
            .ok_or_else(|| VerificationError::InvalidPatch {
                id: actor_id,
                message: format!("replacement bytecode {} is missing", code.replacement),
            })?;
        validate_runtime_code(&bytes).map_err(|error| VerificationError::InvalidPatch {
            id: actor_id,
            message: error.to_string(),
        })?;
        if keccak256(&bytes).0 != replacement_hash {
            return Err(VerificationError::InvalidPatch {
                id: actor_id,
                message: "replacement bytecode bytes do not match their recorded hash".into(),
            });
        }
    }
    let mut keys = BTreeSet::new();
    for storage in &patch.storage {
        let key = to_word(&storage.key, actor_id, "storage key")?;
        let _source = to_word(&storage.source, actor_id, "source storage value")?;
        let replacement = to_word(&storage.replacement, actor_id, "replacement storage value")?;
        if !keys.insert(key) {
            return Err(VerificationError::InvalidPatch {
                id: actor_id,
                message: format!("storage key 0x{} appears more than once", hex::encode(key)),
            });
        }
        let actual = read_storage(store, &state.contract_state, key).map_err(|error| {
            VerificationError::InvalidPatch { id: actor_id, message: error.to_string() }
        })?;
        if actual != replacement {
            return Err(VerificationError::InvalidPatch {
                id: actor_id,
                message: format!(
                    "storage key 0x{} does not contain its replacement",
                    hex::encode(key)
                ),
            });
        }
    }
    Ok(())
}

fn to_word(
    value: &[u8],
    actor_id: u64,
    field: &'static str,
) -> Result<[u8; 32], VerificationError> {
    value.try_into().map_err(|_| VerificationError::InvalidPatch {
        id: actor_id,
        message: format!("{field} must contain exactly 32 bytes"),
    })
}
