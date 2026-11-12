#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Reconcile every original FWSS word after a completed local cleanup execution.

Target-zero proof is inherited from the executor's final, plan-hash-bound
verification. All original non-target words, all historical queue lengths, the
queue flags (including originally absent words), and the cleanup route are read
at that same explicit canonical block. No transactions are submitted.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import mmap
import os
import subprocess
import sys
import tempfile
import time
from bisect import bisect_right
from collections import Counter
from pathlib import Path
from typing import Any, Iterator

from cleanup_execute import (
    PHASES,
    Plan,
    Rpc,
    address,
    durable_json,
    fingerprint,
    hex_bytes,
    hex_value,
    nonnegative,
)
from cleanup_plan import MODULUS, data_slot, mapping_slot, word


READ_BATCH_SIZE = 128
ZERO_WORD = bytes(32)
PENDING_FLAG = 1 << 192


def sha256_hex(value: Any) -> str:
    if not isinstance(value, str):
        raise ValueError("expected a SHA-256 hex string")
    return hex_bytes(value if value.startswith("0x") else "0x" + value, 32).hex()


class OriginalSlots:
    """Disk-backed binary lookup; no set of the original 24 million slots."""

    def __init__(self, path: Path, manifest: dict[str, Any]) -> None:
        self.path = path
        self.size = path.stat().st_size
        self.count = nonnegative(manifest["records"])
        if manifest["recordBytes"] != 64 or self.size != self.count * 64:
            raise ValueError("occupied-slots.bin size/record count differs from its manifest")
        if nonnegative(manifest["bytes"]) != self.size:
            raise ValueError("occupied-slots.bin bytes differs from its manifest")
        if not self.size:
            raise ValueError("the full FWSS occupied-slot inventory must not be empty")
        self.digest = hashlib.sha256()
        self.file = path.open("rb")
        try:
            self.mapping = mmap.mmap(self.file.fileno(), 0, access=mmap.ACCESS_READ)
        except BaseException:
            self.file.close()
            raise

    def __enter__(self) -> OriginalSlots:
        return self

    def __exit__(self, *_: Any) -> None:
        self.mapping.close()
        self.file.close()

    def lookup(self, slot: bytes) -> bytes:
        low, high = 0, self.count
        while low < high:
            middle = (low + high) // 2
            offset = middle * 64
            candidate = self.mapping[offset:offset + 32]
            if candidate < slot:
                low = middle + 1
            elif candidate > slot:
                high = middle
            else:
                return self.mapping[offset + 32:offset + 64]
        return ZERO_WORD

    def records(self) -> Iterator[tuple[bytes, bytes]]:
        previous = None
        for offset in range(0, self.size, 64 * 16384):
            chunk = self.mapping[offset:min(offset + 64 * 16384, self.size)]
            self.digest.update(chunk)
            for index in range(0, len(chunk), 64):
                slot, value = chunk[index:index + 32], chunk[index + 32:index + 64]
                if previous is not None and slot <= previous:
                    raise ValueError(f"original inventory is not strictly slot-sorted at {hex_value(slot)}")
                if value == ZERO_WORD:
                    raise ValueError(f"original occupied inventory contains a zero word at {hex_value(slot)}")
                previous = slot
                yield slot, value


