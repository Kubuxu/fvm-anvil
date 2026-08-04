// Copyright 2019-2026 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT
//
// The ForestCAR framing, footer, CID summary, and V1 index layout below are
// adapted from Forest's `src/db/car/forest.rs` and
// `src/db/car/forest/index` implementation. The external sort and scratch-file
// construction keep this synchronous adaptation bounded in memory.

use std::{
    cmp,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use cid::Cid;
use fvm_ipld_car::CarHeader;
use thiserror::Error;

use super::PositionedFile;

const FRAME_TARGET: u64 = 8192;
#[cfg(not(test))]
const SORT_MEMORY: usize = 8 * 1024 * 1024;
#[cfg(test)]
const SORT_MEMORY: usize = 8 * RECORD_LEN as usize;
const RECORD_LEN: u64 = 16;
const SLOT_LEN: u64 = 16;
const INDEX_PREFIX_LEN: u64 = 32;
const INDEX_FRAME_DATA_MAX: u64 = 512 * 1024 * 1024;
const INDEX_V1: u64 = 0xdead_beef + 1;
const EMPTY: u64 = u64::MAX;
const SKIP_MAGIC: [u8; 4] = [0x50, 0x2a, 0x4d, 0x18];
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum ForestCarWriterError {
    #[error("I/O while writing ForestCAR: {0}")]
    Io(#[from] io::Error),
    #[error("could not encode CAR header: {0}")]
    Header(String),
    #[error("could not encode block CID: {0}")]
    Cid(String),
    #[error("ForestCAR requires at least one root")]
    NoRoots,
    #[error("ForestCAR size limit exceeded: {0}")]
    TooLarge(&'static str),
}

pub type Result<T, E = ForestCarWriterError> = std::result::Result<T, E>;

#[derive(Default)]
struct ScratchCleanup {
    paths: Vec<PathBuf>,
}

impl ScratchCleanup {
    fn create(&mut self, parent: &Path, stem: &str, kind: &str) -> io::Result<(PathBuf, File)> {
        let (path, file) = create_scratch(parent, stem, kind)?;
        self.paths.push(path.clone());
        Ok((path, file))
    }

    fn disarm(&mut self) {
        self.paths.clear();
    }
}

impl Drop for ScratchCleanup {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// A synchronous, bounded-memory encoder for Forest-compatible ForestCAR files.
pub struct ForestCarWriter {
    output_path: PathBuf,
    output_temp_path: PathBuf,
    entries_path: PathBuf,
    merge_path: PathBuf,
    frame_path: PathBuf,
    index_path: PathBuf,
    output: File,
    entries: File,
    frame: Option<zstd::stream::write::Encoder<'static, File>>,
    frame_offset: u64,
    frame_has_blocks: bool,
    committed: bool,
}

impl ForestCarWriter {
    pub fn create(output: impl AsRef<Path>, roots: &[Cid]) -> Result<Self> {
        if roots.is_empty() {
            return Err(ForestCarWriterError::NoRoots);
        }
        let output_path = output.as_ref().to_path_buf();
        let parent = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let stem = output_path.file_name().and_then(|name| name.to_str()).unwrap_or("forest.car");
        let mut cleanup = ScratchCleanup::default();
        let (output_temp_path, output_file) = cleanup.create(parent, stem, "output")?;
        let (entries_path, entries) = cleanup.create(parent, stem, "entries")?;
        let (merge_path, _) = cleanup.create(parent, stem, "merge")?;
        let (frame_path, frame_file) = cleanup.create(parent, stem, "frame")?;
        let (index_path, _) = cleanup.create(parent, stem, "index")?;

        let mut output = output_file;
        let header = CarHeader::from(roots.to_vec());
        let encoded = fvm_ipld_encoding::to_vec(&header)
            .map_err(|error| ForestCarWriterError::Header(error.to_string()))?;
        {
            let mut encoder =
                zstd::stream::write::Encoder::new(&mut output, zstd::DEFAULT_COMPRESSION_LEVEL)?;
            write_uvarint(&mut encoder, encoded.len() as u64)?;
            encoder.write_all(&encoded)?;
            encoder.finish()?;
        }
        let frame_offset = output.stream_position()?;
        let frame = zstd::stream::write::Encoder::new(frame_file, zstd::DEFAULT_COMPRESSION_LEVEL)?;

        let writer = Self {
            output_path,
            output_temp_path,
            entries_path,
            merge_path,
            frame_path,
            index_path,
            output,
            entries,
            frame: Some(frame),
            frame_offset,
            frame_has_blocks: false,
            committed: false,
        };
        cleanup.disarm();
        Ok(writer)
    }

    pub fn write_block(&mut self, cid: &Cid, data: &[u8]) -> Result<()> {
        if self.frame_has_blocks && self.frame_len()? > FRAME_TARGET {
            self.flush_frame()?;
        }
        let cid_len = u64::try_from(cid.encoded_len())
            .map_err(|_| ForestCarWriterError::TooLarge("CID length"))?;
        let data_len = u64::try_from(data.len())
            .map_err(|_| ForestCarWriterError::TooLarge("block length"))?;
        let record_len = cid_len
            .checked_add(data_len)
            .ok_or(ForestCarWriterError::TooLarge("block record length"))?;
        let frame = self.frame.as_mut().expect("frame encoder is present");
        write_uvarint(frame, record_len)?;
        cid.write_bytes(&mut *frame)
            .map_err(|error| ForestCarWriterError::Cid(error.to_string()))?;
        frame.write_all(data)?;
        frame.flush()?;
        write_record(&mut self.entries, (summary(cid), self.frame_offset))?;
        self.frame_has_blocks = true;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        if self.frame_has_blocks {
            self.flush_frame()?;
        }
        self.entries.sync_all()?;
        let entry_count = self.entries.metadata()?.len() / RECORD_LEN;
        sort_records(&mut self.entries, &self.merge_path, entry_count)?;
        let (unique, collisions) = count_records(&mut self.entries, entry_count)?;
        let initial_buckets = initial_width(unique)?;
        let (logical_len, longest_distance) = self.build_index(entry_count, initial_buckets)?;
        patch_index_header(&mut self.index_file()?, longest_distance, collisions, initial_buckets)?;

        let index_start = self.output.stream_position()?;
        copy_index_frames(&mut self.index_file()?, logical_len, &mut self.output)?;
        self.output.write_all(&SKIP_MAGIC)?;
        self.output.write_all(&8_u32.to_le_bytes())?;
        self.output.write_all(
            &index_start
                .checked_add(8)
                .ok_or(ForestCarWriterError::TooLarge("footer index offset"))?
                .to_le_bytes(),
        )?;
        self.output.sync_all()?;
        fs::rename(&self.output_temp_path, &self.output_path)?;
        sync_parent(&self.output_path)?;
        self.committed = true;
        Ok(())
    }

    fn frame_len(&mut self) -> Result<u64> {
        let frame = self.frame.as_mut().expect("frame encoder is present");
        frame.flush()?;
        Ok(frame.get_ref().metadata()?.len())
    }

    fn flush_frame(&mut self) -> Result<()> {
        let encoder = self.frame.take().expect("frame encoder is present");
        let mut frame_file = encoder.finish()?;
        frame_file.seek(SeekFrom::Start(0))?;
        io::copy(&mut frame_file, &mut self.output)?;
        self.frame_offset = self.output.stream_position()?;
        frame_file.set_len(0)?;
        frame_file.seek(SeekFrom::Start(0))?;
        self.frame =
            Some(zstd::stream::write::Encoder::new(frame_file, zstd::DEFAULT_COMPRESSION_LEVEL)?);
        self.frame_has_blocks = false;
        Ok(())
    }

    fn index_file(&self) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(&self.index_path)
    }

    fn build_index(&mut self, entry_count: u64, initial_buckets: u64) -> Result<(u64, u64)> {
        let mut index = self.index_file()?;
        index.set_len(0)?;
        index.write_all(&INDEX_V1.to_le_bytes())?;
        index.write_all(&[0; 24])?;
        self.entries.seek(SeekFrom::Start(0))?;

        let mut previous = None;
        let mut written_slots = 0_u64;
        let mut longest_distance = 0_u64;
        for _ in 0..entry_count {
            let record = read_record(&mut self.entries)?;
            if previous == Some(record) {
                continue;
            }
            previous = Some(record);
            let ideal = ideal_slot(record.0, initial_buckets);
            while written_slots < ideal {
                write_empty(&mut index)?;
                written_slots += 1;
            }
            longest_distance = cmp::max(longest_distance, written_slots - ideal);
            write_record(&mut index, record)?;
            written_slots += 1;
        }
        while written_slots < initial_buckets {
            write_empty(&mut index)?;
            written_slots += 1;
        }
        write_empty(&mut index)?;
        let logical_len = INDEX_PREFIX_LEN
            .checked_add(
                written_slots
                    .checked_add(1)
                    .and_then(|slots| slots.checked_mul(SLOT_LEN))
                    .ok_or(ForestCarWriterError::TooLarge("index table"))?,
            )
            .ok_or(ForestCarWriterError::TooLarge("index length"))?;
        index.sync_all()?;
        Ok((logical_len, longest_distance))
    }
}

impl Drop for ForestCarWriter {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.output_temp_path);
        }
        for path in [&self.entries_path, &self.merge_path, &self.frame_path, &self.index_path] {
            let _ = fs::remove_file(path);
        }
    }
}

