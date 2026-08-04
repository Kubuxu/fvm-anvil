// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structural verification for standalone Press state archives.

use std::collections::BTreeSet;

use cid::Cid;
use fvm::state_tree::StateTree;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{CborStore, tuple::*};
use fvm_shared::address::Payload;
use thiserror::Error;

use crate::prepared::{PreparedStateMetadata, load_prepared_metadata};
use crate::state::parse_actor_selector;
use crate::traversal::traverse_blockstore;

const TEST_ACTOR_ID: u64 = 98;

#[derive(Deserialize_tuple)]
struct SystemStateV18 {
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
        .get_cbor::<SystemStateV18>(&system_actor.state)
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
    let mut external = external_codes;
    external.insert(metadata.actor_manifest_data);
    let reachable_blocks = traverse_blockstore(
        store,
        &[metadata.state_root],
        |cid| external.contains(cid),
        |_, _| Ok(()),
    )
    .map_err(|error| VerificationError::IncompleteDag(error.to_string()))?;

    let summary = VerificationSummary { actors: actual.len(), reachable_blocks };
    Ok((metadata, summary))
}
