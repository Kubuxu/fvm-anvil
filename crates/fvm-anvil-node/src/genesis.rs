use std::rc::Rc;

use alloy_primitives::Address as EthAddress;
use cid::Cid;
use fvm::state_tree::{ActorState, StateTree};
use fvm_ipld_blockstore::MemoryBlockstore;
use fvm_ipld_encoding::CborStore;
use fvm_ipld_encoding::tuple::*;
use fvm_shared::address::Address;
use fvm_shared::bigint::{BigInt, Sign};
use fvm_shared::econ::TokenAmount;
use fvm_shared::state::StateTreeVersion;
use multihash_codetable::Code;
use serde::Serialize;

use crate::bundle::BundleArtifacts;
use crate::config::GenesisAccount;
use crate::error::NodeError;

const SYSTEM_ACTOR_ID: u64 = 0;
const INIT_ACTOR_ID: u64 = 1;
const REWARD_ACTOR_ID: u64 = 2;
const EAM_ACTOR_ID: u64 = 10;
const BURNT_FUNDS_ACTOR_ID: u64 = 99;
const FIRST_NON_SINGLETON_ACTOR_ID: u64 = 100;

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
    accounts: &[GenesisAccount],
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

    let mut prefunded_accounts = Vec::new();
    prefunded_accounts
        .try_reserve_exact(accounts.len())
        .map_err(|error| genesis_error(error.to_string()))?;
    for (index, account) in accounts.iter().enumerate() {
        let delegated = Address::new_delegated(EAM_ACTOR_ID, account.ethereum().as_slice())
            .map_err(|error| NodeError::Genesis(error.to_string()))?;
        let actor_id = tree
            .register_new_address(&delegated)
            .map_err(|error| NodeError::Genesis(error.to_string()))?;
        let expected_id = FIRST_NON_SINGLETON_ACTOR_ID
            .checked_add(
                u64::try_from(index)
                    .map_err(|_| NodeError::Genesis("too many prefunded accounts".to_owned()))?,
            )
            .ok_or_else(|| NodeError::Genesis("prefunded actor ID overflow".to_owned()))?;
        if actor_id != expected_id {
            return Err(NodeError::Genesis(format!(
                "prefunded account {} received actor ID {actor_id}, expected {expected_id}",
                account.ethereum()
            )));
        }
        tree.set_actor(
            actor_id,
            actor(
                bundle.code("placeholder")?,
                placeholder_state,
                TokenAmount::from_atto(BigInt::from_bytes_be(
                    Sign::Plus,
                    &account.balance().to_be_bytes::<32>(),
                )),
                Some(delegated),
            ),
        );
        prefunded_accounts.push(PrefundedAccount { ethereum: account.ethereum() });
    }

    let root = tree.flush().map_err(|error| NodeError::Genesis(error.to_string()))?;
    Ok(GenesisOutput { root, accounts: prefunded_accounts })
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
