// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Streams retained FWSS per-piece metadata within recovered historical piece bounds.
//!
//! Export: `fwss_metadata_inventory ARCHIVE OUTPUT_DIR --export-slots-only`.
//! Discover: `fwss_metadata_inventory --intersect-slots SLOT_DIR OUTPUT_DIR`.
//! Occupied-slot exports must remain immutable while discovery reads them.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use alloy_primitives::{Keccak256, keccak256};
use anyhow::{Context, Result, anyhow, bail};
use fvm_anvil_state::{
    ForestCar,
    evm::EvmState,
    prepared::{PreparedActor, load_prepared_metadata},
    verify::validate_prepared_archive,
};
use fvm_ipld_kamt::{AsHashedKey, Config as KamtConfig, HashedKey, Kamt};
use memmap2::{Mmap, MmapOptions};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[path = "fwss_metadata_inventory/intersection.rs"]
mod intersection;

const FWSS: &str = "0x8408502033c418e1bbc97ce9ac48e5528f371a9f";
const PDP: &str = "0xbadd0b92c1c71d02e7d520f64c0876538fa2557f";
const DATA_SET_PIECE_METADATA: u64 = 13;
const DATA_SET_PIECE_METADATA_KEYS: u64 = 14;
const PDP_NEXT_DATA_SET_ID: u64 = 1;
const KAMT_RELOAD_ENTRIES: usize = 100_000;
const KAMT_RELOAD_READS: usize = 100_000;
// Resource guards, not current FWSS admission limits: historical records can exceed today's limits.
const MAX_KEYS_PER_PIECE: u64 = 1024;
const MAX_KEY_BYTES: u64 = 1 << 20;
const MAX_VALUE_BYTES: u64 = 1 << 20;

const DEFAULT_MAX_CANDIDATE_PIECE_BOUND: u64 = 10_000_000;
const EVM_KAMT_CONFIG: KamtConfig =
    KamtConfig { min_data_depth: 0, bit_width: 5, max_array_width: 1 };

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Word([u8; 32]);

impl Serialize for Word {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let first = self.0.iter().position(|byte| *byte != 0).unwrap_or(32);
        serializer.serialize_bytes(&self.0[first..])
    }
}