class Queues:
    def __init__(self, path: Path, source: dict[str, Any], plan: dict[str, Any], original: OriginalSlots) -> None:
        self.length_slots: list[bytes] = []
        self.nonempty: dict[int, dict[str, Any]] = {}
        self.info_words: dict[bytes, bytes] = {}
        self.length_heads: dict[bytes, int] = {}
        self.flag_details: list[dict[str, Any]] = []
        self.entries = 0
        self.live = 0
        self.changed_flags = 0
        self.absent_infos = 0
        spans: list[tuple[bytes, bytes]] = []
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for expected_dataset, line in enumerate(stream, 1):
                digest.update(line)
                row = json.loads(line)
                dataset, length = nonnegative(row["dataSetId"]), nonnegative(row["length"])
                if dataset != expected_dataset or dataset >= 1 << 40 or length >= MODULUS:
                    raise ValueError("queues.jsonl must cover each historical dataset consecutively from 1")
                if type(row["live"]) is not bool:
                    raise ValueError(f"dataset {dataset}: live must be boolean")
                head = mapping_slot(dataset, 22)
                start = data_slot(head)
                length_slot = hex_bytes(row["lengthSlot"], 32)
                if length_slot != word(head) or hex_bytes(row["dataStart"], 32) != word(start):
                    raise ValueError(f"dataset {dataset}: queue storage locations differ from the mapping layout")
                if original.lookup(length_slot) != word(length):
                    raise ValueError(f"dataset {dataset}: original queue length differs from queues.jsonl")
                if row["live"] != (original.lookup(word(mapping_slot(dataset, 7))) != ZERO_WORD):
                    raise ValueError(f"dataset {dataset}: live differs from the original full pdpRailId word")
                self.length_slots.append(length_slot)
                if not length:
                    continue
                self.nonempty[dataset] = row
                self.length_heads[length_slot] = dataset
                self.entries += length
                stop = start + length - 1
                if stop < MODULUS:
                    spans.append((word(start), word(stop)))
                else:
                    spans.extend(((word(start), word(MODULUS - 1)), (ZERO_WORD, word(stop % MODULUS))))
                info_slot = word((mapping_slot(dataset, 7) + 10) % MODULUS)
                before = original.lookup(info_slot)
                after = word(int.from_bytes(before, "big") | PENDING_FLAG) if row["live"] else before
                if info_slot in self.info_words:
                    raise ValueError("queue info slots collide")
                self.info_words[info_slot] = after
                self.live += int(row["live"])
                self.changed_flags += int(before != after)
                self.absent_infos += int(before == ZERO_WORD)
                self.flag_details.append({
                    "dataSetId": dataset, "live": row["live"], "slot": hex_value(info_slot),
                    "originalWord": hex_value(before), "expectedWord": hex_value(after),
                })
        self.sha256 = digest.hexdigest()
        if self.sha256 != sha256_hex(plan["inventorySha256"]["queues.jsonl"]):
            raise ValueError("queues.jsonl SHA-256 differs from the executed plan")
        if len(self.length_slots) != nonnegative(source["queueDataSetCount"]):
            raise ValueError("queue inventory count differs from the source manifest")
        if len(self.length_slots) != nonnegative(source["nextDataSetId"]) - 1:
            raise ValueError("queue inventory does not cover every historical dataset ID")
        if len(self.nonempty) != nonnegative(plan["queueDatasets"]) or self.entries != nonnegative(plan["queueEntries"]):
            raise ValueError("nonempty queue count/entries differ from the executed plan")
        spans.sort()
        for previous, current in zip(spans, spans[1:]):
            if previous[1] >= current[0]:
                raise ValueError("original queue payload storage ranges overlap")
        self.starts = [start for start, _ in spans]
        self.ends = [end for _, end in spans]
        if any(self.payload_contains(head) for head in self.length_heads):
            raise ValueError("queue payload overlaps a queue length slot")

    def payload_contains(self, slot: bytes) -> bool:
        index = bisect_right(self.starts, slot) - 1
        return index >= 0 and slot <= self.ends[index]

    def contains(self, slot: bytes) -> bool:
        return slot in self.length_heads or self.payload_contains(slot)


