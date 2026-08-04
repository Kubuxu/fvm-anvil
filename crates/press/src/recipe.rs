// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use fvm_anvil_state::state::{ActorPatch, parse_actor_selector};
use fvm_shared::bigint::{BigInt, Sign};
use fvm_shared::econ::TokenAmount;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Deserialize)]
struct RecipeEnvelope {
    version: u64,
}

#[derive(Debug)]
struct PatchField<T>(Option<T>);

impl<T> Default for PatchField<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchField<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(|value| Self(Some(value)))
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
    code: PatchField<CodePatchRecipe>,
    #[serde(default)]
    storage: PatchField<BTreeMap<String, String>>,
    #[serde(default)]
    balance: PatchField<String>,
    #[serde(default)]
    sequence: PatchField<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodePatchRecipe {
    replace: String,
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

    pub fn patches(&self) -> Result<Vec<ActorPatch>> {
        let mut patches = Vec::with_capacity(self.actors.len());
        for actor in &self.actors {
            let selector = parse_actor_selector(actor.address.clone())?;
            let runtime_code = actor
                .code
                .0
                .as_ref()
                .map(|code| parse_hex_bytes(&code.replace, "replacement bytecode"))
                .transpose()?;
            let mut storage = BTreeMap::new();
            if let Some(entries) = &actor.storage.0 {
                for (key, value) in entries {
                    let key = parse_word(key, "storage key")?;
                    let value = parse_word(value, "storage value")?;
                    if storage.insert(key, value).is_some() {
                        bail!("recipe actor {} contains a duplicate storage key", actor.address);
                    }
                }
            }
            let balance = actor.balance.0.as_ref().map(|value| parse_balance(value)).transpose()?;
            patches.push(ActorPatch {
                selector,
                runtime_code,
                storage,
                balance,
                sequence: actor.sequence.0,
            });
        }
        Ok(patches)
    }
}

fn parse_hex_bytes(input: &str, field: &str) -> Result<Vec<u8>> {
    let encoded = input.strip_prefix("0x").ok_or_else(|| anyhow!("{field} must start with 0x"))?;
    hex::decode(encoded).with_context(|| format!("invalid {field}"))
}

fn parse_word(input: &str, field: &str) -> Result<[u8; 32]> {
    let bytes = parse_hex_bytes(input, field)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow!("{field} must contain 32 bytes, found {}", bytes.len()))
}

fn parse_balance(input: &str) -> Result<TokenAmount> {
    let atto = BigInt::from_str(input).context("balance must be a decimal attoFIL integer")?;
    if atto.sign() == Sign::Minus {
        bail!("balance must not be negative");
    }
    Ok(TokenAmount::from_atto(atto))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actor_only_recipe() {
        let recipe: Recipe =
            serde_json::from_str(r#"{"version":1,"actors":[{"address":"f0100"}]}"#).unwrap();
        let patches = recipe.patches().unwrap();
        assert_eq!(patches[0].selector.input, "f0100");
        assert!(patches[0].runtime_code.is_none());
    }

    #[test]
    fn parses_all_supported_patches() {
        let key = format!("0x{}", "01".repeat(32));
        let value = format!("0x{}", "02".repeat(32));
        let recipe: Recipe = serde_json::from_value(serde_json::json!({
            "version": 1,
            "actors": [{
                "address": "f0100",
                "code": { "replace": "0x6000" },
                "storage": { key: value },
                "balance": "123",
                "sequence": 7
            }]
        }))
        .unwrap();

        let patches = recipe.patches().unwrap();
        assert_eq!(patches[0].runtime_code.as_deref(), Some(&[0x60, 0x00][..]));
        assert_eq!(patches[0].storage.len(), 1);
        assert_eq!(patches[0].balance, Some(TokenAmount::from_atto(123)));
        assert_eq!(patches[0].sequence, Some(7));
    }

    #[test]
    fn rejects_explicitly_null_patch_fields() {
        let error = Recipe::decode(br#"{"version":1,"actors":[{"address":"f0100","code":null}]}"#)
            .unwrap_err();
        assert!(error.to_string().contains("invalid type: null"));
    }

    #[test]
    fn rejects_short_storage_words() {
        let recipe: Recipe = serde_json::from_str(
            r#"{"version":1,"actors":[{"address":"f0100","storage":{"0x01":"0x02"}}]}"#,
        )
        .unwrap();
        assert!(recipe.patches().unwrap_err().to_string().contains("32 bytes"));
    }

    #[test]
    fn rejects_negative_balance() {
        let recipe: Recipe =
            serde_json::from_str(r#"{"version":1,"actors":[{"address":"f0100","balance":"-1"}]}"#)
                .unwrap();
        assert_eq!(recipe.patches().unwrap_err().to_string(), "balance must not be negative");
    }

    #[test]
    fn reports_unsupported_version_before_decoding_its_body() {
        let error = Recipe::decode(br#"{"version":2}"#).unwrap_err();
        assert_eq!(error.to_string(), "unsupported Press recipe version 2");
    }
}
