use std::rc::Rc;

use alloy_primitives::{Address as EthAddress, address};
use cid::Cid;
use fvm::state_tree::{ActorState, StateTree};
use fvm_ipld_blockstore::MemoryBlockstore;
use fvm_ipld_encoding::CborStore;
use fvm_ipld_encoding::tuple::*;
use fvm_shared::address::Address;
use fvm_shared::econ::TokenAmount;
use fvm_shared::state::StateTreeVersion;
use multihash_codetable::Code;
use serde::Serialize;

use crate::bundle::BundleArtifacts;
use crate::error::NodeError;

const SYSTEM_ACTOR_ID: u64 = 0;
const INIT_ACTOR_ID: u64 = 1;
const REWARD_ACTOR_ID: u64 = 2;
const EAM_ACTOR_ID: u64 = 10;
const BURNT_FUNDS_ACTOR_ID: u64 = 99;
const FIRST_NON_SINGLETON_ACTOR_ID: u64 = 100;
const PREFUNDED_BALANCE_FIL: u64 = 10_000;

pub(crate) const PREFUNDED_ETH_ADDRESS: EthAddress =
    address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

// These tuple layouts are adapted from builtin-actors v18.0.0 system, init, and account state
// definitions (Copyright Protocol Labs, MIT OR Apache-2.0). They intentionally reproduce only the
// serialized genesis fields needed here, avoiding a production dependency on individual actors.
#[derive(Serialize_tuple)]
struct SystemStateV18 {
    builtin_actors: Cid,
}

#[derive(Serialize_tuple)]
struct InitStateV18 {
    address_map: Cid,
    next_id: u64,
    network_name: String,
}

#[derive(Serialize_tuple)]
struct AccountStateV18 {
    address: Address,
}

// The current HAMT's empty root serializes as a zero bitfield byte string and no pointers. This is
// adapted from ref-FVM's fvm_ipld_hamt v3 Node and Bitfield serialization.
#[derive(Serialize)]
struct EmptyHamtNode(#[serde(with = "fvm_ipld_encoding::strict_bytes")] Vec<u8>, Vec<()>);

pub(crate) struct PrefundedAccount {
    pub(crate) ethereum: EthAddress,
}

pub(crate) struct GenesisOutput {
    pub(crate) root: Cid,
    pub(crate) accounts: Vec<PrefundedAccount>,
}

pub(crate) fn create_genesis(
    store: &Rc<MemoryBlockstore>,
    bundle: &BundleArtifacts,
) -> Result<GenesisOutput, NodeError> {
    let empty_hamt = store
        .put_cbor(&EmptyHamtNode(Vec::new(), Vec::new()), Code::Blake2b256)
        .map_err(genesis_error)?;
    let empty_state = store.put_cbor(&[(); 0], Code::Blake2b256).map_err(genesis_error)?;
    // This zeroed placeholder state follows ref-FVM's integration Tester::create_placeholder
    // representation (Copyright Protocol Labs, MIT OR Apache-2.0).
    let placeholder_state = store.put_cbor(&[0_u8; 32], Code::Blake2b256).map_err(genesis_error)?;

    let system_state = store
        .put_cbor(&SystemStateV18 { builtin_actors: bundle.manifest_data }, Code::Blake2b256)
        .map_err(genesis_error)?;
    let init_state = store
        .put_cbor(
            &InitStateV18 {
                address_map: empty_hamt,
                next_id: FIRST_NON_SINGLETON_ACTOR_ID,
                network_name: "fvm-anvil".to_owned(),
            },
            Code::Blake2b256,
        )
        .map_err(genesis_error)?;
    let burnt_state = store
        .put_cbor(
            &AccountStateV18 { address: Address::new_id(BURNT_FUNDS_ACTOR_ID) },
            Code::Blake2b256,
        )
        .map_err(genesis_error)?;

    let mut tree = StateTree::new(Rc::clone(store), StateTreeVersion::V5)
        .map_err(|error| NodeError::Genesis(error.to_string()))?;
    tree.set_actor(
        SYSTEM_ACTOR_ID,
        actor(bundle.code("system")?, system_state, TokenAmount::default(), None),
    );
    tree.set_actor(
        INIT_ACTOR_ID,
        actor(bundle.code("init")?, init_state, TokenAmount::default(), None),
    );
    // Actor f02 is deliberately a balance-bearing fee sink. DefaultExecutor transfers miner tips
    // to it, but Wave 1 never invokes Reward actor methods, so no reward-state semantics are needed.
    tree.set_actor(
        REWARD_ACTOR_ID,
        actor(bundle.code("reward")?, empty_state, TokenAmount::default(), None),
    );
    tree.set_actor(
        EAM_ACTOR_ID,
        actor(bundle.code("eam")?, empty_state, TokenAmount::default(), None),
    );
    tree.set_actor(
        BURNT_FUNDS_ACTOR_ID,
        actor(bundle.code("account")?, burnt_state, TokenAmount::default(), None),
    );

    let delegated = Address::new_delegated(EAM_ACTOR_ID, PREFUNDED_ETH_ADDRESS.as_slice())
        .map_err(|error| NodeError::Genesis(error.to_string()))?;
    let actor_id = tree
        .register_new_address(&delegated)
        .map_err(|error| NodeError::Genesis(error.to_string()))?;
    if actor_id != FIRST_NON_SINGLETON_ACTOR_ID {
        return Err(NodeError::Genesis(format!(
            "prefunded account received actor ID {actor_id}, expected {FIRST_NON_SINGLETON_ACTOR_ID}"
        )));
    }
    tree.set_actor(
        actor_id,
        actor(
            bundle.code("placeholder")?,
            placeholder_state,
            TokenAmount::from_whole(PREFUNDED_BALANCE_FIL),
            Some(delegated),
        ),
    );

    let root = tree.flush().map_err(|error| NodeError::Genesis(error.to_string()))?;
    Ok(GenesisOutput { root, accounts: vec![PrefundedAccount { ethereum: PREFUNDED_ETH_ADDRESS }] })
}

fn actor(
    code: Cid,
    state: Cid,
    balance: TokenAmount,
    delegated_address: Option<Address>,
) -> ActorState {
    ActorState { code, state, sequence: 0, balance, delegated_address }
}

fn genesis_error(error: impl std::fmt::Display) -> NodeError {
    NodeError::Genesis(error.to_string())
}
