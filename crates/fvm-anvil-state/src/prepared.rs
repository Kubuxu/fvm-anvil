// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Versioned metadata for standalone state archives produced by Press.

use std::fmt;

use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{CborStore, tuple::*};
use fvm_shared::address::Address;
use fvm_shared::econ::TokenAmount;
use multihash_codetable::Code;
use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::snapshot::SnapshotContext;

const FORMAT_NAME: &str = "fvm-anvil-press-state";
const FORMAT_VERSION: u64 = 2;
const STATE_TREE_VERSION: u64 = 5;

/// A runtime-bytecode replacement applied to an EVM actor.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedBytecodePatch {
    pub source: Cid,
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    pub source_hash: Vec<u8>,
    pub replacement: Cid,
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    pub replacement_hash: Vec<u8>,
}

/// One EVM storage-slot replacement applied to an actor.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedStoragePatch {
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    pub key: Vec<u8>,
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    pub source: Vec<u8>,
    #[serde(with = "fvm_ipld_encoding::strict_bytes")]
    pub replacement: Vec<u8>,
}

/// An actor balance replacement.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedBalancePatch {
    pub source: TokenAmount,
    pub replacement: TokenAmount,
}

/// An actor sequence replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedSequencePatch {
    pub source: u64,
    pub replacement: u64,
}

/// Transformations applied to one retained actor.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedActorPatch {
    pub runtime_code: Option<PreparedBytecodePatch>,
    pub storage: Vec<PreparedStoragePatch>,
    pub balance: Option<PreparedBalancePatch>,
    pub sequence: Option<PreparedSequencePatch>,
}

/// One source actor retained in a prepared state tree.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedActor {
    pub id: u64,
    pub code: Cid,
    pub source_state: Cid,
    pub state: Cid,
    pub sequence: u64,
    pub balance: TokenAmount,
    pub delegated_address: Option<Address>,
    pub requested_addresses: Vec<String>,
    pub patch: Option<PreparedActorPatch>,
}

/// The root block of a standalone Press state archive.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize_tuple, Serialize_tuple)]
pub struct PreparedStateMetadata {
    format: String,
    version: u64,
    state_tree_version: u64,
    pub state_root: Cid,
    pub source_head_tipset: Vec<Cid>,
    pub source_parent_state_root: Cid,
    pub epoch: i64,
    pub timestamp: u64,
    pub base_fee: TokenAmount,
    pub network_version: u32,
    pub actor_manifest_data: Cid,
    pub external_actor_codes: Vec<Cid>,
    pub actors: Vec<PreparedActor>,
}

impl PreparedStateMetadata {
    pub fn new(
        state_root: Cid,
        source: &SnapshotContext,
        network_version: u32,
        actor_manifest_data: Cid,
        external_actor_codes: Vec<Cid>,
        actors: Vec<PreparedActor>,
    ) -> Self {
        Self {
            format: FORMAT_NAME.to_owned(),
            version: FORMAT_VERSION,
            state_tree_version: STATE_TREE_VERSION,
            state_root,
            source_head_tipset: source.head_tipset.clone(),
            source_parent_state_root: source.state_root,
            epoch: source.epoch,
            timestamp: source.timestamp,
            base_fee: source.base_fee.clone(),
            network_version,
            actor_manifest_data,
            external_actor_codes,
            actors,
        }
    }

    pub fn validate(&self) -> Result<(), PreparedStateError> {
        if self.format != FORMAT_NAME {
            return Err(PreparedStateError::WrongFormat(self.format.clone()));
        }
        if self.version != FORMAT_VERSION {
            return Err(PreparedStateError::UnsupportedVersion(self.version));
        }
        if self.state_tree_version != STATE_TREE_VERSION {
            return Err(PreparedStateError::UnsupportedStateTreeVersion(self.state_tree_version));
        }
        Ok(())
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn state_tree_version(&self) -> u64 {
        self.state_tree_version
    }
}

struct MetadataVersionProbe {
    format: String,
    version: u64,
}

impl<'de> Deserialize<'de> for MetadataVersionProbe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProbeVisitor;

        impl<'de> Visitor<'de> for ProbeVisitor {
            type Value = MetadataVersionProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Press metadata tuple")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let format = sequence
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("metadata format is missing"))?;
                let version = sequence
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("metadata version is missing"))?;
                while sequence.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MetadataVersionProbe { format, version })
            }
        }

        deserializer.deserialize_seq(ProbeVisitor)
    }
}