impl<'de> Deserialize<'de> for Word {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Word;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an EVM word encoded in at most 32 big-endian bytes")
            }
            fn visit_bytes<E: serde::de::Error>(
                self,
                bytes: &[u8],
            ) -> std::result::Result<Word, E> {
                if bytes.len() > 32 {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                let mut word = [0; 32];
                word[32 - bytes.len()..].copy_from_slice(bytes);
                Ok(Word(word))
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

struct StateHash;
impl AsHashedKey<Word, 32> for StateHash {
    fn as_hashed_key(key: &Word) -> Cow<'_, HashedKey<32>> {
        Cow::Owned(key.0)
    }
}

type StateKamt<'a> = Kamt<&'a ForestCar, Word, Word, StateHash>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyValue {
    key_hex: String,
    value_hex: String,
}

#[derive(Serialize)]
struct StorageWord {
    slot: String,
    value: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PieceRow {
    data_set_id: u64,
    piece_id: u64,
    keys: Vec<KeyValue>,
    slots: Vec<StorageWord>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct DatasetSummary {
    data_set_id: u64,
    recovered_piece_bound_exclusive: u64,
    extra_pieces: u64,
    recovered_pieces_with_metadata: u64,
    recovered_keys: u64,
    recovered_nonzero_words: u64,
    added_candidate_pieces: u64,
    added_pieces_with_metadata: u64,
    added_keys: u64,
    added_nonzero_words: u64,
    added_range_hits_file: Option<String>,
    piece_scan_start_inclusive: u64,
    piece_scan_bound_exclusive: u64,
    pieces_with_metadata: u64,
    keys: u64,
    payload_bytes: u64,
    nonzero_words: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    archive: String,
    metadata_root: String,
    source_parent_state_root: String,
    source_epoch: i64,
    fwss: String,
    pdp: String,
    pdp_next_data_set_id: u64,
    datasets: Vec<DatasetSummary>,
    datasets_scanned: u64,
    recovered_pieces_scanned: u64,
    recovered_pieces_with_metadata: u64,
    recovered_keys: u64,
    recovered_nonzero_words: u64,
    extra_pieces_per_dataset: u64,
    added_candidate_pieces: u64,
    added_pieces_with_metadata: u64,
    added_keys: u64,
    added_nonzero_words: u64,
    added_range_hits_file: Option<String>,
    pieces_scanned: u64,
    pieces_with_metadata: u64,
    keys: u64,
    payload_bytes: u64,
    nonzero_words: u64,
    key_count_distribution: BTreeMap<u64, u64>,
    nonzero_word_count_distribution: BTreeMap<u64, u64>,
    rejected_nonce_candidates: u64,
    unresolved_nonce_candidates: u64,
    storage_words_indexed: usize,
    slot_index_bytes: usize,
    index_build_seconds: f64,
    metadata_scan_seconds: f64,
    provenance: BTreeMap<String, String>,
    coverage: BTreeMap<String, String>,
}

#[derive(Default)]
struct NonceBounds {
    bounds: BTreeMap<u64, u64>,
    rejected: u64,
    unresolved: u64,
}

struct Reader<'a> {
    store: &'a ForestCar,
    root: cid::Cid,
    kamt: StateKamt<'a>,
    reads: usize,
}

impl<'a> Reader<'a> {
    fn new(store: &'a ForestCar, root: cid::Cid) -> Result<Self> {
        let kamt = Kamt::load_with_config(&root, store, EVM_KAMT_CONFIG.clone())
            .context("loading EVM storage KAMT")?;
        Ok(Self { store, root, kamt, reads: 0 })
    }

    fn get(&mut self, slot: [u8; 32]) -> Result<[u8; 32]> {
        if self.reads == KAMT_RELOAD_READS {
            self.kamt = Kamt::load_with_config(&self.root, self.store, EVM_KAMT_CONFIG.clone())
                .context("reloading EVM storage KAMT to bound node cache")?;
            self.reads = 0;
        }
        self.reads += 1;
        Ok(self
            .kamt
            .get(&Word(slot))
            .context("reading EVM storage word")?
            .copied()
            .unwrap_or_default()
            .0)
    }
}

struct SlotIndex {
    words: Mmap,
    file: Option<tempfile::NamedTempFile>,
}

impl SlotIndex {
    fn len(&self) -> usize {
        self.words.len() / 64
    }

    fn publish(self, output: &Path) -> Result<()> {
        let Self { words, file } = self;
        drop(words);
        file.context("only a newly exported index can be published")?
            .persist_noclobber(output)
            .with_context(|| format!("publishing {}", output.display()))?;
        Ok(())
    }

    fn get(&self, slot: [u8; 32]) -> [u8; 32] {
        let mut first = 0;
        let mut end = self.len();
        while first < end {
            let middle = first + (end - first) / 2;
            let offset = middle * 64;
            match self.words[offset..offset + 32].cmp(slot.as_slice()) {
                Ordering::Less => first = middle + 1,
                Ordering::Greater => end = middle,
                Ordering::Equal => {
                    return self.words[offset + 32..offset + 64]
                        .try_into()
                        .expect("fixed-width slot index value");
                }
            }
        }
        [0; 32]
    }
}

fn main() -> Result<()> {
    let mut args = env::args_os();
    let program = args.next().unwrap_or_default();
    let first = args.next().context("expected ARCHIVE or --intersect-slots")?;
    if first == "--intersect-slots" {
        let input = args.next().context("--intersect-slots requires SLOT_DIR OUTPUT_DIR")?;
        let output = args.next().context("--intersect-slots requires OUTPUT_DIR")?;
        let mut extra_pieces = 1000u64;
        let mut historical_bounds = None;
        while let Some(argument) = args.next() {
            if argument == "--extra-pieces" {
                extra_pieces = args
                    .next()
                    .context("--extra-pieces requires a value")?
                    .to_string_lossy()
                    .parse::<u64>()
                    .context("--extra-pieces must be an unsigned integer")?;
            } else if argument == "--piece-bounds" {
                historical_bounds =
                    Some(PathBuf::from(args.next().context("--piece-bounds requires a JSON map")?));
            } else {
                bail!("unknown intersection argument {argument:?}");
            }
        }
        return intersection::run(
            Path::new(&input),
            Path::new(&output),
            extra_pieces,
            historical_bounds.as_deref(),
        );
    }
    let archive = PathBuf::from(first);
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("usage: {} ARCHIVE OUTPUT_DIR", Path::new(&program).display()))?;
    let mut max_piece_bound = DEFAULT_MAX_CANDIDATE_PIECE_BOUND;
    let mut export_slots_only = false;
    let mut historical_bounds = None;
    while let Some(argument) = args.next() {
        if argument == "--export-slots-only" {
            export_slots_only = true;
        } else if argument == "--piece-bounds" {
            historical_bounds =
                Some(PathBuf::from(args.next().context("--piece-bounds requires a JSON map")?));
        } else if argument == "--max-piece-bound" {
            max_piece_bound = args
                .next()
                .context("--max-piece-bound requires a value")?
                .to_string_lossy()
                .parse::<u64>()
                .context("--max-piece-bound must be an unsigned integer")?;
        } else {
            bail!("unknown argument {argument:?}");
        }
    }
    if max_piece_bound == 0 {
        bail!("--max-piece-bound must be positive");
    }
    fs::create_dir_all(&output).with_context(|| format!("creating {}", output.display()))?;
    for name in [
        "occupied-slots.bin",
        "occupied-slots.json",
        "queues.jsonl",
        "pieces.jsonl",
        "summary.json",
        "unresolved_nonce_candidates.jsonl",
    ] {
        if output.join(name).exists() {
            bail!("refusing to replace existing output {}", output.join(name).display());
        }
    }

    eprintln!("opening {}", archive.display());
    let store =
        ForestCar::open(&archive).with_context(|| format!("opening {}", archive.display()))?;
    let (metadata_root, metadata) = load_prepared_metadata(&store, store.roots())?
        .ok_or_else(|| anyhow!("{} is not a Press prepared archive", archive.display()))?;
    validate_prepared_archive(&store, store.roots())
        .context("validating prepared actor identities and state roots")?;
    let fwss_actor = prepared_actor(&metadata.actors, FWSS)?;
    let pdp_actor = prepared_actor(&metadata.actors, PDP)?;
    let fwss_state = EvmState::load(&store, &fwss_actor.state).context("loading FWSS EVM state")?;
    let pdp_state = EvmState::load(&store, &pdp_actor.state).context("loading PDP EVM state")?;
    let mut pdp = Reader::new(&store, pdp_state.contract_state())?;
    let next_dataset = word_u64(pdp.get(slot(PDP_NEXT_DATA_SET_ID))?)?;
    if next_dataset == 0 {
        bail!("PDP nextDataSetId is zero; expected initialized counter at slot 1");
    }
    eprintln!("PDP historical dataset bound: [1, {next_dataset})");
    usize::try_from(next_dataset).context("dataset domain does not fit host indexing")?;
    let mut pdp_bounds = vec![0];
    for data_set_id in 1..next_dataset {
        let legacy = word_u64(pdp.get(mapping_slot_for_word(slot(data_set_id), slot(5)))?)?;
        let compact = word_u64(pdp.get(mapping_slot_for_word(slot(data_set_id), slot(19)))?)?;
        pdp_bounds.push(legacy.max(compact));
    }
    drop(pdp);
    let explicit_bounds =
        load_piece_bounds(historical_bounds.as_deref(), next_dataset, max_piece_bound)?;

    eprintln!("indexing FWSS KAMT and recovering persisted client-nonce piece bounds");
    let index_started = Instant::now();
    let (nonce_bounds, fwss) = nonce_piece_bounds(
        &store,
        fwss_state.contract_state(),
        next_dataset,
        max_piece_bound,
        &output.join("unresolved_nonce_candidates.jsonl"),
    )?;
    let index_build_seconds = index_started.elapsed().as_secs_f64();
    drop(store);
    eprintln!(
        "indexed {} storage words ({:.1} MiB) in {:.2}s",
        fwss.len(),
        fwss.words.len() as f64 / (1024.0 * 1024.0),
        index_build_seconds
    );
    eprintln!("found {} datasets with recovered nonzero piece bounds", nonce_bounds.bounds.len());
    write_queues(&fwss, next_dataset, &output)?;
    let export_path = output.join("occupied-slots.bin");
    let export_manifest = serde_json::json!({
        "version": 1,
        "format": "fixed-width slot[32] || value[32], no header, ascending unsigned big-endian slot",
        "recordBytes": 64,
        "records": fwss.len(),
        "bytes": fwss.words.len(),
        "scope": "Every entry in the FWSS proxy storage KAMT; no metadata filtering",
        "address": FWSS,
        "actorId": fwss_actor.id,
        "actorState": fwss_actor.state.to_string(),
        "storageRoot": fwss_state.contract_state().to_string(),
        "archiveMetadataRoot": metadata_root.to_string(),
        "sourceActorState": fwss_actor.source_state.to_string(),
        "archive": archive.display().to_string(),
        "preparedStateRoot": metadata.state_root.to_string(),
        "sourceStateRoot": metadata.source_parent_state_root.to_string(),
        "sourceEpoch": metadata.epoch,
        "nextDataSetId": next_dataset,
        "maxPieceBound": max_piece_bound,
        "currentPdpPieceBounds": &pdp_bounds,
        "noncePieceBoundCandidates": &nonce_bounds.bounds,
        "historicalPieceBounds": &explicit_bounds,
        "historicalPieceBoundsFile": historical_bounds.as_ref().map(|path| path.display().to_string()),
        "queueLayout": "scheduledPieceMetadataRemovals mapping slot 22; dataSetInfo mapping slot 7, pdpRailId struct offset 0",
        "queueDataSetCount": next_dataset - 1,
        "coverage": "All occupied slots and all historical dataset queues are exported. Mapping preimages are unknown: metadata coverage is bounded by current PDP counters, nonce-shaped candidates and optional explicit historical bounds; orphan values absent from key arrays cannot be discovered by this inventory.",
        "rejectedNonceCandidates": nonce_bounds.rejected,
        "unresolvedNonceCandidates": nonce_bounds.unresolved,
        "exportSeconds": index_build_seconds
    });
    if export_slots_only {
        fwss.publish(&export_path)?;
        serde_json::to_writer_pretty(
            BufWriter::new(File::create(output.join("occupied-slots.json"))?),
            &export_manifest,
        )?;
        println!("Export: {}", export_path.display());
        println!("Records: {}", export_manifest["records"]);
        println!("Bytes: {}", export_manifest["bytes"]);
        return Ok(());
    }

    let pieces_path = output.join("pieces.jsonl");
    let mut rows = BufWriter::new(
        File::create(&pieces_path)
            .with_context(|| format!("creating {}", pieces_path.display()))?,
    );
    let mut summary = Summary {
        archive: archive.display().to_string(),
        metadata_root: metadata_root.to_string(),
        source_parent_state_root: metadata.source_parent_state_root.to_string(),
        source_epoch: metadata.epoch,
        fwss: FWSS.into(),
        pdp: PDP.into(),
        pdp_next_data_set_id: next_dataset,
        datasets: Vec::new(),
        datasets_scanned: next_dataset - 1,
        recovered_pieces_with_metadata: 0,
        recovered_keys: 0,
        recovered_nonzero_words: 0,
        recovered_pieces_scanned: 0,
        extra_pieces_per_dataset: 0,
        added_candidate_pieces: 0,
        added_pieces_with_metadata: 0,
        added_keys: 0,
        added_nonzero_words: 0,
        added_range_hits_file: None,
        pieces_scanned: 0,
        pieces_with_metadata: 0,
        keys: 0,
        payload_bytes: 0,
        nonzero_words: 0,
        key_count_distribution: BTreeMap::new(),
        nonzero_word_count_distribution: BTreeMap::new(),
        rejected_nonce_candidates: nonce_bounds.rejected,
        unresolved_nonce_candidates: nonce_bounds.unresolved,
        storage_words_indexed: fwss.len(),
        slot_index_bytes: fwss.words.len(),
        index_build_seconds,
        metadata_scan_seconds: 0.0,
        provenance: BTreeMap::from([
            ("archiveMetadataRoot".into(), metadata_root.to_string()),
            ("preparedSourceParentStateRoot".into(), metadata.source_parent_state_root.to_string()),
            ("fwssStorageLayout".into(), "FilecoinWarmStorageService deployed layout: dataSetPieceMetadata slot 13; dataSetPieceMetadataKeys slot 14".into()),
            ("pdpStorageLayout".into(), "PDPVerifier deployed layout: nextDataSetId slot 1; legacy nextPieceId slot 5; compactPieces slot 19".into()),
            ("maxPieceBound".into(), max_piece_bound.to_string()),
            ("historicalPieceBounds".into(), serde_json::to_string(&explicit_bounds)?),
            ("queueLayout".into(), "FWSSStorage / FilecoinWarmStorageServiceStateInternalLibrary: scheduledPieceMetadataRemovals slot 22; dataSetInfo slot 7; pdpRailId offset 0".into()),
        ]),
        coverage: BTreeMap::from([
            ("datasetBounds".into(), "All historical PDP IDs [1, nextDataSetId) are examined; deleted datasets are not filtered by current PDP membership.".into()),
            ("pieceBounds".into(), "Piece scan bounds combine current PDP legacy counters and compact array lengths with persisted FWSS clientNonces-shaped values. Packed piecesAdded nonces store the exclusive piece high-water mark in the upper 128 bits and dataset ID in the lower 128 bits. Shape matches are conservative candidates; every candidate piece is checked by its exact metadata key-array slot.".into()),
            ("metadata".into(), "Every nonempty retained per-piece metadata key array within recovered bounds is emitted, including metadata for pieces no longer live in PDP. Slots include every nonzero word owned by the key array and its string-keyed mapping values.".into()),
            ("orphanPreimages".into(), "Unknown Solidity mapping preimages cannot be inverted from occupied slots. Value mappings whose keys are missing from retained key arrays are not enumerated; explicit historical piece bounds extend the scan but do not reconstruct missing string keys.".into()),
            ("queues".into(), "queues.jsonl includes every historical dataset ID, including deleted and zero-length queues; live means dataSetInfo[id].pdpRailId != 0.".into()),
            ("restriction".into(), "State does not identify the preimages of Solidity mapping slots. Therefore current PDP counters plus clientNonces recovery do not prove that no metadata lies above recovered bounds: PDP cleanup shrinks counters and arrays, and historical nonce encodings may lack high-water marks. Datasets with neither source have a zero scan range. Candidates above the explicit probe cap are recorded as unresolved. Counts are exhaustive within recovered bounds, not an unqualified historical-total claim.".into()),
        ]),
    };

    let scan_started = Instant::now();
    for data_set_id in 1..next_dataset {
        let bound = nonce_bounds
            .bounds
            .get(&data_set_id)
            .copied()
            .unwrap_or(0)
            .max(pdp_bounds[data_set_id as usize])
            .max(explicit_bounds.get(&data_set_id).copied().unwrap_or(0));
        if bound > max_piece_bound {
            bail!(
                "dataset {data_set_id} current PDP piece bound {bound} exceeds --max-piece-bound {max_piece_bound}"
            );
        }
        let mut dataset = DatasetSummary {
            data_set_id,
            recovered_piece_bound_exclusive: bound,
            piece_scan_start_inclusive: 0,
            piece_scan_bound_exclusive: bound,
            ..Default::default()
        };
        let metadata_base = nested_mapping_base(data_set_id, DATA_SET_PIECE_METADATA);
        let keys_base = nested_mapping_base(data_set_id, DATA_SET_PIECE_METADATA_KEYS);
        if bound != 0 {
            eprintln!("dataset {data_set_id}: scanning pieces [0, {bound})");
        }
        for piece_id in 0..bound {
            summary.recovered_pieces_scanned += 1;
            summary.pieces_scanned += 1;
            let array_header = fwss.get(mapping_slot_for_word(slot(piece_id), keys_base));
            if let Some((row, key_count, payload_bytes, words)) =
                piece_row(&fwss, data_set_id, piece_id, metadata_base, keys_base, array_header)?
            {
                serde_json::to_writer(&mut rows, &row)?;
                rows.write_all(b"\n")?;
                dataset.recovered_pieces_with_metadata += 1;
                dataset.recovered_keys += key_count;
                dataset.recovered_nonzero_words += words;
                dataset.pieces_with_metadata += 1;
                dataset.keys += key_count;
                dataset.payload_bytes += payload_bytes;
                dataset.nonzero_words += words;
                summary.recovered_pieces_with_metadata += 1;
                summary.recovered_keys += key_count;
                summary.recovered_nonzero_words += words;
                summary.pieces_with_metadata += 1;
                summary.keys += key_count;
                summary.payload_bytes += payload_bytes;
                summary.nonzero_words += words;
                *summary.key_count_distribution.entry(key_count).or_default() += 1;
                *summary.nonzero_word_count_distribution.entry(words).or_default() += 1;
            }
        }
        summary.datasets.push(dataset);
    }
    summary.metadata_scan_seconds = scan_started.elapsed().as_secs_f64();
    rows.flush()?;
    let summary_path = output.join("summary.json");
    serde_json::to_writer_pretty(BufWriter::new(File::create(&summary_path)?), &summary)?;
    eprintln!("wrote {} and {}", pieces_path.display(), summary_path.display());
    fwss.publish(&export_path)?;
    serde_json::to_writer_pretty(
        BufWriter::new(File::create(output.join("occupied-slots.json"))?),
        &export_manifest,
    )?;
    Ok(())
}

fn prepared_actor<'a>(actors: &'a [PreparedActor], wanted: &str) -> Result<&'a PreparedActor> {
    let actor = actors
        .iter()
        .find(|actor| {
            actor.requested_addresses.iter().any(|address| address.eq_ignore_ascii_case(wanted))
        })
        .ok_or_else(|| anyhow!("prepared archive does not retain requested actor {wanted}"))?;
    let address = fvm_shared::address::Address::new_delegated(10, &hex::decode(&wanted[2..])?)?;
    if actor.delegated_address != Some(address) {
        bail!("prepared actor f0{} does not have FWSS/PDP delegated identity {wanted}", actor.id);
    }
    if actor.patch.is_some() || actor.source_state != actor.state {
        bail!("inventory requires unpatched source actor {wanted}");
    }
    Ok(actor)
}

