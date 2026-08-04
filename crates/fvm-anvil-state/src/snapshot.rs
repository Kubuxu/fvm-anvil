// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::tuple::*;
use fvm_shared::{address::Address, econ::TokenAmount};
use serde::Deserialize;
use serde::de::IgnoredAny;
use thiserror::Error;

/// Filecoin execution context described by a snapshot's head tipset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotContext {
    pub head_tipset: Vec<Cid>,
    pub state_root: Cid,
    pub epoch: i64,
    pub timestamp: u64,
    pub base_fee: TokenAmount,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot has no roots")]
    NoRoots,
    #[error("failed to read snapshot block {cid}: {message}")]
    Blockstore { cid: Box<Cid>, message: String },
    #[error("snapshot block {0} is missing")]
    MissingBlock(Box<Cid>),
    #[error("unsupported FRC-0108 snapshot version {0}")]
    UnsupportedVersion(u64),
    #[error("FRC-0108 v2 metadata has an empty head tipset key")]
    EmptyHeadTipset,
    #[error("failed to decode head block {cid}: {message}")]
    InvalidHeader { cid: Box<Cid>, message: String },
    #[error("inconsistent head headers for {field} at {cid}: expected {expected}, found {actual}")]
    InconsistentHeadHeaders { cid: Box<Cid>, field: &'static str, expected: String, actual: String },
}

// Narrow wire definitions adapted from Forest's `chain/snapshot_format.rs` and
// `blocks/header.rs` (ChainSafe Systems, MIT OR Apache-2.0). They deliberately
// decode only the fields needed to inspect a snapshot.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SnapshotMetadataWire {
    version: u64,
    head_tipset_key: Vec<Cid>,
    #[serde(rename = "F3Data", default)]
    _f3_data: Option<Cid>,
}

#[derive(Deserialize_tuple)]
struct BlockHeaderWire {
    miner_address: Address,
    ticket: Option<IgnoredAny>,
    _election_proof: IgnoredAny,
    _beacon_entries: IgnoredAny,
    _winning_post_proof: IgnoredAny,
    parents: Vec<Cid>,
    _weight: IgnoredAny,
    epoch: i64,
    state_root: Cid,
    _message_receipts: IgnoredAny,
    _messages: IgnoredAny,
    _bls_aggregate: IgnoredAny,
    timestamp: u64,
    _signature: IgnoredAny,
    _fork_signal: IgnoredAny,
    parent_base_fee: TokenAmount,
}

