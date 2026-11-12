#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Build a bounded-memory, storage-ordered plan for FWSS PR #658."""

from __future__ import annotations

import argparse
import hashlib
import json
import sqlite3
from collections import Counter
from pathlib import Path

from eth_abi import encode
from eth_hash.auto import keccak

SIGNATURE = b"cleanupLegacyPieceMetadata(bytes)"
PR_COMMIT = "35c163640aa20c65da04b481a460ab416af3df9c"
MODULUS = 1 << 256


def word(value: int) -> bytes:
    return value.to_bytes(32, "big")


def slot_hex(value: int) -> str:
    return "0x" + word(value).hex()


def mapping_slot(identifier: int, base: int) -> int:
    return int.from_bytes(keccak(word(identifier) + word(base)), "big")


def data_slot(head: int) -> int:
    return int.from_bytes(keccak(word(head)), "big")


def calldata(packed: bytes) -> str:
    return "0x" + (keccak(SIGNATURE)[:4] + encode(["bytes"], [packed])).hex()


def operation(kind: int, dataset: int, piece: int = 0, *, key: bytes = b"",
              index: int = 0, count: int = 0) -> bytes:
    prefix = bytes([kind]) + dataset.to_bytes(5, "big")
    if kind == 4:
        return prefix + count.to_bytes(2, "big")
    prefix += piece.to_bytes(5, "big")
    if kind in (3, 5):
        return prefix + bytes([index])
    if kind == 2:
        return prefix
    if kind == 1:
        prefix += bytes([count])
    return prefix + bytes([len(key)]) + key


def piece_operations(row: dict):
    """Split heads from payloads; no operation reads another operation's target."""
    dataset, piece = row["dataSetId"], row["pieceId"]
    if not (0 <= dataset < 1 << 40 and 0 <= piece < 1 << 40):
        raise ValueError("dataset/piece ID exceeds PR #658's uint40 encoding")
    keys = row["keys"]
    if not 1 <= len(keys) <= 5:
        raise ValueError(f"{dataset}/{piece}: legacy key count outside [1, 5]")
    occupied = {int(item["slot"], 16) for item in row["slots"] if int(item["value"], 16)}
    covered = set()
    result = []

    def add(kind, slots, **kwargs):
        slots = [s % MODULUS for s in slots]
        touched = occupied.intersection(slots)
        if touched:
            covered.update(touched)
            result.append((min(slots), operation(kind, dataset, piece, **kwargs), slots))

    keys_head = mapping_slot(piece, mapping_slot(dataset, 14))
    values_base = mapping_slot(piece, mapping_slot(dataset, 13))
    add(2, [keys_head])
    for index, entry in enumerate(keys):
        key = bytes.fromhex(entry["keyHex"].removeprefix("0x"))
        value = bytes.fromhex(entry["valueHex"].removeprefix("0x"))
        if len(key) > 32 or len(value) > 128:
            raise ValueError(f"{dataset}/{piece}: string exceeds cleanup facet's legacy bounds")
        head = int.from_bytes(keccak(key + word(values_base)), "big")
        add(0, [head], key=key)
        if len(value) >= 32:
            count = (len(value) + 31) // 32
            add(1, [data_slot(head) + i for i in range(count)], key=key, count=count)
        element = (data_slot(keys_head) + index) % MODULUS
        add(3, [element], index=index)
        if len(key) == 32:
            add(5, [data_slot(element)], index=index)
    if covered != occupied:
        raise ValueError(f"{dataset}/{piece}: {len(occupied - covered)} unaddressable inventory slots")
    return result


