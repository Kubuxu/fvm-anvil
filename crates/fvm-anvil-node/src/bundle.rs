use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read};

use cid::Cid;
use fvm::machine::Manifest;
use fvm_ipld_blockstore::{Blockstore, MemoryBlockstore};
use fvm_ipld_car::load_car;
use fvm_ipld_encoding::CborStore;

use crate::config::ActorBundle;
use crate::error::{BundleError, NodeError};

const REQUIRED_ACTORS: [&str; 8] =
    ["system", "init", "reward", "account", "eam", "placeholder", "ethaccount", "evm"];

pub(crate) struct BundleArtifacts {
    pub(crate) root: Cid,
    pub(crate) manifest_data: Cid,
    codes_by_name: BTreeMap<String, Cid>,
    all_codes: Vec<Cid>,
}

impl BundleArtifacts {
    pub(crate) fn code(&self, name: &'static str) -> Result<Cid, BundleError> {
        self.codes_by_name.get(name).copied().ok_or(BundleError::MissingActor(name))
    }

    pub(crate) fn all_codes(&self) -> &[Cid] {
        &self.all_codes
    }
}

pub(crate) fn load_bundle(
    selection: &ActorBundle,
    store: &MemoryBlockstore,
) -> Result<BundleArtifacts, NodeError> {
    let expected_root = selection.expected_root()?;
    let roots = match selection {
        ActorBundle::Official => {
            let url = selection.source_url()?.expect("the official actor bundle always has a URL");
            let response = ureq::get(url.as_str()).call().map_err(|error| {
                BundleError::Download { url: url.to_string(), detail: error.to_string() }
            })?;
            let mut bytes = Vec::new();
            response.into_body().into_reader().read_to_end(&mut bytes).map_err(|error| {
                BundleError::Download { url: url.to_string(), detail: error.to_string() }
            })?;
            load_car(store, Cursor::new(bytes))
                .map_err(|error| BundleError::Car(error.to_string()))?
        }
        ActorBundle::Local { path, .. } => {
            let file = File::open(path)
                .map_err(|source| BundleError::Open { path: path.clone(), source })?;
            load_car(store, BufReader::new(file))
                .map_err(|error| BundleError::Car(error.to_string()))?
        }
    };

    let actual_root = match roots.as_slice() {
        [root] => *root,
        roots => return Err(BundleError::RootCount(roots.len()).into()),
    };
    if actual_root != expected_root {
        return Err(BundleError::RootMismatch {
            expected: Box::new(expected_root),
            actual: Box::new(actual_root),
        }
        .into());
    }

    let manifest_info = store
        .get_cbor::<(u32, Cid)>(&actual_root)
        .map_err(|error| BundleError::InvalidManifestInformation {
            root: Box::new(actual_root),
            detail: error.to_string(),
        })?
        .ok_or(BundleError::MissingManifestInformation(Box::new(actual_root)))?;
    let (manifest_format, manifest_data) = manifest_info;
    if manifest_format != 1 {
        return Err(BundleError::UnsupportedManifestFormat(manifest_format).into());
    }

    Manifest::load(store, &manifest_data, manifest_format)
        .map_err(|error| BundleError::Manifest(error.to_string()))?;
    let entries = store
        .get_cbor::<Vec<(String, Cid)>>(&manifest_data)
        .map_err(|error| BundleError::Manifest(error.to_string()))?
        .ok_or_else(|| BundleError::Manifest(format!("manifest data {manifest_data} is absent")))?;
    let codes_by_name = entries.iter().cloned().collect::<BTreeMap<_, _>>();
    for name in REQUIRED_ACTORS {
        let code = codes_by_name.get(name).copied().ok_or(BundleError::MissingActor(name))?;
        if !store.has(&code).map_err(|error| BundleError::Manifest(error.to_string()))? {
            return Err(BundleError::MissingActorCode {
                name: name.to_owned(),
                code: Box::new(code),
            }
            .into());
        }
    }
    let all_codes = entries.into_iter().map(|(_, code)| code).collect();

    Ok(BundleArtifacts { root: actual_root, manifest_data, codes_by_name, all_codes })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use cid::Cid;
    use fvm_ipld_blockstore::MemoryBlockstore;
    use fvm_ipld_car::{CarHeader, CarWriter};
    use fvm_shared::IPLD_RAW;
    use multihash_codetable::{Code, MultihashDigest};

    use super::load_bundle;
    use crate::config::ActorBundle;
    use crate::error::{BundleError, NodeError};

    #[test]
    fn rejects_a_car_with_multiple_roots() {
        let first = test_cid(b"first");
        let second = test_cid(b"second");
        let path = write_header("multiple", vec![first, second]);
        let selection = ActorBundle::Local { path: path.clone(), expected_root: first };

        let result = load_bundle(&selection, &MemoryBlockstore::default());
        fs::remove_file(path).unwrap();

        assert!(matches!(result, Err(NodeError::Bundle(BundleError::RootCount(2)))));
    }

    #[test]
    fn rejects_a_car_whose_root_is_not_expected() {
        let actual = test_cid(b"actual");
        let expected = test_cid(b"expected");
        let path = write_header("mismatch", vec![actual]);
        let selection = ActorBundle::Local { path: path.clone(), expected_root: expected };

        let result = load_bundle(&selection, &MemoryBlockstore::default());
        fs::remove_file(path).unwrap();

        assert!(matches!(
            result,
            Err(NodeError::Bundle(BundleError::RootMismatch {
                expected: found_expected,
                actual: found_actual,
            })) if *found_expected == expected && *found_actual == actual
        ));
    }

    fn test_cid(seed: &[u8]) -> Cid {
        Cid::new_v1(IPLD_RAW, Code::Blake2b256.digest(seed))
    }

    fn write_header(name: &str, roots: Vec<Cid>) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("fvm-anvil-node-{name}-{}.car", std::process::id()));
        let mut bytes = Vec::new();
        {
            let mut writer = CarWriter::new(CarHeader::from(roots), &mut bytes).unwrap();
            writer.flush().unwrap();
        }
        fs::write(&path, bytes).unwrap();
        path
    }
}