fn load_piece_bounds(
    path: Option<&Path>,
    next_dataset: u64,
    maximum: u64,
) -> Result<BTreeMap<u64, u64>> {
    let Some(path) = path else { return Ok(BTreeMap::new()) };
    let bounds: BTreeMap<u64, u64> = serde_json::from_reader(File::open(path)?)
        .with_context(|| format!("reading exclusive historical piece bounds {}", path.display()))?;
    for (&dataset, &bound) in &bounds {
        if dataset == 0 || dataset >= next_dataset || bound > maximum {
            bail!(
                "historical dataset {dataset} bound {bound} is outside the dataset domain or --max-piece-bound {maximum}"
            );
        }
    }
    Ok(bounds)
}

fn write_queues(index: &SlotIndex, next_dataset: u64, output: &Path) -> Result<()> {
    let mut writer = BufWriter::new(File::create_new(output.join("queues.jsonl"))?);
    for dataset in 1..next_dataset {
        let length_slot = nested_mapping_base(dataset, 22);
        let length = word_u64(index.get(length_slot))
            .with_context(|| format!("dataset {dataset} removal queue length"))?;
        let live = index.get(nested_mapping_base(dataset, 7)) != [0; 32];
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "dataSetId": dataset, "length": length, "live": live,
                "lengthSlot": hex_word(length_slot), "dataStart": hex_word(keccak_slot(length_slot))
            }),
        )?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn nonce_piece_bounds(
    store: &ForestCar,
    root: cid::Cid,
    next_dataset: u64,
    max_piece_bound: u64,
    unresolved_path: &Path,
) -> Result<(NonceBounds, SlotIndex)> {
    let mut unresolved = BufWriter::new(
        File::create(unresolved_path)
            .with_context(|| format!("creating {}", unresolved_path.display()))?,
    );
    let mut result = NonceBounds::default();
    let index_file = tempfile::NamedTempFile::new_in(
        unresolved_path.parent().context("slot index output has no parent directory")?,
    )
    .context("creating private disk-backed slot index")?;
    let mut words = BufWriter::new(index_file.as_file());
    let mut word_count = 0usize;
    let started = Instant::now();
    let mut cursor = None::<Word>;
    loop {
        let kamt: StateKamt<'_> = Kamt::load_with_config(&root, store, EVM_KAMT_CONFIG.clone())
            .context("loading FWSS KAMT for bounded scan")?;
        let mut consumed = 0;
        let mut exhausted = true;
        let iterator: Box<dyn Iterator<Item = Result<(&Word, &Word), fvm_ipld_kamt::Error>>> =
            match &cursor {
                Some(last) => {
                    Box::new(kamt.iter_from(last).context("resuming FWSS KAMT scan")?.skip(1))
                }
                None => Box::new(kamt.iter()),
            };
        for entry in iterator {
            let (key, value) = entry.context("iterating FWSS KAMT")?;
            if consumed == KAMT_RELOAD_ENTRIES {
                exhausted = false;
                break;
            }
            if cursor.is_some_and(|previous| previous >= *key) {
                bail!("FWSS KAMT iteration is not strictly increasing in raw slot order");
            }
            cursor = Some(*key);
            words.write_all(&key.0)?;
            words.write_all(&value.0)?;
            word_count += 1;
            consumed += 1;
            let data_set_id =
                u128::from_be_bytes(value.0[16..].try_into().expect("16-byte suffix"));
            let piece_bound =
                u128::from_be_bytes(value.0[..16].try_into().expect("16-byte prefix"));
            if data_set_id != 0 && data_set_id < u128::from(next_dataset) && piece_bound != 0 {
                if piece_bound > u128::from(max_piece_bound) {
                    writeln!(
                        unresolved,
                        "{{\"dataSetId\":{data_set_id},\"candidatePieceBound\":\"{piece_bound}\"}}"
                    )?;
                    result.unresolved += 1;
                    continue;
                }
                let bound =
                    u64::try_from(piece_bound).context("candidate bound does not fit u64")?;
                let data_set_id = u64::try_from(data_set_id).expect("bounded by u64 nextDataSetId");
                result
                    .bounds
                    .entry(data_set_id)
                    .and_modify(|old| *old = (*old).max(bound))
                    .or_insert(bound);
            }
        }
        drop(kamt);
        eprintln!("indexed {word_count} storage words in {:.1}s", started.elapsed().as_secs_f64());
        if exhausted {
            break;
        }
    }
    unresolved.flush()?;
    words.flush()?;
    drop(words);
    if word_count == 0 {
        bail!("FWSS storage is empty");
    }
    // SAFETY: This private temporary file is owned exclusively here, with no remaining writer.
    // It is not published until all reads finish and the immutable mapping has been dropped.
    let words = unsafe { MmapOptions::new().map(index_file.as_file()) }
        .context("mapping immutable slot index")?;
    Ok((result, SlotIndex { words, file: Some(index_file) }))
}