class Batches:
    def __init__(self, stream, max_operations: int, max_writes: int, max_bytes: int):
        self.stream = stream
        self.max_operations, self.max_writes, self.max_bytes = max_operations, max_writes, max_bytes
        self.phase = None
        self.ops, self.slots, self.queues = [], [], []
        self.size = self.writes = self.index = 0
        self.digest = hashlib.sha256()
        self.counts = Counter()
        self.prefix_touches = Counter()
        # Fixed-depth KAMT prefix visits are a locality proxy, not FVM gas.
        self.prefix_loads = {bits: {} for bits in (0, 5, 10, 15)}
        self.metadata_touches = Counter()

    def add(self, phase: str, packed: bytes, slots: list[int], queue=None):
        if len(packed) > self.max_bytes or len(slots) > self.max_writes:
            raise ValueError("one operation exceeds batch limits; increase --max-writes/--max-packed-bytes")
        if self.ops and (phase != self.phase or len(self.ops) == self.max_operations
                         or self.size + len(packed) > self.max_bytes
                         or self.writes + len(slots) > self.max_writes):
            self.flush()
        self.phase = phase
        self.ops.append(packed)
        self.size += len(packed)
        self.writes += len(slots)
        self.slots.extend(slot_hex(s) for s in slots)
        if queue:
            self.queues.append(queue)
        self.counts[str(packed[0])] += 1
        if phase == "metadata":
            for bits, loads in self.prefix_loads.items():
                writes = Counter(s >> (256 - bits) for s in set(slots))
                for prefix, count in writes.items():
                    old = loads.get(prefix, (0, 0, 0))
                    loads[prefix] = (old[0] + 1, old[1] + count, old[2] + len(packed))

    def flush(self):
        if not self.ops:
            return
        slots = sorted(set(self.slots))
        row = {"index": self.index, "phase": self.phase, "operations": len(self.ops),
               "data": calldata(b"".join(self.ops)), "slots": slots, "queues": self.queues}
        raw = (json.dumps(row, separators=(",", ":")) + "\n").encode()
        self.stream.write(raw)
        self.digest.update(raw)
        for bits in (5, 10, 15, 20):
            self.prefix_touches[str(bits)] += len({int(s, 16) >> (256 - bits) for s in slots})
        if self.phase == "metadata":
            for bits in self.prefix_loads:
                self.metadata_touches[str(bits)] += len({int(s, 16) >> (256 - bits) for s in slots})
        self.index += 1
        self.ops, self.slots, self.queues = [], [], []
        self.size = self.writes = 0

    def locality_bound(self):
        result = {}
        for bits, loads in self.prefix_loads.items():
            lower = sum(max((ops + self.max_operations - 1) // self.max_operations,
                            (writes + self.max_writes - 1) // self.max_writes,
                            (size + self.max_bytes - 1) // self.max_bytes)
                        for ops, writes, size in loads.values())
            observed = self.metadata_touches[str(bits)]
            result[str(bits)] = {"lowerBound": lower, "plannedTouches": observed,
                                 "ratio": observed / lower if lower else 1.0}
        return result


def build(inventory: Path, output: Path, *, max_operations=256, max_writes=256,
          max_packed_bytes=16000, queue_pops=128) -> dict:
    if min(max_operations, max_writes, max_packed_bytes, queue_pops) <= 0:
        raise ValueError("batch limits must be positive")
    if queue_pops > 65535 or queue_pops + 1 > max_writes:
        raise ValueError("--queue-pops must be <=65535 and leave one write for the length slot")
    # A fresh directory prevents overwriting a plan that an executor may be using.
    output.mkdir(parents=True, exist_ok=False)
    source = json.loads((inventory / "occupied-slots.json").read_text())
    summary = json.loads((inventory / "summary.json").read_text())
    counts, hashes = Counter(), {}
    db_path = output / "sort.sqlite"
    db = sqlite3.connect(db_path)
    try:
        db.execute("PRAGMA journal_mode=OFF")
        db.execute("PRAGMA synchronous=OFF")
        db.execute("PRAGMA cache_size=-32768")
        db.execute("PRAGMA temp_store=FILE")
        db.execute("CREATE TABLE ops (packed BLOB PRIMARY KEY, anchor BLOB NOT NULL, slots TEXT NOT NULL) WITHOUT ROWID")
        db.execute("CREATE TABLE pieces (dataset INTEGER, piece INTEGER, PRIMARY KEY(dataset,piece)) WITHOUT ROWID")
        digest = hashlib.sha256()
        with (inventory / "pieces.jsonl").open("rb") as stream:
            for line in stream:
                digest.update(line)
                row = json.loads(line)
                db.execute("INSERT INTO pieces VALUES (?,?)", (row["dataSetId"], row["pieceId"]))
                counts["pieces"] += 1
                counts["keys"] += len(row["keys"])
                counts["nonzeroWords"] += len(row["slots"])
                for anchor, packed, slots in piece_operations(row):
                    db.execute("INSERT OR IGNORE INTO ops VALUES (?,?,?)",
                               (packed, word(anchor), json.dumps(slots)))
                if counts["pieces"] % 10000 == 0:
                    db.commit()
        hashes["pieces.jsonl"] = digest.hexdigest()
        for name, key in (("pieces", "piecesWithMetadata"), ("keys", "keys"), ("nonzeroWords", "nonzeroWords")):
            if counts[name] != summary[key]:
                raise ValueError(f"inventory summary mismatch: {name}")
        db.commit()
        db.execute("CREATE INDEX storage_order ON ops(anchor,packed)")
        queues_digest = hashlib.sha256()
        queues = []
        with (inventory / "queues.jsonl").open("rb") as stream:
            for line in stream:
                queues_digest.update(line)
                row = json.loads(line)
                dataset, length = row["dataSetId"], row["length"]
                if not (0 <= dataset < 1 << 40 and length >= 0):
                    raise ValueError("invalid queue dataset or length")
                if int(row["lengthSlot"], 16) != mapping_slot(dataset, 22):
                    raise ValueError("queue length slot mismatch")
                if int(row["dataStart"], 16) != data_slot(mapping_slot(dataset, 22)):
                    raise ValueError("queue data start mismatch")
                if length:
                    queues.append(row)
        if len({q["dataSetId"] for q in queues}) != len(queues):
            raise ValueError("duplicate queue dataset")
        hashes["queues.jsonl"] = queues_digest.hexdigest()
        with (output / "batches.jsonl").open("wb") as stream:
            batches = Batches(stream, max_operations, max_writes, max_packed_bytes)
            for live in (True, False):
                phase = "live-queues" if live else "deleted-queues"
                for queue in sorted((q for q in queues if q["live"] == live),
                                    key=lambda q: q["dataStart"]):
                    length = queue["length"]
                    base = int(queue["dataStart"], 16)
                    while length:
                        count = min(queue_pops, length)
                        slots = [(base + i) % MODULUS for i in range(length - count, length)]
                        length -= count
                        if length == 0:
                            slots.append(int(queue["lengthSlot"], 16))
                        batches.add(phase, operation(4, queue["dataSetId"], count=count), slots,
                                    {"dataSetId": queue["dataSetId"], "lengthSlot": queue["lengthSlot"]})
            for packed, slots in db.execute("SELECT packed,slots FROM ops ORDER BY anchor,packed"):
                batches.add("metadata", packed, json.loads(slots))
            batches.flush()
        manifest = {
            "version": 1, "fwss": source["address"], "source": source,
            "protocol": {"pr": "https://github.com/FilOzone/filecoin-services/pull/658", "commit": PR_COMMIT},
            "batchCount": batches.index, "batchesSha256": batches.digest.hexdigest(),
            "inventorySha256": hashes, "counts": dict(counts),
            "queueDatasets": len(queues), "queueEntries": sum(q["length"] for q in queues),
            "operationCounts": dict(batches.counts),
            "batchPrefixTouches": dict(batches.prefix_touches),
            "metadataLocalityBound": batches.locality_bound(),
            "limits": {"maxOperations": max_operations, "maxWrites": max_writes,
                       "maxPackedBytes": max_packed_bytes, "queuePops": queue_pops},
            "ordering": "live queues first, deleted queues second, independent metadata operations by ascending raw slot; bounded contiguous payload runs stay atomic",
            "coverage": summary.get("coverage", {}),
            "optimality": "Locality heuristic, not a measured gas optimum. Lexicographic order groups every KAMT prefix contiguously; fixed batch boundaries and multi-slot operations can split prefixes.",
        }
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        return manifest
    finally:
        db.close()
        db_path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--max-operations", type=int, default=256)
    parser.add_argument("--max-writes", type=int, default=256)
    parser.add_argument("--max-packed-bytes", type=int, default=16000)
    parser.add_argument("--queue-pops", type=int, default=128)
    args = parser.parse_args()
    manifest = build(args.inventory, args.output, max_operations=args.max_operations,
                     max_writes=args.max_writes, max_packed_bytes=args.max_packed_bytes,
                     queue_pops=args.queue_pops)
    print(json.dumps({"output": str(args.output), "batches": manifest["batchCount"],
                      "counts": manifest["counts"]}))


if __name__ == "__main__":
    main()
