use std::path::{Path, PathBuf};
use std::str::FromStr;

use cid::Cid;
use fvm_shared::version::NetworkVersion;
use url::Url;

use crate::error::ConfigError;

pub const DEFAULT_CHAIN_ID: u64 = 31_337;
pub const NETWORK_VERSION: NetworkVersion = NetworkVersion::V28;
pub const ACTOR_BUNDLE_VERSION: u32 = 18;
pub const DEFAULT_ACTOR_BUNDLE_ROOT: &str =
    "bafy2bzacedxsqapxwj5znwy4leem5gsuenhhfyqcntews3yis5iek67thy6lc";
pub const DEFAULT_ACTOR_BUNDLE_URL: &str = "https://github.com/filecoin-project/builtin-actors/releases/download/v18.0.0/builtin-actors-mainnet.car";
/// Checked-in Forest actor bundle table entries for builtin-actors v18.0.0.
///
/// Keeping this allowlist local makes the v18 state adapter's compatibility boundary explicit
/// without introducing a production dependency on Forest.
pub const KNOWN_V18_ACTOR_BUNDLE_ROOTS: [&str; 4] = [
    "bafy2bzacebkfatnbe6w4rj7lf6gkjh7mywlrpdh2dj6hu2dl4rmtwksszm2hs",
    "bafy2bzacedg3y3ylp3w4bqmfgyiasrikg3bgfibblc7o4gri3b7j7ftgjhyuu",
    "bafy2bzaced35gjxagazf2fne5dakbok5abmivsh7cq7huwfuptebgwjmcpcf6",
    DEFAULT_ACTOR_BUNDLE_ROOT,
];

fn is_known_v18_bundle_root(root: &Cid) -> bool {
    let root = root.to_string();
    KNOWN_V18_ACTOR_BUNDLE_ROOTS.contains(&root.as_str())
}

/// Selects the builtin-actors v18 CAR used to create genesis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActorBundle {
    /// Downloads the pinned official mainnet v18 CAR and verifies its root.
    Official,
    /// Reads a checked-in, known v18 release CAR from disk and verifies its root.
    Local { path: PathBuf, expected_root: Cid },
}

impl ActorBundle {
    pub fn local(path: impl Into<PathBuf>, expected_root: Cid) -> Result<Self, ConfigError> {
        let bundle = Self::Local { path: path.into(), expected_root };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn actor_version(&self) -> u32 {
        ACTOR_BUNDLE_VERSION
    }

    pub fn expected_root(&self) -> Result<Cid, ConfigError> {
        match self {
            Self::Official => Cid::from_str(DEFAULT_ACTOR_BUNDLE_ROOT)
                .map_err(|error| ConfigError::InvalidOfficialBundleRoot(error.to_string())),
            Self::Local { expected_root, .. } => Ok(*expected_root),
        }
    }

    pub fn source_url(&self) -> Result<Option<Url>, ConfigError> {
        match self {
            Self::Official => Url::parse(DEFAULT_ACTOR_BUNDLE_URL)
                .map(Some)
                .map_err(|error| ConfigError::InvalidOfficialBundleUrl(error.to_string())),
            Self::Local { .. } => Ok(None),
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Official => None,
            Self::Local { path, .. } => Some(path),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Official => {
                self.expected_root()?;
                self.source_url()?;
            }
            Self::Local { path, .. } if path.as_os_str().is_empty() => {
                return Err(ConfigError::EmptyBundlePath);
            }
            Self::Local { expected_root, .. } if !is_known_v18_bundle_root(expected_root) => {
                return Err(ConfigError::UnrecognizedLocalBundleRoot(*expected_root));
            }
            Self::Local { .. } => {}
        }
        Ok(())
    }
}

/// Immutable genesis-time configuration for a node worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeConfig {
    chain_id: u64,
    actor_bundle: ActorBundle,
}

impl NodeConfig {
    pub fn new(chain_id: u64, actor_bundle: ActorBundle) -> Result<Self, ConfigError> {
        if chain_id == 0 {
            return Err(ConfigError::InvalidChainId(chain_id));
        }
        actor_bundle.validate()?;
        Ok(Self { chain_id, actor_bundle })
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn network_version(&self) -> NetworkVersion {
        NETWORK_VERSION
    }

    pub fn actor_bundle(&self) -> &ActorBundle {
        &self.actor_bundle
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self::new(DEFAULT_CHAIN_ID, ActorBundle::Official)
            .expect("the pinned default node configuration must be valid")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACTOR_BUNDLE_VERSION, ActorBundle, DEFAULT_CHAIN_ID, KNOWN_V18_ACTOR_BUNDLE_ROOTS,
        NETWORK_VERSION, NodeConfig,
    };
    use crate::error::ConfigError;
    use cid::Cid;
    use fvm_shared::version::NetworkVersion;
    use std::str::FromStr;

    #[test]
    fn defaults_identify_nv28_and_actor_bundle_v18() {
        let config = NodeConfig::default();

        assert_eq!(config.chain_id(), DEFAULT_CHAIN_ID);
        assert_eq!(config.network_version(), NetworkVersion::V28);
        assert_eq!(config.network_version(), NETWORK_VERSION);
        assert_eq!(config.actor_bundle().actor_version(), ACTOR_BUNDLE_VERSION);
        assert_eq!(config.actor_bundle(), &ActorBundle::Official);
    }

    #[test]
    fn zero_chain_id_is_rejected() {
        assert!(matches!(
            NodeConfig::new(0, ActorBundle::Official),
            Err(ConfigError::InvalidChainId(0))
        ));
    }

    #[test]
    fn every_checked_in_v18_release_root_is_accepted_for_local_bundles() {
        for root in KNOWN_V18_ACTOR_BUNDLE_ROOTS {
            let root = Cid::from_str(root).unwrap();
            assert!(ActorBundle::local("actors.car", root).is_ok());
        }
    }

    #[test]
    fn local_bundles_reject_roots_outside_the_checked_in_v18_table() {
        let unknown =
            Cid::from_str("bafy2bzaceb6j6666h36xnhksu3ww4kxb6e25niayfgkdnifaqi6m6ooc66i6i")
                .unwrap();

        assert!(matches!(
            ActorBundle::local("actors.car", unknown),
            Err(ConfigError::UnrecognizedLocalBundleRoot(root)) if root == unknown
        ));
    }
}
