// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! External-sort metadata discovery over an immutable occupied-slot export.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use memmap2::MmapOptions;
use serde_json::Value;

use super::{
    DATA_SET_PIECE_METADATA, DATA_SET_PIECE_METADATA_KEYS, DatasetSummary, FWSS,
    MAX_KEYS_PER_PIECE, PDP, SlotIndex, Summary, load_piece_bounds, mapping_slot_for_word,
    nested_mapping_base, piece_row, slot, word_u64, write_queues,
};

pub(super) fn run(
    input: &Path,
    output: &Path,
    extra_pieces: u64,
    historical_bounds: Option<&Path>,
) -> Result<()> {
    let started = Instant::now();
    let mut manifest: Value =
        serde_json::from_reader(BufReader::new(File::open(input.join("occupied-slots.json"))?))?;
    if manifest["version"] != 1 || manifest["recordBytes"] != 64 || manifest["address"] != FWSS {
        bail!("unsupported occupied-slot manifest or unexpected FWSS address");
    }
    let next_dataset = number(&manifest, "nextDataSetId")?;
    let max_piece_bound = number(&manifest, "maxPieceBound")?;
    let pdp_bounds: Vec<u64> = serde_json::from_value(manifest["currentPdpPieceBounds"].clone())?;
    let nonce_bounds: BTreeMap<u64, u64> =
        serde_json::from_value(manifest["noncePieceBoundCandidates"].clone())?;
    if next_dataset == 0 || pdp_bounds.len() as u64 != next_dataset {
        bail!("PDP bounds do not match the historical dataset domain");
    }
    let mut explicit_bounds: BTreeMap<u64, u64> = match manifest.get("historicalPieceBounds") {
        Some(bounds) => serde_json::from_value(bounds.clone())?,
        None => BTreeMap::new(),
    };
    for (&dataset, &bound) in &explicit_bounds {
        if dataset == 0 || dataset >= next_dataset || bound > max_piece_bound {
            bail!("invalid historical piece bound for dataset {dataset}: {bound}");
        }
    }
    for (dataset, bound) in load_piece_bounds(historical_bounds, next_dataset, max_piece_bound)? {
        explicit_bounds.entry(dataset).and_modify(|old| *old = (*old).max(bound)).or_insert(bound);
    }
    manifest["historicalPieceBounds"] = serde_json::to_value(&explicit_bounds)?;
    fs::create_dir_all(output)?;
    let candidates_path = output.join("candidate-metadata-key-slots.tsv");
    let hits_path = output.join("metadata-key-hits.tsv");
    let pieces_path = output.join("pieces.jsonl");
    let added_hits_path = output.join("added-range-hits.tsv");
    for path in [
        &candidates_path,
        &hits_path,
        &pieces_path,
        &added_hits_path,
        &output.join("summary.json"),
        &output.join("queues.jsonl"),
        &output.join("occupied-slots.json"),
        &output.join("occupied-slots.bin"),
    ] {
        if path.exists() {
            bail!("refusing to replace existing output {}", path.display());
        }
    }
    let occupied_path = input.join("occupied-slots.bin");
    let occupied_file = File::open(&occupied_path)?;
    let bytes = occupied_file.metadata()?.len();
    if bytes == 0
        || bytes % 64 != 0
        || bytes != number(&manifest, "bytes")?
        || bytes / 64 != number(&manifest, "records")?
    {
        bail!("occupied-slot file length does not match its manifest");
    }
    let mut summary = Summary {
        archive: occupied_path.display().to_string(),
        metadata_root: text(&manifest, "archiveMetadataRoot")?,
        source_parent_state_root: text(&manifest, "sourceStateRoot")?,
        source_epoch: manifest["sourceEpoch"].as_i64().context("invalid sourceEpoch")?,
        fwss: FWSS.into(), pdp: PDP.into(), pdp_next_data_set_id: next_dataset,
        datasets: Vec::new(), datasets_scanned: next_dataset - 1,
        recovered_pieces_scanned: 0, recovered_pieces_with_metadata: 0, recovered_keys: 0,
        recovered_nonzero_words: 0, extra_pieces_per_dataset: extra_pieces,
        added_candidate_pieces: 0, added_pieces_with_metadata: 0, added_keys: 0,
        added_nonzero_words: 0, added_range_hits_file: Some(added_hits_path.display().to_string()),
        pieces_scanned: 0, pieces_with_metadata: 0, keys: 0, payload_bytes: 0, nonzero_words: 0,
        key_count_distribution: BTreeMap::new(),
        nonzero_word_count_distribution: BTreeMap::new(),
        rejected_nonce_candidates: number(&manifest, "rejectedNonceCandidates")?,
        unresolved_nonce_candidates: number(&manifest, "unresolvedNonceCandidates")?,
        storage_words_indexed: usize::try_from(bytes / 64)?,
        slot_index_bytes: usize::try_from(bytes)?,
        index_build_seconds: 0.0,
        metadata_scan_seconds: 0.0,
        provenance: BTreeMap::from([
            ("occupiedSlotExport".into(), occupied_path.display().to_string()),
            ("archiveMetadataRoot".into(), text(&manifest, "archiveMetadataRoot")?),
            ("fwssStorageRoot".into(), text(&manifest, "storageRoot")?),
            ("discovery".into(), "Sorted merge intersection of generated root-14 key-array slots with every occupied slot; stored strings determine root-13 value slots.".into()),
        ]),
        coverage: BTreeMap::from([
            ("occupiedSlots".into(), "Every exported FWSS KAMT entry is available independently of the Press archive; no metadata filter was applied during export.".into()),
            ("datasetBounds".into(), "All historical PDP IDs [1, nextDataSetId) are included in candidate generation.".into()),
            ("pieceBounds".into(), "Per-dataset recovered exclusive bounds use the maximum of retained PDP piece bounds and exported packed-nonce high-water candidates. Each recovered range is extended by extraPieces solely for probing.".into()),
            ("metadata".into(), "Only occupied candidate key-array headers enter decoding. Every stored key is decoded, its root-13 value slot derived and read, and all nonzero header/payload words recorded.".into()),
            ("restriction".into(), "Intersection is exhaustive over each generated recovered range plus its explicit extension. It does not recover erased Solidity mapping preimages or prove historical coverage above those ranges. Unresolved bounds are not empty datasets.".into()),
            ("orphanPreimages".into(), "String-keyed mapping values whose keys are absent from retained key arrays cannot be enumerated. Occupied-slot intersection cannot invert unknown mapping preimages; explicit historical bounds only extend candidate piece coverage.".into()),
            ("queues".into(), "Every historical dataset ID is emitted in queues.jsonl, including deleted and empty queues; live is the full pdpRailId word at dataSetInfo slot 7, offset 0.".into()),
        ]),
    };

    let raw_candidates = tempfile::NamedTempFile::new_in(output)?;
    let mut candidates = BufWriter::new(raw_candidates.as_file());
    let mut encoded = [0u8; 64];
    for dataset in 1..next_dataset {
        let recovered_bound = pdp_bounds[dataset as usize]
            .max(nonce_bounds.get(&dataset).copied().unwrap_or(0))
            .max(explicit_bounds.get(&dataset).copied().unwrap_or(0));
        if recovered_bound > max_piece_bound {
            bail!(
                "dataset {dataset} recovered bound {recovered_bound} exceeds the candidate probe guard {max_piece_bound}"
            );
        }
        let extended_bound = recovered_bound.checked_add(extra_pieces).with_context(|| {
            format!(
                "dataset {dataset} recovered bound {recovered_bound} plus --extra-pieces {extra_pieces} overflows u64"
            )
        })?;
        summary.datasets.push(DatasetSummary {
            data_set_id: dataset,
            recovered_piece_bound_exclusive: recovered_bound,
            extra_pieces,
            added_candidate_pieces: extra_pieces,
            added_range_hits_file: Some(added_hits_path.display().to_string()),
            piece_scan_start_inclusive: 0,
            piece_scan_bound_exclusive: extended_bound,
            ..Default::default()
        });
        summary.recovered_pieces_scanned = summary
            .recovered_pieces_scanned
            .checked_add(recovered_bound)
            .context("recovered candidate count overflows u64")?;
        summary.added_candidate_pieces = summary
            .added_candidate_pieces
            .checked_add(extra_pieces)
            .context("added candidate count overflows u64")?;
        summary.pieces_scanned = summary
            .pieces_scanned
            .checked_add(extended_bound)
            .context("candidate count overflows u64")?;
        let base = nested_mapping_base(dataset, DATA_SET_PIECE_METADATA_KEYS);
        for piece in 0..extended_bound {
            let candidate = mapping_slot_for_word(slot(piece), base);
            hex::encode_to_slice(candidate, &mut encoded)?;
            candidates.write_all(&encoded)?;
            writeln!(candidates, "\t{dataset}\t{piece}")?;
        }
    }
    candidates.flush()?;
    drop(candidates);
    sort_file(raw_candidates.path(), &candidates_path, output, &["-k1,1"])?;
    drop(raw_candidates);
    let candidate_seconds = started.elapsed().as_secs_f64();
    eprintln!(
        "generated and sorted {} candidate key-array slots in {candidate_seconds:.2}s",
        summary.pieces_scanned
    );

    let raw_hits = tempfile::NamedTempFile::new_in(output)?;
    let mut hits = BufWriter::new(raw_hits.as_file());
    let mut occupied = BufReader::with_capacity(1024 * 1024, File::open(&occupied_path)?);
    let mut record = next_record(&mut occupied)?;
    let mut candidate_input = BufReader::new(File::open(&candidates_path)?);
    let mut line = String::new();
    let mut hit_count = 0u64;
    while candidate_input.read_line(&mut line)? != 0 {
        let mut fields = line.trim_end().split('\t');
        let mut candidate = [0u8; 32];
        hex::decode_to_slice(fields.next().context("missing candidate slot")?, &mut candidate)?;
        let dataset: u64 = fields.next().context("missing candidate dataset")?.parse()?;
        let piece: u64 = fields.next().context("missing candidate piece")?.parse()?;
        while record.as_ref().is_some_and(|entry| entry[..32] < candidate[..]) {
            record = next_record(&mut occupied)?;
        }
        if let Some(entry) = &record
            && entry[..32] == candidate[..]
        {
            let header: [u8; 32] = entry[32..].try_into().expect("64-byte record");
            let length = word_u64(header)?;
            if length > MAX_KEYS_PER_PIECE {
                bail!("dataset {dataset} piece {piece} key-array length exceeds resource guard");
            }
            if length != 0 {
                writeln!(hits, "{dataset}\t{piece}\t{length}")?;
                hit_count += 1;
            }
        }
        line.clear();
    }
    hits.flush()?;
    drop(hits);
    sort_file(raw_hits.path(), &hits_path, output, &["-k1,1n", "-k2,2n"])?;
    drop(raw_hits);
    let intersection_seconds = started.elapsed().as_secs_f64() - candidate_seconds;
    eprintln!(
        "intersection found {hit_count} occupied metadata key arrays in {intersection_seconds:.2}s"
    );

    // SAFETY: The input export is immutable for this command's lifetime, as required by the CLI.
    // The file is opened read-only and remains open until all mapped reads have finished.
    let words = unsafe { MmapOptions::new().map(&occupied_file) }
        .context("mapping occupied-slot export")?;
    let index = SlotIndex { words, file: None };
    write_queues(&index, next_dataset, output)?;
    let mut pieces =
        BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&pieces_path)?);
    let mut added_hits =
        BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&added_hits_path)?);
    let mut hit_input = BufReader::new(File::open(&hits_path)?);
    let mut current_dataset = 0;
    let mut metadata_base = [0; 32];
    let mut keys_base = [0; 32];
    let mut empty_values = 0u64;
    while hit_input.read_line(&mut line)? != 0 {
        let mut fields = line.trim_end().split('\t');
        let dataset: u64 = fields.next().context("missing hit dataset")?.parse()?;
        let piece: u64 = fields.next().context("missing hit piece")?.parse()?;
        let length: u64 = fields.next().context("missing hit key count")?.parse()?;
        if dataset != current_dataset {
            current_dataset = dataset;
            metadata_base = nested_mapping_base(dataset, DATA_SET_PIECE_METADATA);
            keys_base = nested_mapping_base(dataset, DATA_SET_PIECE_METADATA_KEYS);
        }
        let (row, key_count, payload_bytes, words) =
            piece_row(&index, dataset, piece, metadata_base, keys_base, slot(length))?
                .context("nonzero intersection hit decoded as empty metadata")?;
        empty_values += row.keys.iter().filter(|key| key.value_hex == "0x").count() as u64;
        serde_json::to_writer(&mut pieces, &row)?;
        pieces.write_all(b"\n")?;
        let data = &mut summary.datasets[dataset as usize - 1];
        let is_added_range = piece >= data.recovered_piece_bound_exclusive;
        data.pieces_with_metadata += 1;
        data.keys += key_count;
        data.payload_bytes += payload_bytes;
        data.nonzero_words += words;
        if is_added_range {
            data.added_pieces_with_metadata += 1;
            data.added_keys += key_count;
            data.added_nonzero_words += words;
            summary.added_pieces_with_metadata += 1;
            summary.added_keys += key_count;
            summary.added_nonzero_words += words;
            writeln!(added_hits, "{dataset}\t{piece}\t{key_count}\t{words}")?;
        } else {
            data.recovered_pieces_with_metadata += 1;
            data.recovered_keys += key_count;
            data.recovered_nonzero_words += words;
            summary.recovered_pieces_with_metadata += 1;
            summary.recovered_keys += key_count;
            summary.recovered_nonzero_words += words;
        }
        summary.pieces_with_metadata += 1;
        summary.keys += key_count;
        summary.payload_bytes += payload_bytes;
        summary.nonzero_words += words;
        *summary.key_count_distribution.entry(key_count).or_default() += 1;
        *summary.nonzero_word_count_distribution.entry(words).or_default() += 1;
        line.clear();
    }
    pieces.flush()?;
    added_hits.flush()?;
    if summary.pieces_with_metadata != hit_count {
        bail!("not all intersection hits were decoded");
    }
    if summary.pieces_scanned
        != summary
            .recovered_pieces_scanned
            .checked_add(summary.added_candidate_pieces)
            .context("reconciled candidate count overflows u64")?
        || summary.pieces_with_metadata
            != summary
                .recovered_pieces_with_metadata
                .checked_add(summary.added_pieces_with_metadata)
                .context("reconciled metadata piece count overflows u64")?
        || summary.keys
            != summary
                .recovered_keys
                .checked_add(summary.added_keys)
                .context("reconciled key count overflows u64")?
        || summary.nonzero_words
            != summary
                .recovered_nonzero_words
                .checked_add(summary.added_nonzero_words)
                .context("reconciled metadata word count overflows u64")?
    {
        bail!("recovered and added-range counts do not reconcile");
    }
    for dataset in &summary.datasets {
        if dataset.piece_scan_bound_exclusive
            != dataset
                .recovered_piece_bound_exclusive
                .checked_add(dataset.added_candidate_pieces)
                .context("reconciled per-dataset candidate count overflows u64")?
            || dataset.pieces_with_metadata
                != dataset
                    .recovered_pieces_with_metadata
                    .checked_add(dataset.added_pieces_with_metadata)
                    .context("reconciled per-dataset metadata piece count overflows u64")?
            || dataset.keys
                != dataset
                    .recovered_keys
                    .checked_add(dataset.added_keys)
                    .context("reconciled per-dataset key count overflows u64")?
            || dataset.nonzero_words
                != dataset
                    .recovered_nonzero_words
                    .checked_add(dataset.added_nonzero_words)
                    .context("reconciled per-dataset metadata word count overflows u64")?
        {
            bail!(
                "dataset {} recovered and added-range counts do not reconcile",
                dataset.data_set_id
            );
        }
    }
    summary.metadata_scan_seconds = started.elapsed().as_secs_f64();
    let mut result = serde_json::to_value(&summary)?;
    result["intersection"] = serde_json::json!({
        "candidateSlots": summary.pieces_scanned, "occupiedKeyArrayHits": hit_count,
        "valueSlotsCrossVerified": summary.keys, "keysWithEmptyValues": empty_values,
        "candidateGenerationAndSortSeconds": candidate_seconds,
        "intersectionAndHitSortSeconds": intersection_seconds,
        "decodeAndCrossVerifySeconds": summary.metadata_scan_seconds - candidate_seconds - intersection_seconds,
        "candidateFile": candidates_path, "hitFile": hits_path,
        "addedRangeHitsFile": added_hits_path,
        "addedRangeHitsColumns": ["dataSetId", "pieceId", "keyCount", "nonzeroWordCount"],
    });
    serde_json::to_writer_pretty(
        BufWriter::new(File::create(output.join("summary.json"))?),
        &result,
    )?;
    eprintln!(
        "decoded {} keys and {} metadata words in {:.2}s total",
        summary.keys, summary.nonzero_words, summary.metadata_scan_seconds
    );
    let exported_path = output.join("occupied-slots.bin");
    if let Err(error) = fs::hard_link(&occupied_path, &exported_path) {
        if error.raw_os_error() != Some(18) {
            return Err(error).context("linking immutable occupied-slot export");
        }
        fs::copy(&occupied_path, &exported_path)
            .context("copying occupied slots across filesystems")?;
    }
    serde_json::to_writer_pretty(
        BufWriter::new(File::create_new(output.join("occupied-slots.json"))?),
        &manifest,
    )?;
    Ok(())
}

fn sort_file(input: &Path, output: &Path, scratch: &Path, keys: &[&str]) -> Result<()> {
    let status = Command::new("sort")
        .env("LC_ALL", "C")
        .args(["-S", "128M", "--parallel=2", "--temporary-directory"])
        .arg(scratch)
        .args(keys)
        .arg("-o")
        .arg(output)
        .arg(input)
        .status()
        .context("running external candidate sort")?;
    if !status.success() {
        bail!("candidate sort failed with {status}");
    }
    Ok(())
}

fn next_record(input: &mut impl Read) -> Result<Option<[u8; 64]>> {
    let mut record = [0u8; 64];
    if input.read(&mut record[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut record[1..]).context("partial occupied-slot record")?;
    Ok(Some(record))
}

fn number(manifest: &Value, field: &str) -> Result<u64> {
    manifest[field].as_u64().with_context(|| format!("invalid {field} in export manifest"))
}

fn text(manifest: &Value, field: &str) -> Result<String> {
    manifest[field]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("invalid {field} in export manifest"))
}