def sort_targets(run: Path, manifest: dict[str, Any], queues: Queues, directory: Path) -> tuple[Path, dict[str, int]]:
    """Stream target references directly to bounded-memory external GNU sort -u."""
    target_path = directory / "targets.hex"
    environment = dict(os.environ, LC_ALL="C")
    print("Streaming every plan target into external GNU sort -u", file=sys.stderr, flush=True)
    counts: Counter[str] = Counter()
    digest = hashlib.sha256()
    queue_datasets: dict[int, int] = {}
    phase_index = 0
    process = subprocess.Popen(
        ["sort", "-u", "--buffer-size=64M", "--parallel=1", "--temporary-directory=" + str(directory),
         "--output=" + str(target_path)],
        stdin=subprocess.PIPE, env=environment,
    )
    try:
        assert process.stdin is not None
        with (run / "plan" / "batches.jsonl").open("rb") as stream:
            for index, line in enumerate(stream):
                digest.update(line)
                batch = Plan.parse_batch(line, index)
                current_phase = PHASES.index(batch["phase"])
                if current_phase < phase_index:
                    raise ValueError("executed plan phases are not ordered")
                phase_index = current_phase
                if batch["phase"] != "metadata" and not batch["queues"]:
                    raise ValueError(f"batch {index}: queue phase has no queue descriptors")
                for queue in batch["queues"]:
                    dataset = nonnegative(queue["dataSetId"])
                    original = queues.nonempty.get(dataset)
                    if original is None or hex_bytes(queue["lengthSlot"], 32) != hex_bytes(original["lengthSlot"], 32):
                        raise ValueError(f"batch {index}: queue does not match the original inventory")
                    expected_phase = "live-queues" if original["live"] else "deleted-queues"
                    if batch["phase"] != expected_phase:
                        raise ValueError(f"batch {index}: queue is in the wrong live/deleted phase")
                    queue_datasets[dataset] = queue_datasets.get(dataset, 0) + 1
                    counts["queueChecks"] += 1
                process.stdin.write(b"".join(slot[2:].encode("ascii") + b"\n" for slot in batch["slots"]))
                counts["slotChecks"] += len(batch["slots"])
                counts["batches"] += 1
                if counts["batches"] % 5000 == 0:
                    print(f"Sorted input: {counts['batches']:,} batches, {counts['slotChecks']:,} target references", file=sys.stderr, flush=True)
        process.stdin.close()
        if process.wait() != 0:
            raise RuntimeError("external GNU sort -u failed")
    except BaseException:
        if process.stdin is not None and not process.stdin.closed:
            try:
                process.stdin.close()
            except BrokenPipeError:
                pass
        if process.poll() is None:
            process.terminate()
        process.wait()
        raise
    if digest.hexdigest() != sha256_hex(manifest["batchesSha256"]):
        raise ValueError("batches.jsonl SHA-256 differs from the executed plan")
    if counts["batches"] != nonnegative(manifest["batchCount"]):
        raise ValueError("executed plan batchCount differs from batches.jsonl")
    if len(queue_datasets) != len(queues.nonempty):
        raise ValueError("executed plan omits an originally nonempty queue")
    return target_path, dict(counts)


def sorted_targets(path: Path) -> Iterator[bytes]:
    previous = None
    with path.open("rb") as stream:
        for line in stream:
            if len(line) != 65 or line[-1:] != b"\n":
                raise ValueError("sorted target file does not contain hex64 lines")
            slot = bytes.fromhex(line[:-1].decode("ascii"))
            if len(slot) != 32 or (previous is not None and slot <= previous):
                raise ValueError("external target sort is not strictly unique and ascending")
            previous = slot
            yield slot


class Reader:
    def __init__(self, rpc: Rpc, fwss: str, block: str) -> None:
        self.rpc, self.fwss, self.block = rpc, fwss, block
        self.checked = 0

    def check(self, words: list[tuple[bytes, bytes]], label: str) -> None:
        for offset in range(0, len(words), READ_BATCH_SIZE):
            chunk = words[offset:offset + READ_BATCH_SIZE]
            values = self.rpc.batch([
                ("eth_getStorageAt", [self.fwss, hex(int.from_bytes(slot, "big")), self.block])
                for slot, _ in chunk
            ])
            for (slot, expected), value in zip(chunk, values, strict=True):
                actual = hex_bytes(value, 32)
                if actual != expected:
                    raise RuntimeError(f"{label}: slot {hex_value(slot)} is {hex_value(actual)}, expected {hex_value(expected)} at block {self.block}")
                self.checked += 1