#[derive(Debug, Error)]
pub enum PreparedStateError {
    #[error("failed to read prepared-state metadata block {cid}: {message}")]
    Blockstore { cid: Box<Cid>, message: String },
    #[error("prepared-state metadata block {0} is missing")]
    MissingBlock(Box<Cid>),
    #[error("failed to encode prepared-state metadata: {0}")]
    Encode(String),
    #[error("failed to decode Press state metadata version {version}: {message}")]
    Decode { version: u64, message: String },
    #[error("unsupported archive format {0:?}")]
    WrongFormat(String),
    #[error("unsupported Press state metadata version {0}")]
    UnsupportedVersion(u64),
    #[error("unsupported prepared state-tree version {0}")]
    UnsupportedStateTreeVersion(u64),
}

/// Attempts to decode a single CAR root as Press metadata.
///
/// `Ok(None)` means the roots describe another archive type, such as an
/// FRC-0108 Filecoin snapshot.
pub fn load_prepared_metadata(
    store: &impl Blockstore,
    roots: &[Cid],
) -> Result<Option<(Cid, PreparedStateMetadata)>, PreparedStateError> {
    let [root] = roots else {
        return Ok(None);
    };
    let bytes = store
        .get(root)
        .map_err(|error| PreparedStateError::Blockstore {
            cid: Box::new(*root),
            message: error.to_string(),
        })?
        .ok_or_else(|| PreparedStateError::MissingBlock(Box::new(*root)))?;
    let metadata = match fvm_ipld_encoding::from_slice::<PreparedStateMetadata>(&bytes) {
        Ok(metadata) => metadata,
        Err(error) => {
            let Ok(probe) = fvm_ipld_encoding::from_slice::<MetadataVersionProbe>(&bytes) else {
                return Ok(None);
            };
            if probe.format != FORMAT_NAME {
                return Ok(None);
            }
            if probe.version != FORMAT_VERSION {
                return Err(PreparedStateError::UnsupportedVersion(probe.version));
            }
            return Err(PreparedStateError::Decode {
                version: probe.version,
                message: error.to_string(),
            });
        }
    };
    metadata.validate()?;
    Ok(Some((*root, metadata)))
}

pub fn put_prepared_metadata(
    store: &impl Blockstore,
    metadata: &PreparedStateMetadata,
) -> Result<Cid, PreparedStateError> {
    store
        .put_cbor(metadata, Code::Blake2b256)
        .map_err(|error| PreparedStateError::Encode(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fvm_ipld_blockstore::MemoryBlockstore;

    fn cid(seed: u8) -> Cid {
        Cid::new_v1(0x71, cid::multihash::Multihash::<64>::wrap(0, &[seed]).unwrap())
    }

    #[test]
    fn metadata_round_trips_through_its_car_root() {
        let store = MemoryBlockstore::default();
        let context = SnapshotContext {
            head_tipset: vec![cid(1)],
            state_root: cid(2),
            epoch: 3,
            timestamp: 4,
            base_fee: TokenAmount::from_atto(5),
        };
        let metadata = PreparedStateMetadata::new(
            cid(6),
            &context,
            28,
            cid(7),
            vec![cid(8)],
            vec![PreparedActor {
                id: 100,
                code: cid(8),
                source_state: cid(9),
                state: cid(10),
                sequence: 11,
                balance: TokenAmount::from_atto(12),
                delegated_address: None,
                requested_addresses: vec!["f0100".into()],
                patch: None,
            }],
        );
        let root = put_prepared_metadata(&store, &metadata).unwrap();

        let (loaded_root, loaded) = load_prepared_metadata(&store, &[root]).unwrap().unwrap();
        assert_eq!(loaded_root, root);
        assert_eq!(loaded, metadata);
    }

    #[test]
    fn reports_unsupported_metadata_before_decoding_its_body() {
        let store = MemoryBlockstore::default();
        let root = store.put_cbor(&(FORMAT_NAME, 1_u64, 5_u64), Code::Blake2b256).unwrap();

        let error = load_prepared_metadata(&store, &[root]).unwrap_err();
        assert!(matches!(error, PreparedStateError::UnsupportedVersion(1)));
    }
}