fn piece_row(
    reader: &SlotIndex,
    data_set_id: u64,
    piece_id: u64,
    metadata_base: [u8; 32],
    keys_base: [u8; 32],
    array_header: [u8; 32],
) -> Result<Option<(PieceRow, u64, u64, u64)>> {
    let array_slot = mapping_slot_for_word(slot(piece_id), keys_base);
    let array_length = word_u64(array_header)?;
    if array_length == 0 {
        return Ok(None);
    }
    if array_length > MAX_KEYS_PER_PIECE {
        bail!(
            "dataset {data_set_id} piece {piece_id} has {array_length} metadata keys, exceeding inventory resource guard {MAX_KEYS_PER_PIECE}"
        );
    }
    let mut slots = Vec::new();
    let mut seen = BTreeSet::new();
    record(array_header, array_slot, &mut slots, &mut seen);
    let base = keccak_slot(array_slot);
    let mut keys = Vec::with_capacity(array_length as usize);
    let mut payload_bytes = 0;
    let piece_metadata_base = mapping_slot_for_word(slot(piece_id), metadata_base);
    for index in 0..array_length {
        let key_slot = add_slot(base, index)?;
        let key = read_string(reader, key_slot, MAX_KEY_BYTES, &mut slots, &mut seen)?;
        payload_bytes += u64::try_from(key.len()).expect("string length bounded");
        let value_slot = mapping_bytes_slot(&key, piece_metadata_base);
        let value = read_string(reader, value_slot, MAX_VALUE_BYTES, &mut slots, &mut seen)?;
        payload_bytes += u64::try_from(value.len()).expect("string length bounded");
        keys.push(KeyValue { key_hex: hex_bytes(&key), value_hex: hex_bytes(&value) });
    }
    let words = u64::try_from(slots.len()).expect("slot plan length fits u64");
    Ok(Some((PieceRow { data_set_id, piece_id, keys, slots }, array_length, payload_bytes, words)))
}

