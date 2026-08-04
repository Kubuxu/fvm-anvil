// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end construction of standalone Press state archives.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use tempfile::Builder;
use thiserror::Error;

use crate::forest_car::{ForestCar, ForestCarWriter};
use crate::prepared::{PreparedStateMetadata, put_prepared_metadata};
use crate::snapshot::SnapshotContext;
use crate::state::{ActorSelector, construct_standalone_state};
use crate::traversal::traverse_blockstore;
use crate::verify::{VerificationSummary, verify_prepared_archive};

const MIN_NETWORK_VERSION: u32 = 21;
const MAX_NETWORK_VERSION: u32 = 28;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildSummary {
    pub metadata_root: Cid,
    pub state_root: Cid,
    pub actors: usize,
    pub blocks: u64,
}

#[derive(Debug, Error)]
pub enum ArchiveBuildError {
    #[error("output archive already exists: {0}")]
    OutputExists(PathBuf),
    #[error("output path has no parent directory: {0}")]
    MissingParent(PathBuf),
    #[error("unsupported network version {0}; this ref-FVM supports versions 21 through 28")]
    UnsupportedNetworkVersion(u32),
    #[error("failed to construct selected state: {0}")]
    Construct(String),
    #[error("failed to encode prepared metadata: {0}")]
    Metadata(String),
    #[error("new metadata block {0} is missing from the construction store")]
    MissingMetadata(Box<Cid>),
    #[error("failed to create ForestCAR output: {0}")]
    Writer(String),
    #[error("failed to copy selected actor state: {0}")]
    Traversal(String),
    #[error("failed to reopen prepared archive: {0}")]
    Reopen(String),
    #[error("prepared archive failed structural verification: {0}")]
    Verify(String),
    #[error("failed to publish prepared archive: {0}")]
    Publish(String),
}

struct LayeredStore<'a, BS, OS> {
    overlay: &'a OS,
    source: &'a BS,
}

impl<BS: Blockstore, OS: Blockstore> Blockstore for LayeredStore<'_, BS, OS> {
    fn get(&self, cid: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
        match self.overlay.get(cid)? {
            Some(block) => Ok(Some(block)),
            None => self.source.get(cid),
        }
    }

    fn put_keyed(&self, _: &Cid, _: &[u8]) -> anyhow::Result<()> {
        Err(anyhow!("standalone archive traversal store is read-only"))
    }
}