/// Inspect the Filecoin execution context at the head of an FRC-0108 snapshot.
///
/// V1 snapshots place the head block-header CIDs directly in the CAR roots. V2
/// snapshots have one root containing metadata whose `HeadTipsetKey` points to
/// the head headers.
pub fn inspect_filecoin_snapshot(
    store: &impl Blockstore,
    roots: &[Cid],
) -> Result<SnapshotContext, SnapshotError> {
    if roots.is_empty() {
        return Err(SnapshotError::NoRoots);
    }

    // A one-block v1 tipset is ambiguous with v2 until the root block is decoded.
    // Retain the fetched bytes when it is a v1 header to avoid a second store read.
    let mut prefetched_v1_header = None;
    let head_tipset = if roots.len() == 1 {
        let bytes = read_block(store, &roots[0])?;
        match fvm_ipld_encoding::from_slice::<SnapshotMetadataWire>(&bytes) {
            Ok(metadata) => {
                if metadata.version != 2 {
                    return Err(SnapshotError::UnsupportedVersion(metadata.version));
                }
                if metadata.head_tipset_key.is_empty() {
                    return Err(SnapshotError::EmptyHeadTipset);
                }
                metadata.head_tipset_key
            }
            Err(_) => {
                prefetched_v1_header = Some(bytes);
                roots.to_vec()
            }
        }
    } else {
        roots.to_vec()
    };

    let (first_cid, remaining_cids) =
        head_tipset.split_first().ok_or(SnapshotError::EmptyHeadTipset)?;
    let first = decode_header(
        first_cid,
        match prefetched_v1_header.take() {
            Some(bytes) => bytes,
            None => read_block(store, first_cid)?,
        },
    )?;
    let BlockHeaderWire {
        miner_address: first_miner,
        ticket: first_ticket,
        parents,
        epoch,
        state_root,
        mut timestamp,
        parent_base_fee: base_fee,
        ..
    } = first;
    if first_ticket.is_none() {
        return Err(inconsistent(first_cid, "ticket", "present".into(), "missing".into()));
    }

    let mut seen_cids = HashSet::from([first_cid.to_owned()]);
    let mut seen_miners = HashSet::from([first_miner]);

    for cid in remaining_cids {
        if !seen_cids.insert(cid.to_owned()) {
            return Err(inconsistent(cid, "header CID", "unique".into(), cid.to_string()));
        }

        let header = decode_header(cid, read_block(store, cid)?)?;
        if header.ticket.is_none() {
            return Err(inconsistent(cid, "ticket", "present".into(), "missing".into()));
        }
        if header.parents != parents {
            return Err(inconsistent(
                cid,
                "parent tipset key",
                format!("{parents:?}"),
                format!("{:?}", header.parents),
            ));
        }
        if seen_miners.contains(&header.miner_address) {
            return Err(inconsistent(
                cid,
                "miner",
                "unique".into(),
                header.miner_address.to_string(),
            ));
        }
        seen_miners.insert(header.miner_address);
        if header.epoch != epoch {
            return Err(inconsistent(cid, "epoch", epoch.to_string(), header.epoch.to_string()));
        }
        if header.state_root != state_root {
            return Err(inconsistent(
                cid,
                "parent state root",
                state_root.to_string(),
                header.state_root.to_string(),
            ));
        }
        if header.parent_base_fee != base_fee {
            return Err(inconsistent(
                cid,
                "parent base fee",
                base_fee.atto().to_string(),
                header.parent_base_fee.atto().to_string(),
            ));
        }
        timestamp = timestamp.min(header.timestamp);
    }

    Ok(SnapshotContext { head_tipset, state_root, epoch, timestamp, base_fee })
}

fn read_block(store: &impl Blockstore, cid: &Cid) -> Result<Vec<u8>, SnapshotError> {
    store
        .get(cid)
        .map_err(|error| SnapshotError::Blockstore {
            cid: Box::new(cid.to_owned()),
            message: error.to_string(),
        })?
        .ok_or_else(|| SnapshotError::MissingBlock(Box::new(cid.to_owned())))
}

fn decode_header(cid: &Cid, bytes: Vec<u8>) -> Result<BlockHeaderWire, SnapshotError> {
    fvm_ipld_encoding::from_slice(&bytes).map_err(|error| SnapshotError::InvalidHeader {
        cid: Box::new(cid.to_owned()),
        message: error.to_string(),
    })
}