fn read_string(
    reader: &SlotIndex,
    header_slot: [u8; 32],
    maximum: u64,
    slots: &mut Vec<StorageWord>,
    seen: &mut BTreeSet<[u8; 32]>,
) -> Result<Vec<u8>> {
    let header = reader.get(header_slot);
    record(header, header_slot, slots, seen);
    if header[31] & 1 == 0 {
        let length = u64::from(header[31] >> 1);
        if length > 31 {
            bail!("invalid short Solidity string length {length}");
        }
        if length > maximum {
            bail!("short Solidity string length {length} exceeds maximum {maximum}");
        }
        return Ok(header[..length as usize].to_vec());
    }
    if header[..24].iter().any(|byte| *byte != 0) {
        bail!("long Solidity string length does not fit u64");
    }
    let length_word = u128::from(word_u64(header)? >> 1);
    if length_word < 32 {
        bail!("invalid long Solidity string length {length_word}");
    }
    if length_word > u128::from(maximum) {
        bail!("long Solidity string length {length_word} exceeds maximum {maximum}");
    }
    let length = usize::try_from(length_word).context("string length does not fit usize")?;
    let words = length.div_ceil(32);
    let base = keccak_slot(header_slot);
    let mut value = Vec::with_capacity(length);
    for index in 0..words {
        let payload_slot = add_slot(base, u64::try_from(index).expect("payload words fit u64"))?;
        let payload = reader.get(payload_slot);
        record(payload, payload_slot, slots, seen);
        let remaining = length - value.len();
        value.extend_from_slice(&payload[..remaining.min(32)]);
    }
    Ok(value)
}

