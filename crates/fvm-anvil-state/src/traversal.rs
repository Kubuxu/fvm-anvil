// Copyright 2026 Kubuxu
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded-memory primitives for walking IPLD graphs.
//!
//! The link-only DAG-CBOR decoder is adapted from Forest's
//! `utils/encoding/cid_de_cbor.rs` (Copyright 2019-2026 ChainSafe Systems,
//! Apache-2.0 OR MIT) and ref-FVM's DAG-CBOR codec handling. Unlike decoding an
//! IPLD value, it retains only links.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{CBOR, DAG_CBOR, IPLD_RAW};
use thiserror::Error;

const MAX_CBOR_DEPTH: usize = 512;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
const CID_BUCKETS: usize = 1 << 16;
const EMPTY_RECORD: u64 = u64::MAX;

/// Errors returned by the disk set and graph walker.
#[derive(Debug, Error)]
pub enum TraversalError {
    #[error("temporary visited-set I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("blockstore failed while fetching {cid}: {source}")]
    Blockstore {
        cid: Cid,
        #[source]
        source: anyhow::Error,
    },
    #[error("reachable block {0} is missing")]
    MissingBlock(Cid),
    #[error("unsupported codec {codec:#x} for reachable block {cid}")]
    UnsupportedCodec { cid: Cid, codec: u64 },
    #[error("malformed DAG-CBOR block {cid}: {message}")]
    MalformedDagCbor { cid: Cid, message: String },
    #[error("block emission failed for {cid}: {source}")]
    Emit {
        cid: Cid,
        #[source]
        source: anyhow::Error,
    },
}

/// An exact, disk-backed set of CIDs.
///
/// Records contain a 64-bit summary followed by the complete CID. The summary
/// only filters candidates: complete CID bytes are always compared, so summary
/// collisions cannot suppress a CID. Lookup uses constant heap space. The
/// scratch file is unlinked immediately where the platform permits it and is
/// otherwise removed on drop.
pub struct DiskCidSet {
    file: File,
    path: Option<PathBuf>,
    heads: Box<[u64]>,
    len: u64,
}

impl DiskCidSet {
    pub fn create() -> Result<Self, TraversalError> {
        let directory = std::env::temp_dir();
        for _ in 0..128 {
            let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".fvm-anvil-cids-{}-{nonce}", std::process::id()));
            match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(file) => {
                    let retained_path =
                        if std::fs::remove_file(&path).is_ok() { None } else { Some(path) };
                    return Ok(Self {
                        file,
                        path: retained_path,
                        heads: vec![EMPTY_RECORD; CID_BUCKETS].into_boxed_slice(),
                        len: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique visited-set tempfile",
        )
        .into())
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts `cid`, returning `true` exactly when it was not already present.
    pub fn insert(&mut self, cid: &Cid) -> Result<bool, TraversalError> {
        let wanted = cid.to_bytes();
        let bucket = summary(&wanted) as usize & (CID_BUCKETS - 1);
        self.insert_bytes_in_bucket(&wanted, bucket)
    }

    fn insert_bytes_in_bucket(
        &mut self,
        wanted: &[u8],
        bucket: usize,
    ) -> Result<bool, TraversalError> {
        let mut record = self.heads[bucket];
        let mut header = [0_u8; 10];
        let mut candidate = Vec::new();
        while record != EMPTY_RECORD {
            self.file.seek(SeekFrom::Start(record))?;
            self.file.read_exact(&mut header)?;
            let next = u64::from_le_bytes(header[..8].try_into().expect("fixed slice"));
            let length =
                usize::from(u16::from_le_bytes(header[8..].try_into().expect("fixed slice")));
            candidate.resize(length, 0);
            self.file.read_exact(&mut candidate[..length])?;
            if &candidate[..length] == wanted {
                return Ok(false);
            }
            record = next;
        }

        let length = u16::try_from(wanted.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "CID exceeds scratch record limit")
        })?;
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&self.heads[bucket].to_le_bytes())?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(wanted)?;
        self.heads[bucket] = offset;
        self.len += 1;
        Ok(true)
    }

    #[cfg(test)]
    fn insert_with_summary(
        &mut self,
        cid: &Cid,
        forced_summary: u64,
    ) -> Result<bool, TraversalError> {
        self.insert_bytes_in_bucket(&cid.to_bytes(), forced_summary as usize & (CID_BUCKETS - 1))
    }
}

