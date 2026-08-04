// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fvm_anvil_state::state::{ActorSelector, parse_actor_selector};
use serde::{Deserialize, Deserializer, de::IgnoredAny};

#[derive(Debug, Deserialize)]
struct RecipeEnvelope {
    version: u64,
}

#[derive(Debug, Default)]
struct PatchField(bool);

impl<'de> Deserialize<'de> for PatchField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IgnoredAny::deserialize(deserializer)?;
        Ok(Self(true))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    #[serde(rename = "version")]
    _version: u64,
    actors: Vec<ActorRecipe>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorRecipe {
    address: String,
    #[serde(default)]
    code: PatchField,
    #[serde(default)]
    storage: PatchField,
    #[serde(default)]
    balance: PatchField,
    #[serde(default)]
    sequence: PatchField,
}

impl Recipe {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("failed to read recipe {}", path.display()))?;
        Self::decode(&bytes).with_context(|| format!("failed to decode recipe {}", path.display()))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let envelope: RecipeEnvelope = serde_json::from_slice(bytes)?;
        if envelope.version != 1 {
            bail!("unsupported Press recipe version {}", envelope.version);
        }
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn selectors(&self) -> Result<Vec<ActorSelector>> {
        self.actors
            .iter()
            .map(|actor| {
                if actor.code.0 || actor.storage.0 || actor.balance.0 || actor.sequence.0 {
                    bail!(
                        "recipe actor {} requests a state patch; patches are not supported by the standalone extraction stage",
                        actor.address
                    );
                }
                parse_actor_selector(actor.address.clone()).map_err(Into::into)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actor_only_recipe() {
        let recipe: Recipe =
            serde_json::from_str(r#"{"version":1,"actors":[{"address":"f0100"}]}"#).unwrap();
        let selectors = recipe.selectors().unwrap();
        assert_eq!(selectors[0].input, "f0100");
    }

    #[test]
    fn rejects_patches_until_the_patch_stage() {
        let recipe: Recipe = serde_json::from_str(
            r#"{"version":1,"actors":[{"address":"f0100","code":{"replace":"0x00"}}]}"#,
        )
        .unwrap();
        assert!(recipe.selectors().unwrap_err().to_string().contains("not supported"));
    }

    #[test]
    fn rejects_explicitly_null_patch_fields() {
        let recipe: Recipe =
            serde_json::from_str(r#"{"version":1,"actors":[{"address":"f0100","code":null}]}"#)
                .unwrap();
        assert!(recipe.selectors().unwrap_err().to_string().contains("not supported"));
    }

    #[test]
    fn reports_unsupported_version_before_decoding_its_body() {
        let error = Recipe::decode(br#"{"version":2}"#).unwrap_err();
        assert_eq!(error.to_string(), "unsupported Press recipe version 2");
    }
}