def assert_canonical(rpc: Rpc, number: int, expected_hash: bytes) -> None:
    # The head must remain the executor's final block, not merely contain it.
    # Storage reads below always use the explicit minimal-hex quantity.
    if nonnegative(rpc.call("eth_chainId")) != 31337:
        raise RuntimeError("reconciliation is restricted to local chain ID 31337")
    if nonnegative(rpc.call("eth_blockNumber")) != number:
        raise RuntimeError("chain head moved after execution; keep the node quiet during reconciliation")
    block = rpc.call("eth_getBlockByNumber", hex(number), False)
    if not isinstance(block, dict) or nonnegative(block["number"]) != number or hex_bytes(block["hash"], 32) != expected_hash:
        raise RuntimeError("executor final verification block is no longer canonical")


def merge_original(original: OriginalSlots, target_path: Path, queues: Queues, route_slot: bytes,
                   route_word: bytes, reader: Reader) -> dict[str, int]:
    counts: Counter[str] = Counter()
    pending: list[tuple[bytes, bytes]] = []
    targets = sorted_targets(target_path)

    def next_target() -> bytes | None:
        slot = next(targets, None)
        if slot is not None:
            if slot == route_slot or slot in queues.info_words:
                raise ValueError(f"plan zero target overlaps an intentional nonzero setup/queue-info slot: {hex_value(slot)}")
            counts["uniqueTargetWords"] += 1
            if queues.contains(slot):
                counts["queueTargetWords"] += 1
        return slot

    target = next_target()
    print(f"Reconciling {original.count:,} original occupied words at explicit block {reader.block}", file=sys.stderr, flush=True)
    try:
        for slot, before in original.records():
            counts["originalOccupiedWords"] += 1
            while target is not None and target < slot:
                counts["originallyAbsentTargetWords"] += 1
                target = next_target()
            if slot == target:
                counts["originalTargetWords"] += 1
                counts["originalQueueTargetWords" if queues.contains(slot) else "originalMetadataTargetWords"] += 1
                target = next_target()
            else:
                expected = route_word if slot == route_slot else queues.info_words.get(slot, before)
                pending.append((slot, expected))
                if len(pending) == READ_BATCH_SIZE:
                    reader.check(pending, "original non-target reconciliation")
                    counts["nonTargetChecked"] += len(pending)
                    pending.clear()
            if counts["originalOccupiedWords"] % 1000000 == 0:
                print(f"Accounted {counts['originalOccupiedWords']:,}/{original.count:,} occupied words; read {counts['nonTargetChecked']:,} non-target words", file=sys.stderr, flush=True)
        while target is not None:
            counts["originallyAbsentTargetWords"] += 1
            target = next_target()
        if pending:
            reader.check(pending, "original non-target reconciliation")
            counts["nonTargetChecked"] += len(pending)
    finally:
        targets.close()
    if counts["originalOccupiedWords"] != original.count:
        raise ValueError("original occupied-word coverage is incomplete")
    if counts["originalTargetWords"] + counts["nonTargetChecked"] != original.count:
        raise ValueError("not every original occupied word was accounted for exactly once")
    if counts["originalTargetWords"] + counts["originallyAbsentTargetWords"] != counts["uniqueTargetWords"]:
        raise ValueError("unique target merge membership does not reconcile")
    if counts["queueTargetWords"] != queues.entries + len(queues.nonempty):
        raise ValueError("plan does not cover every original queue element and nonempty length slot")
    return {name: counts[name] for name in (
        "originalOccupiedWords", "originalTargetWords", "nonTargetChecked", "uniqueTargetWords",
        "originallyAbsentTargetWords", "queueTargetWords", "originalQueueTargetWords", "originalMetadataTargetWords",
    )}