fn create_scratch(parent: &Path, stem: &str, kind: &str) -> io::Result<(PathBuf, File)> {
    for _ in 0..1024 {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{stem}.{}.{}.{}.tmp", std::process::id(), id, kind));
        match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not allocate ForestCAR scratch file"))
}

fn sort_records(file: &mut File, merge_path: &Path, records: u64) -> Result<()> {
    let records_per_run =
        u64::try_from(SORT_MEMORY / RECORD_LEN as usize).expect("constant fits u64");
    let mut start = 0_u64;
    while start < records {
        let count = cmp::min(records_per_run, records - start);
        file.seek(SeekFrom::Start(start * RECORD_LEN))?;
        let mut run = Vec::with_capacity(usize::try_from(count).expect("bounded run"));
        for _ in 0..count {
            run.push(read_record(file)?);
        }
        run.sort_unstable();
        file.seek(SeekFrom::Start(start * RECORD_LEN))?;
        for record in run {
            write_record(file, record)?;
        }
        start += count;
    }

    let mut run_len = records_per_run;
    while run_len < records {
        let mut output =
            OpenOptions::new().read(true).write(true).truncate(true).open(merge_path)?;
        let mut pair_start = 0_u64;
        while pair_start < records {
            let left_end = cmp::min(pair_start + run_len, records);
            let right_end = cmp::min(left_end + run_len, records);
            merge_pair(file, &mut output, pair_start, left_end, left_end, right_end)?;
            pair_start = right_end;
        }
        output.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        output.seek(SeekFrom::Start(0))?;
        io::copy(&mut output, file)?;
        run_len = run_len.checked_mul(2).ok_or(ForestCarWriterError::TooLarge("sort run"))?;
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}
fn merge_pair(
    input: &File,
    output: &mut File,
    left: u64,
    left_end: u64,
    right: u64,
    right_end: u64,
) -> io::Result<()> {
    let left_offset = left
        .checked_mul(RECORD_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "record offset overflow"))?;
    let right_offset = right
        .checked_mul(RECORD_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "record offset overflow"))?;
    let mut left_reader =
        BufReader::with_capacity(64 * 1024, PositionedFile::new(input, left_offset));
    let mut right_reader =
        BufReader::with_capacity(64 * 1024, PositionedFile::new(input, right_offset));
    let mut left_remaining = left_end - left;
    let mut right_remaining = right_end - right;
    let mut left_record = read_next_record(&mut left_reader, &mut left_remaining)?;
    let mut right_record = read_next_record(&mut right_reader, &mut right_remaining)?;
    let mut output = BufWriter::with_capacity(64 * 1024, output);
    while left_record.is_some() || right_record.is_some() {
        let take_left = match (left_record, right_record) {
            (Some(a), Some(b)) => a <= b,
            (Some(_), None) => true,
            _ => false,
        };
        if take_left {
            write_record(&mut output, left_record.expect("left record"))?;
            left_record = read_next_record(&mut left_reader, &mut left_remaining)?;
        } else {
            write_record(&mut output, right_record.expect("right record"))?;
            right_record = read_next_record(&mut right_reader, &mut right_remaining)?;
        }
    }
    output.flush()
}

