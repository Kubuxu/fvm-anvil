use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{
    CborStore,
    tuple::{Serialize_tuple, serde_tuple},
};
use fvm_shared::{address::Address, econ::TokenAmount};
use multihash_codetable::Code;
use serde::{Serialize, de::DeserializeOwned};

use crate::externs::DeterministicExterns;
use crate::{Block, NodeError};

// Lotus TipSetKey hashes a CBOR byte string of concatenated CID bytes, not a CID array.
#[derive(Serialize)]
#[serde(transparent)]
struct KeyBytes(#[serde(with = "fvm_ipld_encoding::strict_bytes")] Vec<u8>);

/// One canonical synthetic Filecoin block; its CID hashes the Lotus 16-field header encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTipset {
    pub cid: Cid,
    pub key_cid: Cid,
    pub parents: Vec<Cid>,
    pub height: i64,
    pub timestamp: u64,
    pub parent_state_root: Cid,
    pub parent_message_receipts: Cid,
    pub messages: Cid,
    pub base_fee: u128,
    pub ticket: [u8; 32],
    pub beacon: [u8; 32],
}

/// Ordered canonical head changes. A subscription starts with exactly one Current change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadChange {
    Current(NativeTipset),
    Apply(NativeTipset),
    Revert(NativeTipset),
}

// Lotus v1.38.0 chain/types/blockheader.go and cbor_gen.go field order.
#[derive(Serialize_tuple)]
struct Ticket<'a> {
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    proof: &'a [u8],
}

#[derive(Serialize_tuple)]
struct Beacon<'a> {
    round: u64,
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    data: &'a [u8],
}

#[derive(Serialize_tuple)]
struct Header<'a> {
    miner: Address,
    ticket: Ticket<'a>,
    election_proof: Option<()>,
    beacon_entries: [Beacon<'a>; 1],
    win_post_proof: Vec<()>,
    parents: &'a [Cid],
    parent_weight: TokenAmount,
    height: i64,
    parent_state_root: Cid,
    parent_message_receipts: Cid,
    messages: Cid,
    bls_aggregate: Option<()>,
    timestamp: u64,
    block_sig: Option<()>,
    fork_signaling: u64,
    parent_base_fee: TokenAmount,
}

pub(crate) fn project_tipset(
    store: &impl Blockstore,
    block: &Block,
    parent: Option<&NativeTipset>,
    parent_root: Cid,
    parent_receipts: Cid,
    messages: Cid,
) -> Result<NativeTipset, NodeError> {
    let height = i64::try_from(block.number)
        .map_err(|_| NodeError::Runtime("native height exceeds i64".into()))?;
    let ticket = block.hash.0;
    let beacon = DeterministicExterns::beacon_digest(height);
    let parents = parent.map(|parent| vec![parent.cid]).unwrap_or_default();
    let header = Header {
        miner: Address::new_id(0),
        ticket: Ticket { proof: &ticket },
        election_proof: None,
        beacon_entries: [Beacon { round: block.number, data: &beacon }],
        win_post_proof: Vec::new(),
        parents: &parents,
        parent_weight: TokenAmount::from_atto(block.number),
        height,
        parent_state_root: parent_root,
        parent_message_receipts: parent_receipts,
        messages,
        bls_aggregate: None,
        timestamp: block.timestamp,
        block_sig: None,
        fork_signaling: 0,
        parent_base_fee: TokenAmount::from_atto(block.base_fee_per_gas),
    };
    let cid = store
        .put_cbor(&header, Code::Blake2b256)
        .map_err(|error| NodeError::Runtime(format!("encoding native header: {error}")))?;
    let key_cid = store
        .put_cbor(&KeyBytes(cid.to_bytes()), Code::Blake2b256)
        .map_err(|error| NodeError::Runtime(format!("encoding native key: {error}")))?;
    Ok(NativeTipset {
        cid,
        key_cid,
        parents,
        height,
        timestamp: block.timestamp,
        parent_state_root: parent_root,
        parent_message_receipts: parent_receipts,
        messages,
        base_fee: block.base_fee_per_gas,
        ticket,
        beacon,
    })
}

pub(crate) fn array_root<T: Serialize + DeserializeOwned>(
    store: &impl Blockstore,
    values: impl IntoIterator<Item = T>,
    bit_width: u32,
) -> Result<Cid, NodeError> {
    let mut array = fvm_ipld_amt::Amt::new_with_bit_width(store, bit_width);
    for (index, value) in values.into_iter().enumerate() {
        array
            .set(index as u64, value)
            .map_err(|error| NodeError::Runtime(format!("encoding native array: {error}")))?;
    }
    array.flush().map_err(|error| NodeError::Runtime(format!("flushing native array: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use fvm_ipld_blockstore::MemoryBlockstore;
    use multihash_codetable::MultihashDigest;

    #[test]
    fn native_header_and_key_have_lotus_canonical_cid_encodings() {
        let store = MemoryBlockstore::default();
        let root = store.put_cbor(&Vec::<Cid>::new(), Code::Blake2b256).unwrap();
        let block = Block {
            hash: B256::repeat_byte(1),
            parent_hash: B256::ZERO,
            number: 42,
            state_root: B256::ZERO,
            timestamp: 1_800_000_000,
            gas_limit: 10_000_000_000,
            gas_used: 0,
            base_fee_per_gas: 100,
            transactions: Vec::new(),
            logs: Vec::new(),
        };
        let tipset = project_tipset(&store, &block, None, root, root, root).unwrap();
        let header = store.get(&tipset.cid).unwrap().unwrap();
        assert_eq!(header[0], 0x90, "Lotus headers contain exactly 16 tuple fields");
        assert_eq!(
            tipset.cid,
            Cid::new_v1(fvm_ipld_encoding::DAG_CBOR, Code::Blake2b256.digest(&header))
        );
        let mut expected_key = vec![0x58, 38];
        expected_key.extend_from_slice(&tipset.cid.to_bytes());
        assert_eq!(store.get(&tipset.key_cid).unwrap().unwrap(), expected_key);
        assert_eq!(tipset.beacon, DeterministicExterns::beacon_digest(42));
        let mut child = block;
        child.number += 1;
        child.timestamp += 1;
        child.hash = B256::repeat_byte(2);
        let child = project_tipset(&store, &child, Some(&tipset), root, root, root).unwrap();
        assert_eq!(child.parents, vec![tipset.cid]);
        assert_ne!(child.cid, tipset.cid);
        assert_ne!(child.key_cid, tipset.key_cid);
    }
}