def reconcile(args: argparse.Namespace) -> dict[str, Any]:
    started = time.monotonic()
    run, inventory = args.run.resolve(), args.inventory.resolve()
    output = run / "reconcile.json"
    if output.exists():
        raise FileExistsError(f"refusing to overwrite immutable reconciliation result: {output}")
    paths = {
        "execution": run / "execution.json", "setup": run / "setup.json",
        "plan": run / "plan" / "manifest.json", "batches": run / "plan" / "batches.jsonl",
        "inventory": inventory / "occupied-slots.json", "occupied": inventory / "occupied-slots.bin",
        "queues": inventory / "queues.jsonl",
    }
    identities = {path: fingerprint(path) for path in paths.values()}
    payloads = {name: paths[name].read_bytes() for name in ("execution", "setup", "plan", "inventory")}
    execution, setup, manifest, source = (json.loads(payloads[name]) for name in ("execution", "setup", "plan", "inventory"))
    plan_hash = hashlib.sha256(payloads["plan"]).hexdigest()
    if manifest.get("version") != 1 or manifest["source"] != source:
        raise ValueError("run plan does not bind this exact original inventory manifest")
    if execution.get("status") != "verified" or nonnegative(execution["chainId"]) != 31337:
        raise ValueError("execution.json must record verified execution on chain ID 31337")
    if sha256_hex(execution["planSha256"]) != plan_hash:
        raise ValueError("execution final target proof is for a different plan")
    if nonnegative(execution["batchCount"]) != nonnegative(manifest["batchCount"]):
        raise ValueError("execution batchCount differs from the committed plan")
    fwss = address(manifest["fwss"])
    if any(address(value) != fwss for value in (source["address"], execution["fwss"], setup["fwss"])):
        raise ValueError("setup, execution, and original inventory must identify the same FWSS proxy")
    if hex_bytes(execution["facetCodeHash"], 32) != hex_bytes(setup["facetCodeHash"], 32):
        raise ValueError("executed cleanup facet differs from setup")
    source_plan_hash = sha256_hex(setup["sourcePlanSha256"])
    if sha256_hex(manifest["sourcePlanSha256"]) != source_plan_hash:
        raise ValueError("setup source plan differs from rebatching provenance")
    final = execution["verification"]
    number, block_hash = nonnegative(final["blockNumber"]), hex_bytes(final["blockHash"], 32)
    route_slot = hex_bytes(setup["routeSlot"], 32)
    facet = address(setup["facet"])
    route_word = bytes(12) + hex_bytes(facet, 20)
    rpc = Rpc(args.rpc_url, 120.0)
    assert_canonical(rpc, number, block_hash)
    reader = Reader(rpc, fwss, hex(number))
    with OriginalSlots(paths["occupied"], source) as original:
        queues = Queues(paths["queues"], source, manifest, original)
        if route_slot in queues.info_words:
            raise ValueError("cleanup route collides with a queue info word")
        route_before = original.lookup(route_slot)
        with tempfile.TemporaryDirectory(prefix=".cleanup-reconcile-", dir=run) as temporary:
            target_path, plan_counts = sort_targets(run, manifest, queues, Path(temporary))
            if nonnegative(final["slotChecks"]) != plan_counts["slotChecks"] or nonnegative(final["queueChecks"]) != plan_counts.get("queueChecks", 0):
                raise ValueError("executor final target/queue slot checks do not cover the entire committed plan")
            counts = merge_original(original, target_path, queues, route_slot, route_word, reader)
        expected_metadata = nonnegative(manifest["counts"]["nonzeroWords"])
        if counts["originalMetadataTargetWords"] != expected_metadata:
            raise ValueError("original metadata target-word membership differs from source-plan nonzeroWords")
        if counts["originalTargetWords"] != expected_metadata + counts["originalQueueTargetWords"]:
            raise ValueError("original target-word count does not reconcile with metadata plus queue words")
        print(f"Checking all {len(queues.length_slots):,} historical queue lengths, {len(queues.info_words):,} queue info words, and the cleanup route", file=sys.stderr, flush=True)
        reader.check([(slot, ZERO_WORD) for slot in queues.length_slots], "all historical queues must be empty")
        reader.check(list(queues.info_words.items()), "queue flag must preserve every other packed byte")
        reader.check([(route_slot, route_word)], "cleanup selector route")
        occupied_hash = original.digest.hexdigest()
    for path, expected in identities.items():
        if fingerprint(path) != expected:
            raise RuntimeError(f"immutable reconciliation input changed during use: {path}")
    assert_canonical(rpc, number, block_hash)
    report = {
        "status": "verified", "chainId": 31337, "fwss": fwss,
        "blockNumber": number, "blockHash": hex_value(block_hash),
        "planSha256": plan_hash, "batchesSha256": sha256_hex(manifest["batchesSha256"]),
        "sourcePlanSha256": source_plan_hash,
        "executionSha256": hashlib.sha256(payloads["execution"]).hexdigest(),
        "setupSha256": hashlib.sha256(payloads["setup"]).hexdigest(),
        "inventoryManifestSha256": hashlib.sha256(payloads["inventory"]).hexdigest(),
        "occupiedSlotsSha256": occupied_hash, "queuesSha256": queues.sha256,
        **counts,
        "zeroTargetWords": counts["uniqueTargetWords"],
        "sourcePlanNonzeroMetadataWords": expected_metadata,
        "originallyAbsentQueueTargetWords": counts["queueTargetWords"] - counts["originalQueueTargetWords"],
        "expectedFlags": queues.live, "changedLiveFlags": queues.changed_flags,
        "queueInfoChecks": len(queues.info_words), "originallyAbsentInfoWords": queues.absent_infos,
        "queuesEmpty": len(queues.length_slots), "originallyEmptyQueues": len(queues.length_slots) - len(queues.nonempty),
        "nonemptyQueueDatasets": len(queues.nonempty), "queueEntries": queues.entries,
        "storageReads": reader.checked,
        "targetProof": {
            "method": "inherited-execution-final-verification", "planSha256": plan_hash,
            "executionSha256": hashlib.sha256(payloads["execution"]).hexdigest(),
            "verification": final, "metadataTargetWordsReread": 0,
            "queueLengthTargetsRechecked": len(queues.nonempty),
            "scope": "All committed plan targets were zero at the same canonical block; this reconciler proves membership and reads every original non-target word, not a second pass over metadata targets.",
        },
        "queueFlags": queues.flag_details,
        "route": {"slot": hex_value(route_slot), "facet": facet,
                  "originalWord": hex_value(route_before), "expectedWord": hex_value(route_word)},
        "source": {key: source[key] for key in (
            "archive", "archiveMetadataRoot", "sourceEpoch", "sourceStateRoot", "preparedStateRoot",
            "sourceActorState", "storageRoot", "scope", "records",
        ) if key in source},
        "coverage": {
            "originalState": "Every original occupied FWSS word is accounted for exactly once as a verified zero target or a directly checked non-target; no FWSS roots/top-level slots are excluded.",
            "inventory": source.get("coverage"), "plan": manifest.get("coverage", {}),
            "historicalCompleteness": "Not established: unknown historical piece bounds and erased mapping-key preimages remain unknown. This proves the committed inventory cleanup and original-state preservation, not that every historical metadata value was discoverable.",
        },
        "elapsedSeconds": time.monotonic() - started,
    }
    durable_json(output, report)
    print(f"Reconciled {counts['originalOccupiedWords']:,} original words; {counts['originalTargetWords']:,} inherited zero targets, {counts['nonTargetChecked']:,} non-target reads; all {len(queues.length_slots):,} queues empty", file=sys.stderr, flush=True)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", type=Path, required=True, help="completed full run directory")
    parser.add_argument("--inventory", type=Path, required=True, help="immutable pre-cleanup inventory directory")
    parser.add_argument("--rpc-url", required=True, help="quiet local chain 31337 JSON-RPC endpoint")
    args = parser.parse_args()
    try:
        report = reconcile(args)
        print(json.dumps({"status": report["status"], "output": str(args.run / "reconcile.json"),
                          "originalOccupiedWords": report["originalOccupiedWords"],
                          "nonTargetChecked": report["nonTargetChecked"], "queuesEmpty": report["queuesEmpty"]}))
        return 0
    except KeyboardInterrupt:
        print("Reconciliation interrupted; temporary sort files removed, no result written", file=sys.stderr)
        return 130
    except (OSError, RuntimeError, ValueError, KeyError, TypeError) as error:
        print(f"Reconciliation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
