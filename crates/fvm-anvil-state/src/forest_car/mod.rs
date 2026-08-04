//! Random-access reader for ForestCAR archives.
//!
//! The embedded-index layout and its hash function are adapted from Forest's
//! `src/db/car/forest.rs` and `src/db/car/forest/index` (ChainSafe Systems,
//! Apache-2.0 OR MIT). This module deliberately retains only the reader needed
//! for lookup: index slots stay on disk and block frames are decoded on demand.

pub mod writer;

pub use writer::{ForestCarWriter, ForestCarWriterError};

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use anyhow::anyhow;
use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_car::CarHeader;
use thiserror::Error;

const SKIP_MAGIC: [u8; 4] = [0x50, 0x2a, 0x4d, 0x18];
const SKIP_HEADER_LEN: u64 = 8;
const FOOTER_LEN: u64 = 16;
const INDEX_V1: u64 = 0xdead_beef + 1;
const INDEX_HEADER_LEN: u64 = 24;
const INDEX_PREFIX_LEN: u64 = 8 + INDEX_HEADER_LEN;
const SLOT_LEN: u64 = 16;
const EMPTY_HASH: u64 = u64::MAX;
const MAX_HEADER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BLOCK_BYTES: u64 = 1024 * 1024 * 1024;
const INDEX_FRAME_LEN: u64 = 512 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ForestCarError {
    #[error("I/O while reading ForestCAR: {0}")]
    Io(#[from] io::Error),
    #[error("invalid ForestCAR: {0}")]
    Invalid(&'static str),
    #[error("invalid ForestCAR: {0}")]
    InvalidOwned(String),
    #[error("could not decode CAR header: {0}")]
    Header(String),
}

/// A read-only ForestCAR blockstore backed by an embedded on-disk index.
pub struct ForestCar {
    file: File,
    roots: Vec<Cid>,
    index: EmbeddedIndex,
}

impl ForestCar {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ForestCarError> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < FOOTER_LEN {
            return Err(ForestCarError::Invalid("file is shorter than the ForestCAR footer"));
        }

        let footer_offset = file_len - FOOTER_LEN;
        let mut footer = [0; FOOTER_LEN as usize];
        read_exact_at(&file, footer_offset, &mut footer)?;
        if footer[..4] != SKIP_MAGIC || footer[4..8] != 8_u32.to_le_bytes() {
            return Err(ForestCarError::Invalid("missing ForestCAR footer"));
        }
        let index_payload_offset =
            u64::from_le_bytes(footer[8..16].try_into().expect("fixed slice"));
        let index_start = index_payload_offset
            .checked_sub(SKIP_HEADER_LEN)
            .ok_or(ForestCarError::Invalid("footer index offset precedes skip-frame header"))?;
        if index_start >= footer_offset {
            return Err(ForestCarError::Invalid("embedded index is outside the archive"));
        }

        let roots = read_roots(&file)?;
        if roots.is_empty() {
            return Err(ForestCarError::Invalid("CAR header has no roots"));
        }
        let index = EmbeddedIndex::open(&file, index_start, footer_offset)?;
        Ok(Self { file, roots, index })
    }

    pub fn roots(&self) -> &[Cid] {
        &self.roots
    }

    fn get_block(&self, wanted: &Cid) -> Result<Option<Vec<u8>>, ForestCarError> {
        self.index.lookup(&self.file, wanted, |frame_offset| {
            if frame_offset >= self.index.physical_start {
                return Err(ForestCarError::Invalid("index points outside block frames"));
            }
            read_frame_for_cid(&self.file, frame_offset, wanted)
        })
    }
}

impl Blockstore for ForestCar {
    fn get(&self, k: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
        self.get_block(k).map_err(|error| anyhow!(error))
    }

    fn put_keyed(&self, _: &Cid, _: &[u8]) -> anyhow::Result<()> {
        Err(anyhow!("ForestCar is read-only"))
    }
}

struct EmbeddedIndex {
    physical_start: u64,
    segments: Vec<IndexSegment>,
    logical_len: u64,
    table_offset: u64,
    slots: u64,
    longest_distance: u64,
    initial_buckets: u64,
}

struct IndexSegment {
    physical_data: u64,
    logical_start: u64,
    len: u64,
}

impl EmbeddedIndex {
    fn open(file: &File, physical_start: u64, physical_end: u64) -> Result<Self, ForestCarError> {
        let physical_len = physical_end
            .checked_sub(physical_start)
            .ok_or(ForestCarError::Invalid("index physical range is reversed"))?;
        let max_segments = physical_len
            .checked_div(INDEX_FRAME_LEN + SKIP_HEADER_LEN)
            .and_then(|count| count.checked_add(1))
            .ok_or(ForestCarError::Invalid("index segment count overflow"))?;
        let mut physical = physical_start;
        let mut logical = 0_u64;
        let mut segments = Vec::new();
        while physical < physical_end {
            let header_end = physical
                .checked_add(SKIP_HEADER_LEN)
                .ok_or(ForestCarError::Invalid("index header offset overflow"))?;
            if header_end > physical_end {
                return Err(ForestCarError::Invalid("truncated index skip-frame header"));
            }
            let mut header = [0_u8; 8];
            read_exact_at(file, physical, &mut header)?;
            if header[..4] != SKIP_MAGIC {
                return Err(ForestCarError::Invalid("index contains a non-skippable frame"));
            }
            let len = u64::from(u32::from_le_bytes(header[4..8].try_into().expect("fixed slice")));
            if len == 0 {
                return Err(ForestCarError::Invalid("index contains an empty skip frame"));
            }
            let next = header_end
                .checked_add(len)
                .ok_or(ForestCarError::Invalid("index length overflow"))?;
            if next > physical_end {
                return Err(ForestCarError::Invalid("truncated index skip-frame data"));
            }
            if next < physical_end && len < INDEX_FRAME_LEN {
                return Err(ForestCarError::Invalid("non-final index skip frame is too short"));
            }
            if u64::try_from(segments.len()).expect("segment count fits u64") >= max_segments {
                return Err(ForestCarError::Invalid("embedded index has too many segments"));
            }
            segments.push(IndexSegment { physical_data: header_end, logical_start: logical, len });
            logical = logical
                .checked_add(len)
                .ok_or(ForestCarError::Invalid("index logical length overflow"))?;
            physical = next;
        }
        if segments.is_empty() || logical < INDEX_PREFIX_LEN + SLOT_LEN {
            return Err(ForestCarError::Invalid("embedded index is too short"));
        }
        let mut prefix = [0_u8; INDEX_PREFIX_LEN as usize];
        read_index_exact(file, &segments, logical, 0, &mut prefix)?;
        if u64::from_le_bytes(prefix[0..8].try_into().expect("fixed slice")) != INDEX_V1 {
            return Err(ForestCarError::Invalid("unsupported embedded index version"));
        }
        let longest_distance = u64::from_le_bytes(prefix[8..16].try_into().expect("fixed slice"));
        let initial_buckets = u64::from_le_bytes(prefix[24..32].try_into().expect("fixed slice"));
        let table_bytes = logical - INDEX_PREFIX_LEN;
        if !table_bytes.is_multiple_of(SLOT_LEN) {
            return Err(ForestCarError::Invalid("embedded index has a partial slot"));
        }
        let slots = table_bytes / SLOT_LEN;
        if initial_buckets > slots.saturating_sub(1) {
            return Err(ForestCarError::Invalid("embedded index bucket count exceeds table"));
        }
        Ok(Self {
            physical_start,
            segments,
            logical_len: logical,
            table_offset: INDEX_PREFIX_LEN,
            slots,
            longest_distance,
            initial_buckets,
        })
    }

    fn lookup<T>(
        &self,
        file: &File,
        cid: &Cid,
        mut inspect: impl FnMut(u64) -> Result<Option<T>, ForestCarError>,
    ) -> Result<Option<T>, ForestCarError> {
        if self.initial_buckets == 0 {
            return Ok(None);
        }
        let hash = summary(cid);
        let mut slot = ((u128::from(hash) * u128::from(self.initial_buckets)) >> 64) as u64;
        let mut remaining = self.longest_distance;
        loop {
            if slot >= self.slots {
                return Err(ForestCarError::Invalid("index probe exceeds table"));
            }
            let (stored_hash, mut offset) = self.slot(file, slot)?;
            if stored_hash == EMPTY_HASH {
                return Ok(None);
            }
            if stored_hash == hash {
                loop {
                    if let Some(value) = inspect(offset)? {
                        return Ok(Some(value));
                    }
                    slot = slot
                        .checked_add(1)
                        .ok_or(ForestCarError::Invalid("index slot overflow"))?;
                    if slot >= self.slots {
                        return Err(ForestCarError::Invalid("index probe exceeds table"));
                    }
                    let (next_hash, next_offset) = self.slot(file, slot)?;
                    if next_hash != hash {
                        return Ok(None);
                    }
                    offset = next_offset;
                }
            }
            if remaining == 0 {
                return Ok(None);
            }
            remaining -= 1;
            slot = slot.checked_add(1).ok_or(ForestCarError::Invalid("index slot overflow"))?;
        }
    }

    fn slot(&self, file: &File, slot: u64) -> Result<(u64, u64), ForestCarError> {
        let offset = self
            .table_offset
            .checked_add(
                slot.checked_mul(SLOT_LEN)
                    .ok_or(ForestCarError::Invalid("index slot offset overflow"))?,
            )
            .ok_or(ForestCarError::Invalid("index slot offset overflow"))?;
        let mut bytes = [0_u8; SLOT_LEN as usize];
        read_index_exact(file, &self.segments, self.logical_len, offset, &mut bytes)?;
        let hash = u64::from_le_bytes(bytes[0..8].try_into().expect("fixed slice"));
        let offset = u64::from_le_bytes(bytes[8..16].try_into().expect("fixed slice"));
        if (hash == EMPTY_HASH) != (offset == u64::MAX) {
            return Err(ForestCarError::Invalid("malformed empty index slot"));
        }
        Ok((hash, offset))
    }
}

fn read_roots(file: &File) -> Result<Vec<Cid>, ForestCarError> {
    let reader = PositionedFile::new(file, 0);
    let decoder = zstd::stream::read::Decoder::new(reader)?;
    let mut bounded = decoder.single_frame().take(MAX_HEADER_BYTES + 1);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HEADER_BYTES {
        return Err(ForestCarError::Invalid("compressed CAR header exceeds maximum size"));
    }
    let (header_len, consumed) = read_uvarint(&bytes)?;
    let end = consumed
        .checked_add(
            usize::try_from(header_len)
                .map_err(|_| ForestCarError::Invalid("CAR header length overflows memory"))?,
        )
        .ok_or(ForestCarError::Invalid("CAR header length overflow"))?;
    if end != bytes.len() {
        return Err(ForestCarError::Invalid(
            "compressed header frame contains trailing or truncated data",
        ));
    }
    let header: CarHeader = fvm_ipld_encoding::from_slice(&bytes[consumed..end])
        .map_err(|error| ForestCarError::Header(error.to_string()))?;
    if header.version != 1 {
        return Err(ForestCarError::Invalid("CAR version is not 1"));
    }
    Ok(header.roots)
}

fn read_frame_for_cid(
    file: &File,
    offset: u64,
    wanted: &Cid,
) -> Result<Option<Vec<u8>>, ForestCarError> {
    let reader = PositionedFile::new(file, offset);
    let decoder = zstd::stream::read::Decoder::new(reader)?;
    let mut decoder = decoder.single_frame();
    loop {
        let frame_len = match read_uvarint_reader(&mut decoder) {
            Ok(value) => value,
            Err(ForestCarError::Invalid("unexpected end of frame")) => return Ok(None),
            Err(error) => return Err(error),
        };
        let stored = Cid::read_bytes(&mut decoder)
            .map_err(|error| ForestCarError::InvalidOwned(format!("invalid block CID: {error}")))?;
        let cid_len = u64::try_from(stored.encoded_len()).expect("CID length fits u64");
        let data_len = frame_len
            .checked_sub(cid_len)
            .ok_or(ForestCarError::Invalid("block frame is shorter than its CID"))?;
        if data_len > MAX_BLOCK_BYTES {
            return Err(ForestCarError::Invalid("block exceeds maximum size"));
        }
        if stored == *wanted {
            let mut data = vec![
                0_u8;
                usize::try_from(data_len).map_err(|_| ForestCarError::Invalid(
                    "block length overflows memory"
                ))?
            ];
            decoder.read_exact(&mut data)?;
            return Ok(Some(data));
        }
        copy_exact_to_sink(&mut decoder, data_len)?;
    }
}

fn read_index_exact(
    file: &File,
    segments: &[IndexSegment],
    logical_len: u64,
    offset: u64,
    out: &mut [u8],
) -> Result<(), ForestCarError> {
    let end = offset
        .checked_add(u64::try_from(out.len()).expect("slice length fits u64"))
        .ok_or(ForestCarError::Invalid("index read offset overflow"))?;
    if end > logical_len {
        return Err(ForestCarError::Invalid("index read exceeds table"));
    }
    let mut logical = offset;
    let mut written = 0;
    while written < out.len() {
        let segment = segments
            .iter()
            .find(|segment| {
                logical >= segment.logical_start
                    && logical < segment.logical_start.saturating_add(segment.len)
            })
            .ok_or(ForestCarError::Invalid("index logical offset has no backing segment"))?;
        let in_segment = logical - segment.logical_start;
        let available =
            usize::try_from(segment.len - in_segment).expect("index segment remainder fits usize");
        let count = available.min(out.len() - written);
        read_exact_at(
            file,
            segment
                .physical_data
                .checked_add(in_segment)
                .ok_or(ForestCarError::Invalid("index physical offset overflow"))?,
            &mut out[written..written + count],
        )?;
        logical += u64::try_from(count).expect("count fits u64");
        written += count;
    }
    Ok(())
}

fn summary(cid: &Cid) -> u64 {
    let folded =
        cid.hash().digest().chunks_exact(8).fold(cid.codec() ^ cid.hash().code(), |hash, chunk| {
            hash ^ u64::from_le_bytes(chunk.try_into().expect("exact chunks"))
        });
    folded.saturating_sub(1)
}

#[cfg(unix)]
fn read_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, out, offset)
}