pub fn build_prepared_archive<BS: Blockstore>(
    source: &BS,
    context: &SnapshotContext,
    selectors: &[ActorSelector],
    network_version: u32,
    output: &Path,
    overwrite: bool,
) -> Result<BuildSummary, ArchiveBuildError> {
    if !(MIN_NETWORK_VERSION..=MAX_NETWORK_VERSION).contains(&network_version) {
        return Err(ArchiveBuildError::UnsupportedNetworkVersion(network_version));
    }
    if !overwrite && output.exists() {
        return Err(ArchiveBuildError::OutputExists(output.to_path_buf()));
    }
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(ArchiveBuildError::MissingParent(parent.to_path_buf()));
    }

    let constructed = construct_standalone_state(source, &context.state_root, selectors)
        .map_err(|error| ArchiveBuildError::Construct(error.to_string()))?;
    let metadata = PreparedStateMetadata::new(
        constructed.state_root,
        context,
        network_version,
        constructed.actor_manifest_data,
        constructed.external_actor_codes.iter().copied().collect(),
        constructed.actors,
    );
    let metadata_root = put_prepared_metadata(constructed.store.as_ref(), &metadata)
        .map_err(|error| ArchiveBuildError::Metadata(error.to_string()))?;
    let metadata_block = constructed
        .store
        .get(&metadata_root)
        .map_err(|error| ArchiveBuildError::Metadata(error.to_string()))?
        .ok_or_else(|| ArchiveBuildError::MissingMetadata(Box::new(metadata_root)))?;

    let temporary = Builder::new()
        .prefix(".press-build-")
        .suffix(".forest.car.zst")
        .tempfile_in(parent)
        .map_err(|error| ArchiveBuildError::Writer(error.to_string()))?
        .into_temp_path();
    std::fs::remove_file(&temporary)
        .map_err(|error| ArchiveBuildError::Writer(error.to_string()))?;

    let mut writer = ForestCarWriter::create(&temporary, &[metadata_root])
        .map_err(|error| ArchiveBuildError::Writer(error.to_string()))?;
    writer
        .write_block(&metadata_root, &metadata_block)
        .map_err(|error| ArchiveBuildError::Writer(error.to_string()))?;
    let layered = LayeredStore { overlay: constructed.store.as_ref(), source };
    let copied = traverse_blockstore(
        &layered,
        &[constructed.state_root],
        |cid| constructed.external_links.contains(cid),
        |cid, data| writer.write_block(cid, data).map_err(|error| anyhow!(error)),
    )
    .map_err(|error| ArchiveBuildError::Traversal(error.to_string()))?;
    writer.finish().map_err(|error| ArchiveBuildError::Writer(error.to_string()))?;

    let archive = ForestCar::open(&temporary)
        .map_err(|error| ArchiveBuildError::Reopen(error.to_string()))?;
    let (_, VerificationSummary { actors, reachable_blocks }) =
        verify_prepared_archive(&archive, archive.roots())
            .map_err(|error| ArchiveBuildError::Verify(error.to_string()))?;
    if reachable_blocks != copied {
        return Err(ArchiveBuildError::Verify(format!(
            "wrote {copied} reachable blocks but reopened {reachable_blocks}"
        )));
    }

    let persist =
        if overwrite { temporary.persist(output) } else { temporary.persist_noclobber(output) };
    persist.map_err(|error| ArchiveBuildError::Publish(error.error.to_string()))?;
    sync_parent(output).map_err(|error| ArchiveBuildError::Publish(error.to_string()))?;

    Ok(BuildSummary {
        metadata_root,
        state_root: constructed.state_root,
        actors,
        blocks: copied + 1,
    })
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use fvm::state_tree::StateTree;
    use fvm_ipld_blockstore::{Blockstore as _, MemoryBlockstore};
    use fvm_ipld_encoding::{CborStore, tuple::*};
    use fvm_ipld_hamt::{Config, Hamt};
    use fvm_shared::address::Address;
    use fvm_shared::econ::TokenAmount;
    use fvm_shared::state::{ActorState, StateTreeVersion};
    use multihash_codetable::Code;

    use super::*;
    use crate::state::ActorSelector;

    #[derive(Serialize_tuple)]
    struct SystemState {
        builtin_actors: Cid,
    }

    #[derive(Serialize_tuple)]
    struct InitState {
        address_map: Cid,
        next_id: u64,
        network_name: String,
    }

    #[derive(Serialize_tuple)]
    struct LinkedState {
        child: Cid,
    }

    fn external(seed: u8) -> Cid {
        Cid::new_v1(0x55, cid::multihash::Multihash::<64>::wrap(0, &[seed]).unwrap())
    }

    fn actor(code: Cid, state: Cid, delegated_address: Option<Address>) -> ActorState {
        ActorState { code, state, sequence: 0, balance: TokenAmount::default(), delegated_address }
    }

    #[test]
    fn rejects_network_versions_unsupported_by_ref_fvm() {
        let source = MemoryBlockstore::default();
        let context = SnapshotContext {
            head_tipset: vec![external(20)],
            state_root: external(21),
            epoch: 1,
            timestamp: 2,
            base_fee: TokenAmount::from_atto(3),
        };
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("unsupported.press.car.zst");

        let error = build_prepared_archive(&source, &context, &[], 20, &output, false).unwrap_err();
        assert!(matches!(error, ArchiveBuildError::UnsupportedNetworkVersion(20)));
        assert!(!output.exists());
    }

    #[test]
    fn builds_archive_that_is_independent_of_source_state() {
        let source = Rc::new(MemoryBlockstore::default());
        let target_address = Address::new_delegated(10, &[1; 20]).unwrap();
        let target_robust_address = Address::new_actor(b"target");
        let config = Config { bit_width: 5, min_data_depth: 0, max_array_width: 3 };
        let mut init_map = Hamt::<_, u64>::new_with_config(Rc::clone(&source), config);
        init_map.set(target_address.to_bytes().into(), 100).unwrap();
        init_map.set(target_robust_address.to_bytes().into(), 100).unwrap();
        let init_map_root = init_map.flush().unwrap();
        let init_state = source
            .put_cbor(
                &InitState {
                    address_map: init_map_root,
                    next_id: 102,
                    network_name: "testnet".into(),
                },
                Code::Blake2b256,
            )
            .unwrap();
        let manifest = external(1);
        let system_state =
            source.put_cbor(&SystemState { builtin_actors: manifest }, Code::Blake2b256).unwrap();
        let empty_state = source.put_cbor(&(), Code::Blake2b256).unwrap();
        let target_leaf = source.put_cbor(&("target", 42_u64), Code::Blake2b256).unwrap();
        let target_state =
            source.put_cbor(&LinkedState { child: target_leaf }, Code::Blake2b256).unwrap();
        let unrelated_state = source.put_cbor(&("unrelated",), Code::Blake2b256).unwrap();
        let target_code = source.put_cbor(&("custom-code",), Code::Blake2b256).unwrap();

        let mut tree = StateTree::new(Rc::clone(&source), StateTreeVersion::V5).unwrap();
        tree.set_actor(0, actor(external(10), system_state, None));
        tree.set_actor(1, actor(external(11), init_state, None));
        tree.set_actor(2, actor(external(12), empty_state, None));
        tree.set_actor(10, actor(external(13), empty_state, None));
        tree.set_actor(99, actor(external(14), empty_state, None));
        tree.set_actor(100, actor(target_code, target_state, Some(target_address)));
        tree.set_actor(101, actor(external(15), unrelated_state, None));
        let source_root = tree.flush().unwrap();
        let context = SnapshotContext {
            head_tipset: vec![external(20)],
            state_root: source_root,
            epoch: 1,
            timestamp: 2,
            base_fee: TokenAmount::from_atto(3),
        };
        let selector = ActorSelector {
            input: "0x0101010101010101010101010101010101010101".into(),
            address: target_address,
        };
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("target.press.car.zst");

        let summary =
            build_prepared_archive(source.as_ref(), &context, &[selector], 28, &output, false)
                .unwrap();
        drop(tree);
        drop(source);

        let archive = ForestCar::open(&output).unwrap();
        let (metadata, verified) = verify_prepared_archive(&archive, archive.roots()).unwrap();
        assert_eq!(summary.actors, 6);
        assert_eq!(verified.actors, 6);
        assert_eq!(metadata.state_root, summary.state_root);
        let output_tree = StateTree::new_from_root(&archive, &metadata.state_root).unwrap();
        assert_eq!(output_tree.lookup_id(&target_address).unwrap(), Some(100));
        assert_eq!(output_tree.lookup_id(&target_robust_address).unwrap(), Some(100));
        assert!(output_tree.get_actor(101).unwrap().is_none());
        assert!(archive.get(&target_leaf).unwrap().is_some());
        assert!(archive.get(&target_code).unwrap().is_some());
    }
}