fn read_next_record(reader: &mut impl Read, remaining: &mut u64) -> io::Result<Option<(u64, u64)>> {
    if *remaining == 0 {
        return Ok(None);
    }
    *remaining -= 1;
    read_record(reader).map(Some)
}

fn count_records(file: &mut File, records: u64) -> io::Result<(u64, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut previous = None;
    let mut unique = 0_u64;
    let mut collisions = 0_u64;
    let mut previous_hash = None;
    for _ in 0..records {
        let record = read_record(file)?;
        if previous == Some(record) {
            continue;
        }
        unique += 1;
        if previous_hash == Some(record.0) {
            collisions += 1;
        }
        previous = Some(record);
        previous_hash = Some(record.0);
    }
    Ok((unique, collisions))
}

fn initial_width(entries: u64) -> Result<u64> {
    if entries == 0 {
        return Ok(0);
    }
    let width = (entries as f64 / 0.8) as u64;
    Ok(cmp::max(width, entries))
}

fn ideal_slot(hash: u64, initial_buckets: u64) -> u64 {
    if initial_buckets == 0 {
        0
    } else {
        ((u128::from(hash) * u128::from(initial_buckets)) >> 64) as u64
    }
}

fn patch_index_header(
    file: &mut File,
    longest: u64,
    collisions: u64,
    buckets: u64,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&longest.to_le_bytes())?;
    file.write_all(&collisions.to_le_bytes())?;
    file.write_all(&buckets.to_le_bytes())?;
    file.sync_all()
}