#[cfg(windows)]
fn read_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, out, offset)
}

fn read_exact_at(file: &File, mut offset: u64, mut out: &mut [u8]) -> io::Result<()> {
    while !out.is_empty() {
        let count = read_at(file, out, offset)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected end of file"));
        }
        offset = offset
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file offset overflow"))?;
        out = &mut out[count..];
    }
    Ok(())
}

struct PositionedFile<'a> {
    file: &'a File,
    position: u64,
}
impl<'a> PositionedFile<'a> {
    fn new(file: &'a File, position: u64) -> Self {
        Self { file, position }
    }
}
impl Read for PositionedFile<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = read_at(self.file, out, self.position)?;
        self.position = self
            .position
            .checked_add(u64::try_from(count).expect("read count fits u64"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file offset overflow"))?;
        Ok(count)
    }
}

fn read_uvarint(bytes: &[u8]) -> Result<(u64, usize), ForestCarError> {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().copied().enumerate().take(10) {
        if index == 9 && byte > 1 {
            return Err(ForestCarError::Invalid("invalid unsigned varint"));
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(ForestCarError::Invalid("invalid unsigned varint"))
}

fn read_uvarint_reader(reader: &mut impl Read) -> Result<u64, ForestCarError> {
    let mut value = 0_u64;
    for index in 0..10 {
        let mut byte = [0_u8; 1];
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && index == 0 => {
                return Err(ForestCarError::Invalid("unexpected end of frame"));
            }
            Err(error) => return Err(error.into()),
        }
        if index == 9 && byte[0] > 1 {
            return Err(ForestCarError::Invalid("invalid unsigned varint"));
        }
        value |= u64::from(byte[0] & 0x7f) << (7 * index);
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(ForestCarError::Invalid("invalid unsigned varint"))
}

fn copy_exact_to_sink(reader: &mut impl Read, mut len: u64) -> Result<(), ForestCarError> {
    let mut buffer = [0_u8; 8192];
    while len != 0 {
        let count = usize::try_from(len.min(buffer.len() as u64)).expect("bounded buffer length");
        reader.read_exact(&mut buffer[..count])?;
        len -= u64::try_from(count).expect("count fits u64");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }
    fn compress(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 1).unwrap()
    }
    fn cid(bytes: &[u8]) -> Cid {
        Cid::new_v1(0x55, cid::multihash::Multihash::<64>::wrap(0, bytes).unwrap())
    }
    fn forest_car(root: Cid, block: (&Cid, &[u8])) -> Vec<u8> {
        let header = CarHeader::from(vec![root]);
        let header_bytes = fvm_ipld_encoding::to_vec(&header).unwrap();
        let mut compressed_header = varint(header_bytes.len() as u64);
        compressed_header.extend(header_bytes);
        let compressed_header = compress(&compressed_header);
        let frame_offset = compressed_header.len() as u64;
        let mut frame = Vec::new();
        let mut cid_bytes = Vec::new();
        block.0.write_bytes(&mut cid_bytes).unwrap();
        frame.extend(varint((cid_bytes.len() + block.1.len()) as u64));
        frame.extend(cid_bytes);
        frame.extend(block.1);
        let block_frame = compress(&frame);
        let mut index = Vec::new();
        index.extend(INDEX_V1.to_le_bytes());
        index.extend(0_u64.to_le_bytes());
        index.extend(0_u64.to_le_bytes());
        index.extend(1_u64.to_le_bytes());
        index.extend(summary(block.0).to_le_bytes());
        index.extend(frame_offset.to_le_bytes());
        index.extend(EMPTY_HASH.to_le_bytes());
        index.extend(u64::MAX.to_le_bytes());
        let index_start = frame_offset + block_frame.len() as u64;
        let mut out = compressed_header;
        out.extend(block_frame);
        out.extend(SKIP_MAGIC);
        out.extend((index.len() as u32).to_le_bytes());
        out.extend(index);
        out.extend(SKIP_MAGIC);
        out.extend(8_u32.to_le_bytes());
        out.extend((index_start + SKIP_HEADER_LEN).to_le_bytes());
        out
    }
    fn write_temp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file
    }
    fn replace_index(bytes: &mut Vec<u8>, index: &[u8]) {
        let index_payload = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        let index_start = usize::try_from(index_payload - SKIP_HEADER_LEN).unwrap();
        bytes.truncate(index_start);
        bytes.extend(SKIP_MAGIC);
        bytes.extend((index.len() as u32).to_le_bytes());
        bytes.extend(index);
        bytes.extend(SKIP_MAGIC);
        bytes.extend(8_u32.to_le_bytes());
        bytes.extend(index_payload.to_le_bytes());
    }

    fn index(longest_distance: u64, initial_buckets: u64, slots: &[(u64, u64)]) -> Vec<u8> {
        let mut index = Vec::new();
        index.extend(INDEX_V1.to_le_bytes());
        index.extend(longest_distance.to_le_bytes());
        index.extend(0_u64.to_le_bytes());
        index.extend(initial_buckets.to_le_bytes());
        for &(hash, offset) in slots {
            index.extend(hash.to_le_bytes());
            index.extend(offset.to_le_bytes());
        }
        index
    }

    #[test]
    fn reads_tail_bucket_despite_global_probe_distance() {
        let root = cid(b"root");
        let block = cid(&u64::MAX.to_le_bytes());
        assert_eq!((u128::from(summary(&block)) * 5) >> 64, 4);
        let mut bytes = forest_car(root, (&block, b"payload"));
        let index_payload = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        let old_slot = usize::try_from(index_payload + INDEX_PREFIX_LEN).unwrap();
        let frame_offset =
            u64::from_le_bytes(bytes[old_slot + 8..old_slot + 16].try_into().unwrap());
        replace_index(
            &mut bytes,
            &index(
                2,
                5,
                &[
                    (0, 0),
                    (1, 0),
                    (2, 0),
                    (EMPTY_HASH, u64::MAX),
                    (summary(&block), frame_offset),
                    (EMPTY_HASH, u64::MAX),
                ],
            ),
        );
        let file = write_temp(&bytes);
        assert_eq!(
            ForestCar::open(file.path()).unwrap().get(&block).unwrap(),
            Some(b"payload".to_vec())
        );
    }

    #[test]
    fn rejects_zero_length_index_skip_frames() {
        let root = cid(b"root");
        let block = cid(b"block");
        let mut bytes = forest_car(root, (&block, b"payload"));
        let index_payload = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        bytes.truncate(usize::try_from(index_payload - SKIP_HEADER_LEN).unwrap());
        for _ in 0..2 {
            bytes.extend(SKIP_MAGIC);
            bytes.extend(0_u32.to_le_bytes());
        }
        bytes.extend(SKIP_MAGIC);
        bytes.extend(8_u32.to_le_bytes());
        bytes.extend(index_payload.to_le_bytes());
        let file = write_temp(&bytes);
        assert!(matches!(ForestCar::open(file.path()), Err(ForestCarError::Invalid(_))));
    }

    #[test]
    fn reads_roots_and_a_indexed_block() {
        let root = cid(b"root");
        let block = cid(b"block");
        let file = write_temp(&forest_car(root, (&block, b"payload")));
        let car = ForestCar::open(file.path()).unwrap();
        assert_eq!(car.roots(), &[root]);
        assert_eq!(car.get(&block).unwrap(), Some(b"payload".to_vec()));
    }
    #[test]
    fn returns_none_when_index_has_no_cid() {
        let root = cid(b"root");
        let block = cid(b"block");
        let file = write_temp(&forest_car(root, (&block, b"payload")));
        assert_eq!(ForestCar::open(file.path()).unwrap().get(&cid(b"missing")).unwrap(), None);
    }
    #[test]
    fn verifies_the_full_cid_after_an_index_hash_hit() {
        let root = cid(b"root");
        let stored = cid(b"stored");
        let wanted = cid(b"wanted");
        let mut bytes = forest_car(root, (&stored, b"payload"));
        let index_payload = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        let slot_hash = usize::try_from(index_payload + INDEX_PREFIX_LEN).unwrap();
        bytes[slot_hash..slot_hash + 8].copy_from_slice(&summary(&wanted).to_le_bytes());
        let file = write_temp(&bytes);
        assert_eq!(ForestCar::open(file.path()).unwrap().get(&wanted).unwrap(), None);
    }
    #[test]
    fn rejects_truncated_footer() {
        let file = write_temp(b"not a ForestCAR");
        assert!(matches!(ForestCar::open(file.path()), Err(ForestCarError::Invalid(_))));
    }
    #[test]
    fn rejects_truncated_index() {
        let root = cid(b"root");
        let block = cid(b"block");
        let mut bytes = forest_car(root, (&block, b"payload"));
        bytes.truncate(bytes.len() - 17);
        let file = write_temp(&bytes);
        assert!(ForestCar::open(file.path()).is_err());
    }
}
