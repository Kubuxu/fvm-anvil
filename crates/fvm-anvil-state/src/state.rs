// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Construction of standalone V5 state trees from selected source actors.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::str::FromStr;

use cid::Cid;
use fvm::state_tree::StateTree;
use fvm_ipld_blockstore::{Blockstore, MemoryBlockstore};
use fvm_ipld_encoding::{CborStore, tuple::*};
use fvm_ipld_hamt::{BytesKey, Config, Hamt};
use fvm_shared::address::{Address, Payload};
use fvm_shared::state::{ActorState, StateTreeVersion};
use multihash_codetable::Code;
use thiserror::Error;

use crate::prepared::PreparedActor;

const SYSTEM_ACTOR_ID: u64 = 0;
const INIT_ACTOR_ID: u64 = 1;
const REWARD_ACTOR_ID: u64 = 2;
const EAM_ACTOR_ID: u64 = 10;
const BURNT_FUNDS_ACTOR_ID: u64 = 99;
const TEST_ACTOR_ID: u64 = 98;
const EAM_NAMESPACE: u64 = 10;
const MANDATORY_ACTOR_IDS: [u64; 5] =
    [SYSTEM_ACTOR_ID, INIT_ACTOR_ID, REWARD_ACTOR_ID, EAM_ACTOR_ID, BURNT_FUNDS_ACTOR_ID];

// Wire layouts adapted from builtin-actors v18 System and Init state definitions
// (Protocol Labs, MIT OR Apache-2.0).
#[derive(Deserialize_tuple)]
struct SystemStateV18 {
    builtin_actors: Cid,
}

#[derive(Deserialize_tuple, Serialize_tuple)]
struct InitStateV18 {
    address_map: Cid,
    next_id: u64,
    network_name: String,
}

/// One user selector resolved against the source Init actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorSelector {
    pub input: String,
    pub address: Address,
}

/// Fresh state-tree blocks plus provenance needed to export them.
pub struct ConstructedState {
    pub state_root: Cid,
    pub store: Rc<MemoryBlockstore>,
    pub actors: Vec<PreparedActor>,
    pub actor_manifest_data: Cid,
    /// Actor code blocks absent from the snapshot and supplied by the external bundle.
    pub external_actor_codes: BTreeSet<Cid>,
    /// All external links at which state traversal must stop.
    pub external_links: BTreeSet<Cid>,
}

#[derive(Debug, Error)]
pub enum StateConstructionError {
    #[error("invalid actor address {input:?}: {message}")]
    InvalidAddress { input: String, message: String },
    #[error("actor address {0:?} does not exist in the source state")]
    MissingAddress(String),
    #[error("source state is missing actor f0{0}")]
    MissingActor(u64),
    #[error("actor f098 is test-only and cannot be included in reachable state")]
    TestActor,
    #[error("failed to load or construct the state tree: {0}")]
    StateTree(String),
    #[error("failed to read actor f0{id} state {state}: {message}")]
    ActorState { id: u64, state: Box<Cid>, message: String },
    #[error("failed to inspect actor code block {code}: {message}")]
    ActorCode { code: Box<Cid>, message: String },
    #[error("failed to rebuild the Init actor address map: {0}")]
    InitMap(String),
}

/// Parses either a Filecoin address or a 20-byte Ethereum hex address.
pub fn parse_actor_selector(
    input: impl Into<String>,
) -> Result<ActorSelector, StateConstructionError> {
    let input = input.into();
    let address = if let Some(hex_address) = input.strip_prefix("0x") {
        let bytes =
            hex::decode(hex_address).map_err(|error| StateConstructionError::InvalidAddress {
                input: input.clone(),
                message: error.to_string(),
            })?;
        if bytes.len() != 20 {
            return Err(StateConstructionError::InvalidAddress {
                input,
                message: "Ethereum addresses must contain exactly 20 bytes".into(),
            });
        }
        Address::new_delegated(EAM_NAMESPACE, &bytes).map_err(|error| {
            StateConstructionError::InvalidAddress {
                input: input.clone(),
                message: error.to_string(),
            }
        })?
    } else {
        Address::from_str(&input).map_err(|error| StateConstructionError::InvalidAddress {
            input: input.clone(),
            message: error.to_string(),
        })?
    };
    Ok(ActorSelector { input, address })
}