fn copy_index_frames(index: &mut File, logical_len: u64, output: &mut File) -> io::Result<()> {
    index.seek(SeekFrom::Start(0))?;
    let mut remaining = logical_len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let frame_len = cmp::min(remaining, INDEX_FRAME_DATA_MAX);
        output.write_all(&SKIP_MAGIC)?;
        output.write_all(&(frame_len as u32).to_le_bytes())?;
        let mut in_frame = frame_len;
        while in_frame > 0 {
            let count = cmp::min(in_frame, buffer.len() as u64) as usize;
            index.read_exact(&mut buffer[..count])?;
            output.write_all(&buffer[..count])?;
            in_frame -= count as u64;
            remaining -= count as u64;
        }
    }
    Ok(())
}

fn write_empty(writer: &mut impl Write) -> io::Result<()> {
    write_record(writer, (EMPTY, EMPTY))
}

fn write_record(writer: &mut impl Write, record: (u64, u64)) -> io::Result<()> {
    writer.write_all(&record.0.to_le_bytes())?;
    writer.write_all(&record.1.to_le_bytes())
}

fn read_record(reader: &mut impl Read) -> io::Result<(u64, u64)> {
    let mut bytes = [0_u8; 16];
    reader.read_exact(&mut bytes)?;
    Ok((
        u64::from_le_bytes(bytes[..8].try_into().expect("fixed slice")),
        u64::from_le_bytes(bytes[8..].try_into().expect("fixed slice")),
    ))
}

fn write_uvarint(writer: &mut impl Write, mut value: u64) -> io::Result<()> {
    let mut bytes = [0_u8; 10];
    let mut len = 0;
    while value >= 0x80 {
        bytes[len] = (value as u8 & 0x7f) | 0x80;
        value >>= 7;
        len += 1;
    }
    bytes[len] = value as u8;
    writer.write_all(&bytes[..=len])
}

