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

/// Selects the builtin-actors v18 CAR used to create genesis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActorBundle {
    /// Downloads the pinned official mainnet v18 CAR and verifies its root.
    Official,
    /// Reads a checked-in, known v18 release CAR from disk and verifies its inferred root.
    Local { path: PathBuf },
}

impl ActorBundle {
    pub fn local(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let bundle = Self::Local { path: path.into() };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn actor_version(&self) -> u32 {
        ACTOR_BUNDLE_VERSION
    }

    pub(crate) fn expected_root(&self) -> Result<Option<Cid>, ConfigError> {
        match self {
            Self::Official => Cid::from_str(DEFAULT_ACTOR_BUNDLE_ROOT)
                .map(Some)
                .map_err(|error| ConfigError::InvalidOfficialBundleRoot(error.to_string())),
            Self::Local { .. } => Ok(None),
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
            Self::Local { path } => Some(path),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Official => {
                self.expected_root()?;
                self.source_url()?;
            }
            Self::Local { path } if path.as_os_str().is_empty() => {
                return Err(ConfigError::EmptyBundlePath);
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
    use super::{ACTOR_BUNDLE_VERSION, ActorBundle, DEFAULT_CHAIN_ID, NETWORK_VERSION, NodeConfig};
    use crate::error::ConfigError;
    use fvm_shared::version::NetworkVersion;

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
    fn local_bundle_requires_only_a_nonempty_path() {
        assert_eq!(
            ActorBundle::local("actors.car").unwrap(),
            ActorBundle::Local { path: "actors.car".into() }
        );
        assert!(matches!(ActorBundle::local(""), Err(ConfigError::EmptyBundlePath)));
    }
}