fn inconsistent(cid: &Cid, field: &'static str, expected: String, actual: String) -> SnapshotError {
    SnapshotError::InconsistentHeadHeaders {
        cid: Box::new(cid.to_owned()),
        field,
        expected,
        actual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fvm_ipld_blockstore::MemoryBlockstore;
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct TestMetadata {
        version: u64,
        head_tipset_key: Vec<Cid>,
        f3_data: Option<Cid>,
    }

    #[derive(Serialize)]
    struct TestBlockHeader(
        Address,
        Option<(Vec<u8>,)>,
        Option<()>,
        Vec<()>,
        Vec<()>,
        Vec<Cid>,
        i64,
        i64,
        Cid,
        Cid,
        Cid,
        Option<()>,
        u64,
        Option<()>,
        u64,
        TokenAmount,
    );

    fn cid(seed: u8) -> Cid {
        let digest = cid::multihash::Multihash::<64>::wrap(0x00, &[seed]).unwrap();
        Cid::new_v1(fvm_ipld_encoding::DAG_CBOR, digest)
    }

    fn put(store: &MemoryBlockstore, seed: u8, value: &impl Serialize) -> Cid {
        let cid = cid(seed);
        let bytes = fvm_ipld_encoding::to_vec(value).unwrap();
        store.put_keyed(&cid, &bytes).unwrap();
        cid
    }

    fn header(
        miner: u64,
        epoch: i64,
        state_root: Cid,
        timestamp: u64,
        base_fee: u64,
    ) -> TestBlockHeader {
        TestBlockHeader(
            Address::new_id(miner),
            Some((vec![1],)),
            None,
            Vec::new(),
            Vec::new(),
            vec![cid(239)],
            0,
            epoch,
            state_root,
            cid(240),
            cid(241),
            None,
            timestamp,
            None,
            0,
            TokenAmount::from_atto(base_fee),
        )
    }

    #[test]
    fn inspects_v1_tipset_and_uses_minimum_timestamp() {
        let store = MemoryBlockstore::default();
        let state_root = cid(10);
        let first = put(&store, 1, &header(100, 123, state_root, 1_020, 100));
        let second = put(&store, 2, &header(101, 123, state_root, 1_000, 100));

        let context = inspect_filecoin_snapshot(&store, &[first, second]).unwrap();

        assert_eq!(context.head_tipset, vec![first, second]);
        assert_eq!(context.state_root, state_root);
        assert_eq!(context.epoch, 123);
        assert_eq!(context.timestamp, 1_000);
        assert_eq!(context.base_fee, TokenAmount::from_atto(100_u64));
    }

    #[test]
    fn inspects_v2_metadata_head_tipset() {
        let store = MemoryBlockstore::default();
        let state_root = cid(20);
        let first = put(&store, 3, &header(102, 456, state_root, 2_000, 300));
        let second = put(&store, 4, &header(103, 456, state_root, 2_030, 300));
        let metadata = put(
            &store,
            5,
            &TestMetadata { version: 2, head_tipset_key: vec![first, second], f3_data: None },
        );

        let context = inspect_filecoin_snapshot(&store, &[metadata]).unwrap();

        assert_eq!(context.head_tipset, vec![first, second]);
        assert_eq!(context.state_root, state_root);
        assert_eq!(context.epoch, 456);
        assert_eq!(context.timestamp, 2_000);
        assert_eq!(context.base_fee, TokenAmount::from_atto(300_u64));
    }

    fn assert_inconsistent(second: TestBlockHeader, expected_field: &'static str) {
        let store = MemoryBlockstore::default();
        let state_root = cid(30);
        let first_cid = put(&store, 6, &header(104, 900, state_root, 3_000, 500));
        let second_cid = put(&store, 7, &second);

        let error = inspect_filecoin_snapshot(&store, &[first_cid, second_cid]).unwrap_err();
        assert!(matches!(
            error,
            SnapshotError::InconsistentHeadHeaders { field, .. } if field == expected_field
        ));
    }

    #[test]
    fn rejects_inconsistent_head_headers() {
        let state_root = cid(30);
        assert_inconsistent(header(105, 901, state_root, 3_000, 500), "epoch");
        assert_inconsistent(header(105, 900, cid(31), 3_000, 500), "parent state root");
        assert_inconsistent(header(105, 900, state_root, 3_000, 501), "parent base fee");
    }

    #[test]
    fn rejects_missing_ticket() {
        let state_root = cid(40);
        let mut second = header(107, 1_000, state_root, 4_000, 600);
        second.1 = None;
        assert_inconsistent(second, "ticket");
    }

    #[test]
    fn rejects_mismatched_parent_tipset_key() {
        let state_root = cid(30);
        let mut second = header(108, 900, state_root, 3_000, 500);
        second.5 = vec![cid(238)];
        assert_inconsistent(second, "parent tipset key");
    }

    #[test]
    fn rejects_duplicate_miners() {
        let state_root = cid(30);
        assert_inconsistent(header(104, 900, state_root, 3_000, 500), "miner");
    }

    #[test]
    fn rejects_duplicate_header_cids() {
        let store = MemoryBlockstore::default();
        let state_root = cid(50);
        let header_cid = put(&store, 8, &header(109, 1_100, state_root, 5_000, 700));

        let error = inspect_filecoin_snapshot(&store, &[header_cid, header_cid]).unwrap_err();
        assert!(matches!(
            error,
            SnapshotError::InconsistentHeadHeaders { field: "header CID", .. }
        ));
    }
}