fn record(
    value: [u8; 32],
    slot: [u8; 32],
    slots: &mut Vec<StorageWord>,
    seen: &mut BTreeSet<[u8; 32]>,
) {
    if value != [0; 32] && seen.insert(slot) {
        slots.push(StorageWord { slot: hex_word(slot), value: hex_word(value) });
    }
}

fn slot(value: u64) -> [u8; 32] {
    let mut slot = [0; 32];
    slot[24..].copy_from_slice(&value.to_be_bytes());
    slot
}
fn word_u64(word: [u8; 32]) -> Result<u64> {
    if word[..24].iter().any(|byte| *byte != 0) {
        bail!("state-derived unsigned integer does not fit u64: {}", hex_word(word));
    }
    Ok(u64::from_be_bytes(word[24..].try_into().expect("8-byte suffix")))
}
fn keccak_slot(slot: [u8; 32]) -> [u8; 32] {
    keccak256(slot).0
}
fn nested_mapping_base(data_set_id: u64, mapping_slot: u64) -> [u8; 32] {
    mapping_slot_for_word(slot(data_set_id), slot(mapping_slot))
}
fn mapping_slot_for_word(key: [u8; 32], base: [u8; 32]) -> [u8; 32] {
    let mut input = [0; 64];
    input[..32].copy_from_slice(&key);
    input[32..].copy_from_slice(&base);
    keccak256(input).0
}
fn mapping_bytes_slot(key: &[u8], base: [u8; 32]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(key);
    hasher.update(base);
    hasher.finalize().0
}
fn add_slot(mut slot: [u8; 32], offset: u64) -> Result<[u8; 32]> {
    let mut carry = offset;
    for byte in slot.iter_mut().rev() {
        let (sum, overflow) = byte.overflowing_add(carry as u8);
        *byte = sum;
        carry = (carry >> 8) + u64::from(overflow);
    }
    if carry != 0 {
        bail!("Solidity storage slot addition overflow");
    }
    Ok(slot)
}
fn hex_word(word: [u8; 32]) -> String {
    format!("0x{}", hex::encode(word))
}
fn hex_bytes(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}