fn summary(cid: &Cid) -> u64 {
    let folded =
        cid.hash().digest().chunks_exact(8).fold(cid.codec() ^ cid.hash().code(), |hash, chunk| {
            hash ^ u64::from_le_bytes(chunk.try_into().expect("exact chunks"))
        });
    folded.saturating_sub(1)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forest_car::ForestCar;
    use fvm_ipld_blockstore::Blockstore;

    #[cfg(unix)]
    #[test]
    fn syncs_current_directory_for_bare_output_path() {
        sync_parent(Path::new("archive.forest.car.zst")).unwrap();
    }

    fn collision_cids() -> (Cid, Cid) {
        let first = [1_u8; 8];
        let second = [2_u8; 8];
        let third = [3_u8; 8];
        let mut left = Vec::from(first);
        left.extend(second);
        let mut right = Vec::from(third);
        right.extend([0_u8; 8]);
        let left = Cid::new_v1(0x55, cid::multihash::Multihash::<64>::wrap(0, &left).unwrap());
        let right = Cid::new_v1(0x55, cid::multihash::Multihash::<64>::wrap(0, &right).unwrap());
        assert_eq!(summary(&left), summary(&right));
        (left, right)
    }

    #[test]
    fn writes_multiple_frames_with_shared_frame_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("archive.forest.car.zst");
        let (first, second) = collision_cids();
        let third = Cid::new_v1(0x55, cid::multihash::Multihash::<64>::wrap(0, b"third").unwrap());
        let mut state = 0x1234_5678_u32;
        let mut large = Vec::with_capacity(64 * 1024);
        for _ in 0..64 * 1024 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            large.push(state as u8);
        }
        let mut writer = ForestCarWriter::create(&output, &[first]).unwrap();
        writer.write_block(&first, b"first").unwrap();
        writer.write_block(&second, b"second").unwrap();
        writer.write_block(&third, &large).unwrap();
        writer.write_block(&third, b"latest").unwrap();
        writer.finish().unwrap();
        let bytes = fs::read(&output).unwrap();
        let zstd_frames =
            bytes.windows(4).filter(|window| *window == [0x28, 0xb5, 0x2f, 0xfd]).count();
        assert!(zstd_frames >= 3, "header plus two block frames");

        let archive = ForestCar::open(&output).unwrap();
        assert_eq!(archive.get(&first).unwrap(), Some(b"first".to_vec()));
        assert_eq!(archive.get(&second).unwrap(), Some(b"second".to_vec()));
        assert_eq!(archive.get(&third).unwrap(), Some(large));
    }

    #[test]
    fn preserves_full_cid_collision_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("collision.forest.car.zst");
        let (left, right) = collision_cids();
        let mut state = 0x8765_4321_u32;
        let mut left_data = Vec::with_capacity(64 * 1024);
        for _ in 0..64 * 1024 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            left_data.push(state as u8);
        }
        let mut writer = ForestCarWriter::create(&output, &[left]).unwrap();
        writer.write_block(&left, &left_data).unwrap();
        writer.write_block(&right, b"right").unwrap();
        writer.finish().unwrap();

        let archive = ForestCar::open(&output).unwrap();
        assert_eq!(archive.get(&left).unwrap(), Some(left_data));
        assert_eq!(archive.get(&right).unwrap(), Some(b"right".to_vec()));
    }

    #[test]
    fn externally_merges_sorted_runs() {
        let mut input = tempfile::tempfile().unwrap();
        for value in (0_u64..40).rev() {
            write_record(&mut input, (value % 7, value)).unwrap();
        }
        let merge = tempfile::NamedTempFile::new().unwrap();
        sort_records(&mut input, merge.path(), 40).unwrap();

        let mut previous = None;
        for _ in 0..40 {
            let record = read_record(&mut input).unwrap();
            assert!(previous.is_none_or(|value| value <= record));
            previous = Some(record);
        }
    }

    #[test]
    fn does_not_replace_output_until_finished() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("atomic.forest.car.zst");
        fs::write(&output, b"previous archive").unwrap();
        let root = Cid::new_v1(0x55, cid::multihash::Multihash::<64>::wrap(0, b"root").unwrap());
        {
            let mut writer = ForestCarWriter::create(&output, &[root]).unwrap();
            writer.write_block(&root, b"replacement").unwrap();
        }
        assert_eq!(fs::read(output).unwrap(), b"previous archive");
    }
}
