use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use cid::Cid;
use fvm::machine::Manifest;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_car::load_car;
use fvm_ipld_encoding::CborStore;
use fvm_shared::version::NetworkVersion;

use crate::config::ActorBundle;
use crate::error::{BundleError, NodeError};

const REQUIRED_ACTORS: [&str; 8] =
    ["system", "init", "reward", "account", "eam", "placeholder", "ethaccount", "evm"];

pub(crate) struct BundleArtifacts {
    pub(crate) root: Cid,
    pub(crate) manifest_data: Cid,
    codes_by_name: BTreeMap<String, Cid>,
}

impl BundleArtifacts {
    pub(crate) fn code(&self, name: &'static str) -> Result<Cid, BundleError> {
        self.codes_by_name.get(name).copied().ok_or(BundleError::MissingActor(name))
    }
}

pub(crate) fn load_bundle<BS: Blockstore>(
    selection: &ActorBundle,
    network_version: NetworkVersion,
    cache_path: Option<&Path>,
    store: &BS,
) -> Result<BundleArtifacts, NodeError> {
    let expected_root = selection.expected_root(network_version)?;
    match selection {
        ActorBundle::Official => {
            if let Some(path) = cache_path
                && path.is_file()
            {
                match File::open(path)
                    .map_err(|source| BundleError::Open { path: path.to_owned(), source })
                    .and_then(|file| load_and_validate(store, BufReader::new(file), expected_root))
                {
                    Ok(bundle) => {
                        tracing::debug!(path = %path.display(), "loaded cached actor bundle");
                        return Ok(bundle);
                    }
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "cached actor bundle is invalid; downloading a fresh copy"
                        );
                    }
                }
            }

            let url = selection
                .source_url(network_version)?
                .expect("the official actor bundle always has a URL");
            tracing::info!(%url, "downloading actor bundle");
            let response = ureq::get(url.as_str()).call().map_err(|error| {
                BundleError::Download { url: url.to_string(), detail: error.to_string() }
            })?;
            let mut bytes = Vec::new();
            response.into_body().into_reader().read_to_end(&mut bytes).map_err(|error| {
                BundleError::Download { url: url.to_string(), detail: error.to_string() }
            })?;
            let bundle = load_and_validate(store, Cursor::new(&bytes), expected_root)?;
            if let Some(path) = cache_path
                && let Err(error) = cache_bundle(path, &bytes)
            {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to cache actor bundle"
                );
            }
            Ok(bundle)
        }
        ActorBundle::Local { path } => {
            let file = File::open(path)
                .map_err(|source| BundleError::Open { path: path.clone(), source })?;
            load_and_validate(store, BufReader::new(file), expected_root).map_err(Into::into)
        }
    }
}

fn load_and_validate<BS: Blockstore>(
    store: &BS,
    reader: impl Read,
    expected_root: Option<Cid>,
) -> Result<BundleArtifacts, BundleError> {
    let roots = load_car(store, reader).map_err(|error| BundleError::Car(error.to_string()))?;
    let actual_root = match roots.as_slice() {
        [root] => *root,
        roots => return Err(BundleError::RootCount(roots.len())),
    };
    if let Some(expected_root) = expected_root
        && actual_root != expected_root
    {
        return Err(BundleError::RootMismatch {
            expected: Box::new(expected_root),
            actual: Box::new(actual_root),
        });
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
        return Err(BundleError::UnsupportedManifestFormat(manifest_format));
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
            });
        }
    }
    Ok(BundleArtifacts { root: actual_root, manifest_data, codes_by_name })
}

fn cache_bundle(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("car.{}.tmp", std::process::id()));
    fs::write(&temporary, bytes)?;
    if let Err(first_error) = fs::rename(&temporary, path) {
        if path.is_file() {
            fs::remove_file(path)?;
            fs::rename(&temporary, path)?;
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(first_error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use cid::Cid;
    use fvm_ipld_blockstore::MemoryBlockstore;
    use fvm_ipld_car::{CarHeader, CarWriter};
    use fvm_ipld_encoding::DAG_CBOR;
    use fvm_shared::IPLD_RAW;
    use fvm_shared::version::NetworkVersion;
    use multihash_codetable::{Code, MultihashDigest};

    use super::{cache_bundle, load_bundle};
    use crate::config::ActorBundle;
    use crate::error::{BundleError, NodeError};

    #[test]
    fn rejects_a_car_with_multiple_roots() {
        let first = test_cid(b"first");
        let second = test_cid(b"second");
        let path = write_header("multiple", vec![first, second]);
        let selection = ActorBundle::local(&path).unwrap();

        let result =
            load_bundle(&selection, NetworkVersion::V28, None, &MemoryBlockstore::default());
        fs::remove_file(path).unwrap();

        assert!(matches!(result, Err(NodeError::Bundle(BundleError::RootCount(2)))));
    }

    #[test]
    fn trusts_a_local_cars_declared_root() {
        let root = Cid::new_v1(DAG_CBOR, Code::Blake2b256.digest(b"unlisted-root"));
        let path = write_header("unlisted-root", vec![root]);
        let selection = ActorBundle::local(&path).unwrap();

        let result =
            load_bundle(&selection, NetworkVersion::V28, None, &MemoryBlockstore::default());
        fs::remove_file(path).unwrap();

        assert!(matches!(
            result,
            Err(NodeError::Bundle(BundleError::MissingManifestInformation(found))) if *found == root
        ));
    }
    #[test]
    fn cache_bundle_creates_parent_and_replaces_invalid_data() {
        let directory =
            std::env::temp_dir().join(format!("fvm-anvil-node-cache-{}", std::process::id()));
        let path = directory.join("nested").join("actors.car");

        cache_bundle(&path, b"first").unwrap();
        cache_bundle(&path, b"verified").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"verified");
        fs::remove_dir_all(directory).unwrap();
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