impl Drop for DiskCidSet {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn summary(bytes: &[u8]) -> u64 {
    // FNV-1a is used only as a disk-scan prefilter, never as identity.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Calls `visit` for every CID link in one well-formed DAG-CBOR value.
///
/// This decoder retains no decoded IPLD tree or link collection; callers can
/// append links directly to their traversal frontier.
pub fn scan_dag_cbor_links<F>(data: &[u8], mut visit: F) -> Result<(), String>
where
    F: FnMut(Cid),
{
    let mut cursor = 0;
    scan_item(data, &mut cursor, 0, &mut visit)?;
    if cursor != data.len() {
        return Err(format!("trailing data at byte {cursor}"));
    }
    Ok(())
}

fn scan_item<F>(data: &[u8], cursor: &mut usize, depth: usize, visit: &mut F) -> Result<(), String>
where
    F: FnMut(Cid),
{
    if depth > MAX_CBOR_DEPTH {
        return Err(format!("nesting exceeds {MAX_CBOR_DEPTH} levels"));
    }
    let initial = take_byte(data, cursor)?;
    let major = initial >> 5;
    let additional = initial & 0x1f;
    match major {
        0 | 1 => {
            read_argument(data, cursor, additional)?;
        }
        2 => {
            let length = definite_length(data, cursor, additional)?;
            take(data, cursor, length)?;
        }
        3 => {
            let length = definite_length(data, cursor, additional)?;
            let text = take(data, cursor, length)?;
            std::str::from_utf8(text).map_err(|error| format!("invalid UTF-8 text: {error}"))?;
        }
        4 => {
            let length = definite_length(data, cursor, additional)?;
            for _ in 0..length {
                scan_item(data, cursor, depth + 1, visit)?;
            }
        }
        5 => {
            let length = definite_length(data, cursor, additional)?;
            for _ in 0..length {
                let key = take_byte(data, cursor)?;
                if key >> 5 != 3 {
                    return Err("DAG-CBOR map key is not text".into());
                }
                let key_length = definite_length(data, cursor, key & 0x1f)?;
                let key_bytes = take(data, cursor, key_length)?;
                std::str::from_utf8(key_bytes)
                    .map_err(|error| format!("invalid UTF-8 map key: {error}"))?;
                scan_item(data, cursor, depth + 1, visit)?;
            }
        }
        6 => {
            let tag = read_argument(data, cursor, additional)?;
            if tag != 42 {
                return Err(format!("unsupported CBOR tag {tag}"));
            }
            let bytes_header = take_byte(data, cursor)?;
            if bytes_header >> 5 != 2 {
                return Err("CID tag is not followed by a byte string".into());
            }
            let length = definite_length(data, cursor, bytes_header & 0x1f)?;
            let encoded = take(data, cursor, length)?;
            let Some((&0, cid_bytes)) = encoded.split_first() else {
                return Err("CID byte string lacks identity prefix 0".into());
            };
            let mut reader = cid_bytes;
            let cid =
                Cid::read_bytes(&mut reader).map_err(|error| format!("invalid CID: {error}"))?;
            if !reader.is_empty() {
                return Err("CID byte string contains trailing data".into());
            }
            visit(cid);
        }
        7 => match additional {
            20..=22 => {}
            25 => {
                let bits =
                    u16::from_be_bytes(take(data, cursor, 2)?.try_into().expect("fixed slice"));
                let exponent = bits & 0x7c00;
                let fraction = bits & 0x03ff;
                if exponent == 0x7c00 && fraction != 0 {
                    return Err("NaN is not valid DAG-CBOR".into());
                }
            }
            26 => {
                let value =
                    f32::from_be_bytes(take(data, cursor, 4)?.try_into().expect("fixed slice"));
                if value.is_nan() {
                    return Err("NaN is not valid DAG-CBOR".into());
                }
            }
            27 => {
                let value =
                    f64::from_be_bytes(take(data, cursor, 8)?.try_into().expect("fixed slice"));
                if value.is_nan() {
                    return Err("NaN is not valid DAG-CBOR".into());
                }
            }
            _ => return Err(format!("unsupported CBOR simple value {additional}")),
        },
        _ => unreachable!(),
    }
    Ok(())
}

fn definite_length(data: &[u8], cursor: &mut usize, additional: u8) -> Result<usize, String> {
    let value = read_argument(data, cursor, additional)?;
    usize::try_from(value).map_err(|_| "CBOR length does not fit usize".into())
}

fn read_argument(data: &[u8], cursor: &mut usize, additional: u8) -> Result<u64, String> {
    match additional {
        0..=23 => Ok(u64::from(additional)),
        24 => Ok(u64::from(take_byte(data, cursor)?)),
        25 => Ok(u64::from(u16::from_be_bytes(
            take(data, cursor, 2)?.try_into().expect("fixed slice"),
        ))),
        26 => Ok(u64::from(u32::from_be_bytes(
            take(data, cursor, 4)?.try_into().expect("fixed slice"),
        ))),
        27 => Ok(u64::from_be_bytes(take(data, cursor, 8)?.try_into().expect("fixed slice"))),
        31 => Err("indefinite-length CBOR is not valid DAG-CBOR".into()),
        _ => Err(format!("reserved CBOR additional information {additional}")),
    }
}

fn take_byte(data: &[u8], cursor: &mut usize) -> Result<u8, String> {
    Ok(take(data, cursor, 1)?[0])
}

fn take<'a>(data: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], String> {
    let end = cursor.checked_add(length).ok_or_else(|| "CBOR length overflow".to_owned())?;
    let bytes = data
        .get(*cursor..end)
        .ok_or_else(|| format!("unexpected end of CBOR at byte {}", *cursor))?;
    *cursor = end;
    Ok(bytes)
}

/// Depth-first traversal of `roots`, emitting each non-excluded CID and block once.
///
/// Exclusion and visited checks happen before fetching. `CBOR` and `IPLD_RAW`
/// blocks are leaves; only `DAG_CBOR` blocks contribute links.
pub fn traverse_blockstore<B, X, E>(
    store: &B,
    roots: &[Cid],
    mut excluded: X,
    mut emit: E,
) -> Result<u64, TraversalError>
where
    B: Blockstore,
    X: FnMut(&Cid) -> bool,
    E: FnMut(&Cid, &[u8]) -> anyhow::Result<()>,
{
    let mut visited = DiskCidSet::create()?;
    let mut frontier = roots.to_vec();
    let mut emitted = 0u64;

    while let Some(cid) = frontier.pop() {
        if excluded(&cid) || !visited.insert(&cid)? {
            continue;
        }
        let data = store
            .get(&cid)
            .map_err(|source| TraversalError::Blockstore { cid, source })?
            .ok_or(TraversalError::MissingBlock(cid))?;

        match cid.codec() {
            DAG_CBOR => scan_dag_cbor_links(&data, |link| frontier.push(link))
                .map_err(|message| TraversalError::MalformedDagCbor { cid, message })?,
            CBOR | IPLD_RAW => {}
            codec => return Err(TraversalError::UnsupportedCodec { cid, codec }),
        }

        emit(&cid, &data).map_err(|source| TraversalError::Emit { cid, source })?;
        emitted += 1;
    }
    Ok(emitted)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use cid::multihash::Multihash;

    use super::*;

    fn cid(codec: u64, marker: u8) -> Cid {
        Cid::new_v1(codec, Multihash::wrap(0, &[marker]).unwrap())
    }

    fn dag_array(links: &[Cid]) -> Vec<u8> {
        assert!(links.len() < 24);
        let mut bytes = vec![0x80 | links.len() as u8];
        for link in links {
            let cid = link.to_bytes();
            assert!(cid.len() + 1 < 24);
            bytes.extend([0xd8, 0x2a, 0x40 | (cid.len() as u8 + 1), 0]);
            bytes.extend(cid);
        }
        bytes
    }

    #[derive(Default)]
    struct Store {
        blocks: HashMap<Cid, Vec<u8>>,
        fetched: RefCell<Vec<Cid>>,
    }

    impl Blockstore for Store {
        fn get(&self, cid: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
            self.fetched.borrow_mut().push(*cid);
            Ok(self.blocks.get(cid).cloned())
        }

        fn put_keyed(&self, _: &Cid, _: &[u8]) -> anyhow::Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn disk_set_compares_full_cids_on_summary_collision() {
        let first = cid(IPLD_RAW, 1);
        let second = cid(IPLD_RAW, 2);
        let mut set = DiskCidSet::create().unwrap();
        assert!(set.insert_with_summary(&first, 7).unwrap());
        assert!(set.insert_with_summary(&second, 7).unwrap());
        assert!(!set.insert_with_summary(&first, 7).unwrap());
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn duplicate_links_are_fetched_and_emitted_once() {
        let root = cid(DAG_CBOR, 1);
        let leaf = cid(IPLD_RAW, 2);
        let store = Store {
            blocks: [(root, dag_array(&[leaf, leaf])), (leaf, b"leaf".to_vec())]
                .into_iter()
                .collect(),
            ..Store::default()
        };
        let emitted = RefCell::new(Vec::new());
        let count = traverse_blockstore(
            &store,
            &[root],
            |_| false,
            |cid, _| {
                emitted.borrow_mut().push(*cid);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(&*emitted.borrow(), &[root, leaf]);
        assert_eq!(&*store.fetched.borrow(), &[root, leaf]);
    }

    #[test]
    fn exclusion_prevents_fetch_and_emit() {
        let root = cid(DAG_CBOR, 1);
        let excluded = cid(IPLD_RAW, 2);
        let store = Store {
            blocks: [(root, dag_array(&[excluded]))].into_iter().collect(),
            ..Store::default()
        };
        let emitted = Cell::new(0);
        traverse_blockstore(
            &store,
            &[root],
            |cid| *cid == excluded,
            |_, _| {
                emitted.set(emitted.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(emitted.get(), 1);
        assert_eq!(&*store.fetched.borrow(), &[root]);
    }

    #[test]
    fn missing_reachable_block_fails() {
        let root = cid(DAG_CBOR, 1);
        let missing = cid(CBOR, 2);
        let store = Store {
            blocks: [(root, dag_array(&[missing]))].into_iter().collect(),
            ..Store::default()
        };
        let error = traverse_blockstore(&store, &[root], |_| false, |_, _| Ok(())).unwrap_err();
        assert!(matches!(error, TraversalError::MissingBlock(cid) if cid == missing));
    }

    #[test]
    fn raw_and_cbor_are_leaves_and_bad_inputs_are_rejected() {
        let raw = cid(IPLD_RAW, 1);
        let cbor = cid(CBOR, 2);
        let unsupported = cid(0x70, 3);
        let store = Store {
            blocks: [(raw, vec![0xff]), (cbor, vec![0xff]), (unsupported, vec![])]
                .into_iter()
                .collect(),
            ..Store::default()
        };
        assert_eq!(traverse_blockstore(&store, &[raw, cbor], |_| false, |_, _| Ok(())).unwrap(), 2);
        let error =
            traverse_blockstore(&store, &[unsupported], |_| false, |_, _| Ok(())).unwrap_err();
        assert!(matches!(error, TraversalError::UnsupportedCodec { codec: 0x70, .. }));

        let malformed = cid(DAG_CBOR, 4);
        let malformed_store = Store {
            blocks: [(malformed, vec![0xd8, 0x2a, 0x41, 0x00])].into_iter().collect(),
            ..Store::default()
        };
        let error = traverse_blockstore(&malformed_store, &[malformed], |_| false, |_, _| Ok(()))
            .unwrap_err();
        assert!(matches!(error, TraversalError::MalformedDagCbor { .. }));
    }
}