/// Builds a fresh V5 tree containing the mandatory runtime actors and selected actors.
pub fn construct_standalone_state<BS: Blockstore>(
    source: &BS,
    source_root: &Cid,
    selectors: &[ActorSelector],
) -> Result<ConstructedState, StateConstructionError> {
    let source_tree = StateTree::new_from_root(source, source_root)
        .map_err(|error| StateConstructionError::StateTree(error.to_string()))?;
    let mut requested = BTreeMap::<u64, Vec<String>>::new();
    for selector in selectors {
        let id = source_tree
            .lookup_id(&selector.address)
            .map_err(|error| StateConstructionError::StateTree(error.to_string()))?
            .ok_or_else(|| StateConstructionError::MissingAddress(selector.input.clone()))?;
        if id == TEST_ACTOR_ID {
            return Err(StateConstructionError::TestActor);
        }
        requested.entry(id).or_default().push(selector.input.clone());
    }

    let mut included_ids = BTreeSet::from(MANDATORY_ACTOR_IDS);
    included_ids.extend(requested.keys().copied());

    let mut actors = BTreeMap::<u64, ActorState>::new();
    for id in included_ids.iter().copied() {
        let actor = source_tree
            .get_actor(id)
            .map_err(|error| StateConstructionError::StateTree(error.to_string()))?
            .ok_or(StateConstructionError::MissingActor(id))?;
        actors.insert(id, actor);
    }
    for (&id, actor) in &actors {
        let Some(address) = actor.delegated_address else {
            continue;
        };
        let resolved = source_tree
            .lookup_id(&address)
            .map_err(|error| StateConstructionError::StateTree(error.to_string()))?;
        if resolved != Some(id) {
            return Err(StateConstructionError::InitMap(format!(
                "delegated address {address} resolves to {resolved:?}, expected f0{id}"
            )));
        }
    }
    let source_states =
        actors.iter().map(|(&id, actor)| (id, actor.state)).collect::<BTreeMap<_, _>>();

    let system = actors.get(&SYSTEM_ACTOR_ID).expect("mandatory System actor was inserted");
    let system_state = source
        .get_cbor::<SystemStateV18>(&system.state)
        .map_err(|error| StateConstructionError::ActorState {
            id: SYSTEM_ACTOR_ID,
            state: Box::new(system.state),
            message: error.to_string(),
        })?
        .ok_or(StateConstructionError::ActorState {
            id: SYSTEM_ACTOR_ID,
            state: Box::new(system.state),
            message: "block is missing".into(),
        })?;

    let source_init_actor = actors.get(&INIT_ACTOR_ID).expect("mandatory Init actor was inserted");
    let source_init = source
        .get_cbor::<InitStateV18>(&source_init_actor.state)
        .map_err(|error| StateConstructionError::ActorState {
            id: INIT_ACTOR_ID,
            state: Box::new(source_init_actor.state),
            message: error.to_string(),
        })?
        .ok_or(StateConstructionError::ActorState {
            id: INIT_ACTOR_ID,
            state: Box::new(source_init_actor.state),
            message: "block is missing".into(),
        })?;

    let output = Rc::new(MemoryBlockstore::default());
    let config = Config { bit_width: 5, min_data_depth: 0, max_array_width: 3 };
    let source_map =
        Hamt::<_, u64>::load_with_config(&source_init.address_map, source, config.clone())
            .map_err(|error| StateConstructionError::InitMap(error.to_string()))?;
    let mut output_map = Hamt::<_, u64>::new_with_config(Rc::clone(&output), config);
    source_map
        .for_each_cacheless(|key: &BytesKey, actor_id| {
            if included_ids.contains(actor_id) {
                let address = Address::from_bytes(&key.0)?;
                if matches!(address.payload(), Payload::ID(_)) {
                    anyhow::bail!("Init map contains an ID-address key");
                }
                output_map.set(key.clone(), *actor_id)?;
            }
            Ok(())
        })
        .map_err(|error| StateConstructionError::InitMap(error.to_string()))?;
    let address_map =
        output_map.flush().map_err(|error| StateConstructionError::InitMap(error.to_string()))?;
    let new_init_state = InitStateV18 {
        address_map,
        next_id: source_init.next_id,
        network_name: source_init.network_name,
    };
    let new_init_state_cid = output
        .put_cbor(&new_init_state, Code::Blake2b256)
        .map_err(|error| StateConstructionError::InitMap(error.to_string()))?;
    actors.get_mut(&INIT_ACTOR_ID).expect("mandatory Init actor was inserted").state =
        new_init_state_cid;

    let mut output_tree = StateTree::new(Rc::clone(&output), StateTreeVersion::V5)
        .map_err(|error| StateConstructionError::StateTree(error.to_string()))?;
    let mut prepared_actors = Vec::with_capacity(actors.len());
    let mut external_actor_codes = BTreeSet::new();
    for actor in actors.values() {
        let present = source
            .get(&actor.code)
            .map_err(|error| StateConstructionError::ActorCode {
                code: Box::new(actor.code),
                message: error.to_string(),
            })?
            .is_some();
        if !present {
            external_actor_codes.insert(actor.code);
        }
    }
    let mut external_links = external_actor_codes.clone();
    external_links.insert(system_state.builtin_actors);
    for (id, actor) in actors {
        prepared_actors.push(PreparedActor {
            id,
            code: actor.code,
            source_state: source_states[&id],
            state: actor.state,
            requested_addresses: requested.remove(&id).unwrap_or_default(),
        });
        output_tree.set_actor(id, actor);
    }
    let state_root = output_tree
        .flush()
        .map_err(|error| StateConstructionError::StateTree(error.to_string()))?;

    Ok(ConstructedState {
        state_root,
        store: output,
        actors: prepared_actors,
        actor_manifest_data: system_state.builtin_actors,
        external_actor_codes,
        external_links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ethereum_address_as_eam_delegated_address() {
        let selector = parse_actor_selector("0x0000000000000000000000000000000000000001").unwrap();
        let Payload::Delegated(delegated) = selector.address.payload() else {
            panic!("expected a delegated address");
        };
        assert_eq!(delegated.namespace(), EAM_NAMESPACE);
        assert_eq!(
            delegated.subaddress(),
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn rejects_short_ethereum_addresses() {
        assert!(parse_actor_selector("0x01").is_err());
    }
}
