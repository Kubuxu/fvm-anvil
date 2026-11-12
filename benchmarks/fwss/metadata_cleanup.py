#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Lab-only comparison of historical FWSS metadata cleanup traversal orders.

This runner never upgrades FWSS.  It temporarily replaces the imported FWSS proxy
runtime with a locally deployed, operator-bound cleanup contract via anvil_setCode,
then restores the exact imported snapshot in a finally block.
"""

from __future__ import annotations

import argparse
import hashlib
import heapq
import json
import os
import sqlite3
import sys
import time
from collections.abc import Iterable
from pathlib import Path
from typing import Any

from chain import Chain, command, quantity
from traces import summarize_trace

DEFAULT_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
FWSS_PROXY = "0x8408502033C418E1bbC97cE9ac48E5528F371A9f"
GAS_LIMIT = 10_000_000_000
SCHEMA_VERSION = 1


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def csv_positive(value: str) -> list[int]:
    values = [int(item) for item in value.split(",")]
    if not values or any(item <= 0 for item in values):
        raise argparse.ArgumentTypeError("must be comma-separated positive integers")
    return values


def hex_word(value: str, name: str) -> str:
    if not isinstance(value, str) or not value.startswith("0x") or len(value) != 66:
        raise ValueError(f"{name} must be a 32-byte 0x hex word: {value!r}")
    int(value, 16)
    return value.lower()


def solidity_array(values: Iterable[int | str]) -> str:
    return "[" + ",".join(str(value) for value in values) + "]"


def range_batches(
    ranges: list[tuple[int, int, int]], piece_limit: int
) -> Iterable[list[tuple[int, int, int]]]:
    """Split natural scan ranges by pieces, never reordering or independently sorting."""
    batch: list[tuple[int, int, int]] = []
    remaining = piece_limit
    for dataset, first, end in ranges:
        cursor = first
        while cursor < end:
            take = min(end - cursor, remaining)
            batch.append((dataset, cursor, cursor + take))
            cursor += take
            remaining -= take
            if remaining == 0:
                yield batch
                batch = []
                remaining = piece_limit
    if batch:
        yield batch


class Inventory:
    """Disk-backed prefix selection; do not load a mainnet word inventory into RAM."""

    def __init__(self, directory: Path, max_pieces: int | None) -> None:
        started = time.monotonic()
        self.directory = directory
        self.summary_path = directory / "summary.json"
        self.pieces_path = directory / "pieces.jsonl"
        if not self.summary_path.is_file() or not self.pieces_path.is_file():
            raise RuntimeError(
                "inventory directory must contain summary.json and pieces.jsonl"
            )
        self.summary = json.loads(self.summary_path.read_text())
        self.max_pieces = max_pieces
        self.db_path = directory / ".metadata-cleanup-selection.sqlite"
        self.connection = sqlite3.connect(self.db_path)
        self.connection.execute("PRAGMA journal_mode=OFF")
        self.connection.execute("PRAGMA synchronous=OFF")
        self.connection.execute(
            "CREATE TABLE selected (dataset INTEGER NOT NULL, piece INTEGER NOT NULL, keys TEXT NOT NULL, slots TEXT NOT NULL, PRIMARY KEY(dataset, piece))"
        )
        self.connection.execute(
            "CREATE TABLE words (position INTEGER NOT NULL UNIQUE, slot TEXT NOT NULL PRIMARY KEY)"
        )
        self._ingest_prefix()
        self.planning_seconds = time.monotonic() - started

    def close(self) -> None:
        self.connection.close()
        self.db_path.unlink(missing_ok=True)

    def _ingest_prefix(self) -> None:
        selected = 0
        last: tuple[int, int] | None = None
        with self.pieces_path.open() as source, self.connection:
            for line_number, line in enumerate(source, 1):
                if self.max_pieces is not None and selected >= self.max_pieces:
                    break
                row = json.loads(line)
                dataset, piece = row.get("dataSetId"), row.get("pieceId")
                if (
                    not isinstance(dataset, int)
                    or not isinstance(piece, int)
                    or dataset < 0
                    or piece < 0
                ):
                    raise RuntimeError(
                        f"pieces.jsonl line {line_number}: invalid dataSetId/pieceId"
                    )
                if last is not None and (dataset, piece) <= last:
                    raise RuntimeError(
                        "pieces.jsonl must be strictly natural dataset,piece order"
                    )
                last = (dataset, piece)
                keys = row.get("keys")
                slots = row.get("slots")
                if not isinstance(keys, list) or not isinstance(slots, list):
                    raise TypeError(
                        f"pieces.jsonl line {line_number}: keys and slots must be lists"
                    )
                normalized_slots: list[dict[str, str]] = []
                seen: set[str] = set()
                for entry in slots:
                    slot = hex_word(entry.get("slot"), "slot")
                    value = hex_word(entry.get("value"), "value")
                    if value == "0x" + "00" * 32:
                        raise RuntimeError(
                            f"pieces.jsonl line {line_number}: inventory includes zero word"
                        )
                    if slot in seen:
                        raise RuntimeError(
                            f"pieces.jsonl line {line_number}: duplicate slot"
                        )
                    seen.add(slot)
                    normalized_slots.append({"slot": slot, "value": value})
                for entry in keys:
                    if not isinstance(entry, dict) or not isinstance(
                        entry.get("keyHex"), str
                    ):
                        raise TypeError(
                            f"pieces.jsonl line {line_number}: malformed key"
                        )
                self.connection.execute(
                    "INSERT INTO selected VALUES (?, ?, ?, ?)",
                    (dataset, piece, json.dumps(keys), json.dumps(normalized_slots)),
                )
                for position, entry in enumerate(normalized_slots):
                    self.connection.execute(
                        "INSERT INTO words VALUES (?, ?)",
                        ((selected << 32) + position, entry["slot"]),
                    )
                selected += 1
        if selected == 0:
            raise RuntimeError(
                "selected inventory contains no historical metadata pieces"
            )

    def rows(
        self,
    ) -> Iterable[tuple[int, int, list[dict[str, str]], list[dict[str, str]]]]:
        for dataset, piece, keys, slots in self.connection.execute(
            "SELECT dataset, piece, keys, slots FROM selected ORDER BY dataset, piece"
        ):
            yield dataset, piece, json.loads(keys), json.loads(slots)

    def word_count(self) -> int:
        return int(self.connection.execute("SELECT COUNT(*) FROM words").fetchone()[0])

    def word_digest(self) -> str:
        digest = hashlib.sha256()
        for (slot,) in self.connection.execute("SELECT slot FROM words ORDER BY slot"):
            digest.update(bytes.fromhex(slot[2:]))
        return digest.hexdigest()

    def word_batches(self, size: int, *, globally_sorted: bool) -> Iterable[list[str]]:
        order = "slot" if globally_sorted else "position"
        batch: list[str] = []
        for (slot,) in self.connection.execute(
            f"SELECT slot FROM words ORDER BY {order}"
        ):
            batch.append(slot)
            if len(batch) == size:
                yield batch
                batch = []
        if batch:
            yield batch

    def ranges(self) -> list[tuple[int, int, int]]:
        """Contiguous scan ranges covering the selected natural prefix, including empty pieces."""
        output: list[tuple[int, int, int]] = []
        current_dataset: int | None = None
        first = end = 0
        for dataset, piece, _, _ in self.rows():
            if dataset != current_dataset:
                if current_dataset is not None:
                    output.append((current_dataset, first, end))
                current_dataset, first, end = dataset, 0, piece + 1
            else:
                end = piece + 1
        assert current_dataset is not None
        output.append((current_dataset, first, end))
        return output

    def metadata_batches(self, size: int) -> Iterable[list[tuple[int, int]]]:
        cursor = self.connection.execute(
            "SELECT dataset, piece FROM selected ORDER BY dataset, piece"
        )
        while batch := cursor.fetchmany(size):
            yield batch

    def selected_piece_count(self) -> int:
        return int(
            self.connection.execute("SELECT COUNT(*) FROM selected").fetchone()[0]
        )


class Runner:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.contracts = Path(__file__).with_name("contracts")
        self.output = args.output.resolve()
        self.inventory = Inventory(args.inventory.resolve(), args.max_pieces)
        self.chain = Chain(args.rpc_url, args.private_key, GAS_LIMIT)
        self.proxy = args.fwss_proxy
        self.records: dict[str, Any] = {}
        self.initial_snapshot: str | None = None
        self.baseline_snapshot: str | None = None
        self.original_code: str | None = None
        self.initial_sentinel_values: dict[str, str] = {}
        self.sentinels = [
            hex_word(slot, "sentinel slot") for slot in args.sentinel_slot
        ]

    def compile_and_install(self, ranges: list[tuple[int, int, int]]) -> dict[str, Any]:
        command(["forge", "build", "--root", str(self.contracts)])
        creation_code = command(
            [
                "forge",
                "inspect",
                "MetadataCleanup",
                "bytecode",
                "--root",
                str(self.contracts),
            ]
        )
        self.original_code = self.chain.rpc("eth_getCode", self.proxy, "latest")
        if self.original_code == "0x":
            raise RuntimeError(f"FWSS proxy {self.proxy} has no imported runtime")
        # This precedes the helper deployment as well as anvil_setCode.
        self.initial_snapshot = self.chain.snapshot()
        deployment, receipt = self.chain.deploy(
            creation_code
            + self.chain.abi_encode("constructor(address)", self.chain.sender)[2:]
        )
        runtime = self.chain.rpc("eth_getCode", deployment, "latest")
        if runtime == "0x":
            raise RuntimeError("locally deployed cleanup contract has no runtime")
        self.chain.rpc("anvil_setCode", self.proxy, runtime)
        if self.chain.rpc("eth_getCode", self.proxy, "latest") != runtime:
            raise RuntimeError(
                "anvil_setCode did not install the deployed cleanup runtime"
            )
        self._verify_inventory_words()
        dataset_metadata_sentinel = self._add_dataset_metadata_sentinel(ranges)
        self.initial_sentinel_values = {
            slot: self.chain.rpc(
                "eth_getStorageAt", self.proxy, hex(int(slot, 16)), "latest"
            ).lower()
            for slot in self.sentinels
        }
        self.baseline_snapshot = self.chain.snapshot()
        return {
            "deployment": deployment,
            "deploymentReceipt": receipt,
            "originalCodeSha256": hashlib.sha256(
                bytes.fromhex(self.original_code[2:])
            ).hexdigest(),
            "labRuntimeSha256": hashlib.sha256(bytes.fromhex(runtime[2:])).hexdigest(),
            "datasetMetadataSentinel": dataset_metadata_sentinel,
        }

    def _verify_inventory_words(self) -> None:
        batch: list[dict[str, str]] = []
        for _, _, _, words in self.inventory.rows():
            batch.extend(words)
            if len(batch) >= 128:
                self._check_inventory_batch(batch)
                batch.clear()
        if batch:
            self._check_inventory_batch(batch)

    def _check_inventory_batch(self, batch: list[dict[str, str]]) -> None:
        matches = self.chain.call(
            self.proxy,
            "slotsMatch(bytes32[],bytes32[])(bool)",
            solidity_array(word["slot"] for word in batch),
            solidity_array(word["value"] for word in batch),
        )
        if matches.strip().lower() != "true":
            raise RuntimeError("inventory words do not match the imported FWSS storage")

    def _add_dataset_metadata_sentinel(
        self, ranges: list[tuple[int, int, int]]
    ) -> dict[str, str]:
        datasets = dict.fromkeys(dataset for dataset, _, _ in ranges)
        for dataset in range(1, self.inventory.summary.get("pdpNextDataSetId", 1)):
            datasets.setdefault(dataset, None)
        for dataset in datasets:
            response = self.chain.call_json(
                self.proxy,
                "dataSetMetadataKeysSlot(uint256)(bytes32,bytes32)",
                str(dataset),
            )
            if not isinstance(response, list) or len(response) != 2:
                raise RuntimeError(
                    f"unexpected dataset metadata sentinel response: {response!r}"
                )
            slot, value = (
                hex_word(item, "dataset metadata sentinel") for item in response
            )
            if value != "0x" + "00" * 32:
                if slot not in self.sentinels:
                    self.sentinels.append(slot)
                key_slot = self.chain.cast("keccak", slot)
                if key_slot not in self.sentinels:
                    self.sentinels.append(key_slot)
                return {"dataSetId": str(dataset), "slot": slot, "value": value}
        raise RuntimeError("archive has no nonempty root-12 dataset metadata sentinel")

    def reset_variant(self) -> None:
        assert self.baseline_snapshot is not None
        self.chain.revert(self.baseline_snapshot)
        # anvil snapshots are invalidated by revert; retain an equivalent new checkpoint.
        self.baseline_snapshot = self.chain.snapshot()

    @staticmethod
    def _ipld_charges(trace_summary: dict[str, Any]) -> dict[str, dict[str, int]]:
        """Retain actual trace charge names instead of assuming a node charge schema."""
        return {
            name: charge
            for name, charge in trace_summary["chargesByName"].items()
            if name.startswith("OnBlock") or "ipld" in name.lower()
        }

    @staticmethod
    def _sum_ipld_charges(
        transactions: list[dict[str, Any]],
    ) -> dict[str, dict[str, int]]:
        total: dict[str, dict[str, int]] = {}
        for transaction in transactions:
            for name, charge in transaction["ipldCharges"].items():
                destination = total.setdefault(
                    name,
                    {
                        "count": 0,
                        "computeMilligas": 0,
                        "otherMilligas": 0,
                        "totalMilligas": 0,
                    },
                )
                for field in destination:
                    destination[field] += charge[field]
        return total

    def _transaction(
        self, name: str, signature: str, arguments: list[str]
    ) -> dict[str, Any]:
        submission = self.chain.submit(
            to=self.proxy, signature=signature, arguments=arguments
        )
        record: dict[str, Any] = {
            "name": name,
            "signature": signature,
            "arguments": arguments,
            "transactionHash": submission.transaction_hash,
            "receipt": submission.receipt,
            "rejection": submission.rejection,
        }
        if submission.rejection is not None or submission.receipt is None:
            atomic_json(self.output / f"{name}.json", record)
            raise RuntimeError(f"{name} rejected: {json.dumps(record)}")
        trace = self.chain.trace(submission.transaction_hash or "")
        trace_summary = summarize_trace(trace, {self.proxy: "metadata-cleanup"})
        record["trace"] = trace
        record["traceSummary"] = trace_summary
        record["ipldCharges"] = self._ipld_charges(trace_summary)
        atomic_json(self.output / f"{name}.json", record)
        del record["trace"]
        record["traceFile"] = f"{name}.json"
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(
                f"{name} reverted; receipt and trace saved to {name}.json"
            )
        return record

    def _verify(self, variant: str) -> dict[str, Any]:
        for batch in self.inventory.word_batches(
            self.args.word_batch_sizes[-1], globally_sorted=False
        ):
            if (
                self.chain.call(
                    self.proxy, "slotsAreZero(bytes32[])(bool)", solidity_array(batch)
                )
                .strip()
                .lower()
                != "true"
            ):
                raise RuntimeError(f"{variant}: targeted word remained nonzero")
        for batch in self.inventory.metadata_batches(128):
            datasets, pieces = zip(*batch)
            if (
                self.chain.call(
                    self.proxy,
                    "metadataKeysEmpty(uint256[],uint256[])(bool)",
                    solidity_array(datasets),
                    solidity_array(pieces),
                )
                .strip()
                .lower()
                != "true"
            ):
                raise RuntimeError(f"{variant}: metadata key arrays not empty")
        changed_sentinels: list[dict[str, str]] = []
        for slot, expected in self._sentinel_before.items():
            actual = self.chain.rpc(
                "eth_getStorageAt", self.proxy, hex(int(slot, 16)), "latest"
            ).lower()
            if actual != expected:
                changed_sentinels.append(
                    {"slot": slot, "expected": expected, "actual": actual}
                )
        if changed_sentinels:
            raise RuntimeError(
                f"{variant}: non-target sentinel changed: {changed_sentinels}"
            )
        return {
            "targetedWordsZero": self.inventory.word_count(),
            "metadataKeysEmpty": self.inventory.selected_piece_count(),
            "sentinels": self._sentinel_before,
        }

    def _run_variant(
        self, name: str, mode: str, batch_size: int, ranges: list[tuple[int, int, int]]
    ) -> dict[str, Any]:
        self.reset_variant()
        self._sentinel_before = {
            slot: self.chain.rpc(
                "eth_getStorageAt", self.proxy, hex(int(slot, 16)), "latest"
            ).lower()
            for slot in self.sentinels
        }
        transactions: list[dict[str, Any]] = []
        if mode == "natural":
            for number, range_batch in enumerate(range_batches(ranges, batch_size)):
                datasets, first, end = zip(*range_batch)
                transactions.append(
                    self._transaction(
                        f"{name}-{number}",
                        "deleteNatural(uint256[],uint256[],uint256[])",
                        [
                            solidity_array(datasets),
                            solidity_array(first),
                            solidity_array(end),
                        ],
                    )
                )
        else:
            for number, word_batch in enumerate(
                self.inventory.word_batches(
                    batch_size, globally_sorted=mode == "raw-global"
                )
            ):
                transactions.append(
                    self._transaction(
                        f"{name}-{number}",
                        "deleteSlots(bytes32[])",
                        [solidity_array(word_batch)],
                    )
                )
        verification = self._verify(name)
        return {
            "name": name,
            "mode": mode,
            "batchSize": batch_size,
            "transactions": transactions,
            "gasUsed": sum(
                quantity(record["receipt"]["gasUsed"]) for record in transactions
            ),
            "ipldCharges": self._sum_ipld_charges(transactions),
            "targetWordSetSha256": self.inventory.word_digest(),
            "verification": verification,
        }

    def run(self) -> dict[str, Any]:
        word_count = self.inventory.word_count()
        ranges = self.inventory.ranges()
        if word_count == 0:
            raise RuntimeError(
                "selected inventory contains no nonzero metadata storage words"
            )
        self.output.mkdir(parents=True, exist_ok=True)
        selected_piece_count = self.inventory.selected_piece_count()
        scanned_piece_count = sum(end - first for _, first, end in ranges)
        target_word_set_sha256 = self.inventory.word_digest()
        self.records = {
            "schemaVersion": SCHEMA_VERSION,
            "labOnly": True,
            "inventory": {
                "directory": str(self.inventory.directory),
                "summarySha256": sha256_file(self.inventory.summary_path),
                "piecesSha256": sha256_file(self.inventory.pieces_path),
                "coverage": self.inventory.summary.get("coverage"),
                "provenance": self.inventory.summary.get("provenance"),
                "selectedPieces": selected_piece_count,
                "naturalScannedPieces": scanned_piece_count,
                "emptyPieceScans": scanned_piece_count - selected_piece_count,
                "targetedNonzeroWords": word_count,
                "targetWordSetSha256": target_word_set_sha256,
                "ranges": [
                    {
                        "dataSetId": dataset,
                        "firstPieceId": first,
                        "endPieceIdExclusive": end,
                    }
                    for dataset, first, end in ranges
                ],
            },
            "parameters": {
                "gasLimit": GAS_LIMIT,
                "pieceBatchSizes": self.args.piece_batch_sizes,
                "wordBatchSizes": self.args.word_batch_sizes,
                "maxPieces": self.args.max_pieces,
                "sentinelSlots": self.sentinels,
            },
            "variants": [],
        }
        self.records["planningSeconds"] = self.inventory.planning_seconds
        self.records["stateRoots"] = self.chain.rpc("filecoin_getStateRoots")
        try:
            self.records["installation"] = self.compile_and_install(ranges)
            self.records["parameters"]["sentinelSlots"] = self.sentinels
            for size in self.args.piece_batch_sizes:
                self.records["variants"].append(
                    self._run_variant(f"natural-pieces-{size}", "natural", size, ranges)
                )
            for size in self.args.word_batch_sizes:
                self.records["variants"].append(
                    self._run_variant(
                        f"raw-natural-words-{size}", "raw-natural", size, ranges
                    )
                )
                self.records["variants"].append(
                    self._run_variant(
                        f"raw-global-slot-order-words-{size}",
                        "raw-global",
                        size,
                        ranges,
                    )
                )
            self.records["outcome"] = "success"
            return self.records
        except BaseException as error:
            self.records["outcome"] = "failure"
            self.records["error"] = repr(error)
            raise
        finally:
            try:
                if self.initial_snapshot is not None:
                    self.chain.revert(self.initial_snapshot)
                    if (
                        self.chain.rpc("eth_getCode", self.proxy, "latest")
                        != self.original_code
                    ):
                        raise RuntimeError(
                            "restored snapshot does not contain the original FWSS runtime"
                        )
                    for slot, expected in self.initial_sentinel_values.items():
                        actual = self.chain.rpc(
                            "eth_getStorageAt", self.proxy, hex(int(slot, 16)), "latest"
                        ).lower()
                        if actual != expected:
                            raise RuntimeError(
                                f"restored snapshot changed sentinel {slot}"
                            )
                elif self.original_code is not None:
                    self.chain.rpc("anvil_setCode", self.proxy, self.original_code)
                self.records["restoration"] = "restored"
            except BaseException as restoration_error:
                self.records["restoration"] = f"FAILED: {restoration_error!r}"
                raise
            finally:
                atomic_json(self.output / "summary.json", self.records)
                for variant in self.records.get("variants", []):
                    atomic_json(self.output / f"{variant['name']}.json", variant)
                self.inventory.close()


class WordSelection:
    """Small frozen storage-word selection; never expands partial owners."""

    def __init__(self, words: list[dict[str, Any]]) -> None:
        self.words = words

    def rows(self) -> Iterable[Any]:
        for word in self.words:
            yield word["dataSetId"], word["pieceId"], [], [word]

    def word_count(self) -> int:
        return len(self.words)

    def word_digest(self) -> str:
        digest = hashlib.sha256()
        for word in sorted(self.words, key=lambda word: word["slot"]):
            digest.update(bytes.fromhex(word["slot"][2:]))
        return digest.hexdigest()

    def word_batches(self, size: int, *, globally_sorted: bool) -> Iterable[list[str]]:
        for offset in range(0, len(self.words), size):
            yield [word["slot"] for word in self.words[offset : offset + size]]

    def metadata_batches(self, size: int) -> Iterable[Any]:
        return iter(())

    def selected_piece_count(self) -> int:
        return 0


def selection_manifest(
    words: list[dict[str, Any]], source_digest: str, keys: list[Any] | None = None
) -> dict[str, Any]:
    ordered = sorted(words, key=lambda word: word["slot"])
    values_digest = hashlib.sha256()
    owners: dict[tuple[int, int], int] = {}
    datasets: dict[int, int] = {}
    for word in ordered:
        values_digest.update(bytes.fromhex(word["slot"][2:] + word["value"][2:]))
        owner = word["dataSetId"], word["pieceId"]
        owners[owner] = owners.get(owner, 0) + 1
        datasets[owner[0]] = datasets.get(owner[0], 0) + 1
    manifest_identity = {
        "sourcePiecesSha256": source_digest,
        "words": words,
        "pieces": keys,
    }
    manifest = {
        "sourcePiecesSha256": source_digest,
        "wordCount": len(words),
        "targetWordSetSha256": WordSelection(words).word_digest(),
        "slotValueSetSha256": values_digest.hexdigest(),
        "manifestSha256": hashlib.sha256(
            json.dumps(
                manifest_identity, sort_keys=True, separators=(",", ":")
            ).encode()
        ).hexdigest(),
        "slotRange": {"first": ordered[0]["slot"], "last": ordered[-1]["slot"]},
        "ownerPieceCount": len(owners),
        "ownerDistribution": [
            {"dataSetId": dataset, "pieceId": piece, "words": count}
            for (dataset, piece), count in sorted(owners.items())
        ],
        "datasetDistribution": [
            {"dataSetId": dataset, "words": count}
            for dataset, count in sorted(datasets.items())
        ],
        "applicationKeyCount": None
        if keys is None
        else sum(len(row["keys"]) for row in keys),
        "words": words,
    }
    if keys is not None:
        manifest["pieces"] = keys
    return manifest


class GlobalComparisonRunner(Runner):
    """Equal-word comparison against the FULL inventory, not a sorted piece subset."""

    def __init__(self, args: argparse.Namespace) -> None:
        started = time.monotonic()
        super().__init__(args)
        self.natural_inventory = self.inventory
        self.natural_rows = [
            {"dataSetId": dataset, "pieceId": piece, "keys": keys, "slots": words}
            for dataset, piece, keys, words in self.inventory.rows()
        ]
        if len(self.natural_rows) != 512 or any(
            not row["keys"] or not row["slots"] for row in self.natural_rows
        ):
            raise RuntimeError(
                "global comparison requires 512 metadata-bearing natural pieces"
            )
        natural_words = [
            dict(word, dataSetId=row["dataSetId"], pieceId=row["pieceId"])
            for row in self.natural_rows
            for word in row["slots"]
        ]
        count = len(natural_words)
        heap: list[int] = []
        candidates: dict[int, dict[str, Any]] = {}
        digest = hashlib.sha256()
        last = None
        full_pieces = full_words = full_keys = 0
        source_before = self.inventory.pieces_path.stat()
        with self.inventory.pieces_path.open("rb") as source:
            for line in source:
                digest.update(line)
                row = json.loads(line)
                owner = row["dataSetId"], row["pieceId"]
                if last is not None and owner <= last:
                    raise RuntimeError("full inventory is not strictly natural ordered")
                last = owner
                full_pieces += 1
                full_keys += len(row["keys"])
                for entry in row["slots"]:
                    slot = hex_word(entry["slot"], "slot")
                    value = hex_word(entry["value"], "value")
                    if value == "0x" + "00" * 32:
                        raise RuntimeError(
                            "full inventory contains a zero metadata word"
                        )
                    full_words += 1
                    number = int(slot, 16)
                    if number in candidates:
                        raise RuntimeError("duplicate slot in global prefix inventory")
                    if len(heap) < count + 1 or number < -heap[0]:
                        if len(heap) == count + 1:
                            del candidates[-heapq.heappop(heap)]
                        heapq.heappush(heap, -number)
                        candidates[number] = {
                            "slot": slot,
                            "value": value,
                            "dataSetId": owner[0],
                            "pieceId": owner[1],
                            "ownerWords": row["slots"],
                        }
        source_after = self.inventory.pieces_path.stat()
        if any(
            getattr(source_before, field) != getattr(source_after, field)
            for field in ("st_dev", "st_ino", "st_size", "st_mtime_ns")
        ):
            raise RuntimeError("inventory changed during global selection")
        if len(heap) <= count:
            raise RuntimeError(
                "full inventory must contain more words than the natural sample"
            )
        ordered = [candidates[number] for number in sorted(candidates)]
        selected = ordered[:count]
        boundary = ordered[count]
        global_words = [
            {field: word[field] for field in ("slot", "value", "dataSetId", "pieceId")}
            for word in selected
        ]
        owner_words = {
            (word["dataSetId"], word["pieceId"]): word["ownerWords"]
            for word in selected
        }
        self.source_digest = digest.hexdigest()
        self.manifests = {
            "natural": selection_manifest(
                natural_words, self.source_digest, self.natural_rows
            ),
            "global": selection_manifest(global_words, self.source_digest),
        }
        natural_slots = {word["slot"] for word in natural_words}
        global_slots = {word["slot"] for word in global_words}
        self.overlap = {
            "words": len(natural_slots & global_slots),
            "naturalOnlyWords": len(natural_slots - global_slots),
            "globalOnlyWords": len(global_slots - natural_slots),
            "identical": natural_slots == global_slots,
        }
        self.global_inventory = WordSelection(global_words)
        self.full_counts = {
            "pieces": full_pieces,
            "words": full_words,
            "applicationKeys": full_keys,
        }
        expected_counts = {
            "pieces": self.inventory.summary.get("piecesWithMetadata"),
            "words": self.inventory.summary.get("nonzeroWords"),
            "applicationKeys": self.inventory.summary.get("keys"),
        }
        if self.full_counts != expected_counts:
            raise RuntimeError(
                f"full inventory counts differ from summary: {self.full_counts} != {expected_counts}"
            )
        self._legacy_digest_cache: dict[Path, str] = {}
        # Boundary and up to two untargeted words per partial owner are checked
        # without requiring key arrays belonging to those owners to be empty.
        self.global_sentinels = {boundary["slot"]: boundary["value"]}
        self.global_sentinel_kinds = {boundary["slot"]: "first-unselected-global-slot"}
        for words in owner_words.values():
            siblings = 0
            for word in words:
                if word["slot"] not in global_slots:
                    self.global_sentinels[word["slot"]] = word["value"]
                    self.global_sentinel_kinds[word["slot"]] = "selected-owner-sibling"
                    siblings += 1
                    if siblings == 2:
                        break
        self.global_planning_seconds = time.monotonic() - started

    def _verify(self, variant: str) -> dict[str, Any]:
        result = super()._verify(variant)
        if self.inventory is self.global_inventory:
            result.pop("metadataKeysEmpty")
            result["partialOwnerKeyArraysNotRequiredEmpty"] = True
            self._verify_global_sentinels()
            result["untargetedGlobalWordsPreserved"] = len(self.global_sentinels)
        return result

    def _verify_global_sentinels(self) -> None:
        words = [
            {"slot": slot, "value": value}
            for slot, value in self.global_sentinels.items()
        ]
        for start in range(0, len(words), 128):
            self._check_inventory_batch(words[start : start + 128])

    @staticmethod
    def _reused_record(
        variant: dict[str, Any],
        path: Path,
        identity: dict[str, Any],
        evidence: str,
    ) -> dict[str, Any]:
        result = dict(
            variant,
            identity=identity,
            reusedFrom=str(path.resolve()),
            reuseEvidence=evidence,
        )
        result["transactions"] = [
            dict(
                transaction,
                traceFile=str((path.parent / transaction["traceFile"]).resolve()),
            )
            if transaction.get("traceFile")
            else dict(transaction)
            for transaction in variant["transactions"]
        ]
        return result

    def _reuse(self, name: str, identity: dict[str, Any]) -> dict[str, Any] | None:
        for directory in [self.output, *self.args.reuse_from]:
            path = directory / "summary.json"
            if not path.is_file():
                continue
            previous = json.loads(path.read_text())
            if (
                previous.get("outcome") != "success"
                or previous.get("restoration") != "restored"
            ):
                continue
            variant = next(
                (item for item in previous.get("variants", []) if item["name"] == name),
                None,
            )
            if variant is None:
                continue
            if variant.get("identity") == identity:
                return self._reused_record(
                    variant, path, identity, "exact measurement identity"
                )
            # Legacy measurements did not persist manifests. Reconstruct their
            # exact selection only when their complete source is still available.
            if name not in ("natural-pieces-64", "raw-natural-words-256"):
                continue
            source = previous.get("inventory", {})
            old_dir = Path(source.get("directory", ""))
            if not (old_dir / "pieces.jsonl").is_file():
                continue
            if (
                previous.get("stateRoots") != self.records["stateRoots"]
                or previous.get("parameters", {}).get("gasLimit") != GAS_LIMIT
                or any(
                    previous.get("installation", {}).get(field)
                    != self.records["installation"][field]
                    for field in ("labRuntimeSha256", "originalCodeSha256")
                )
            ):
                continue
            old_pieces_path = old_dir / "pieces.jsonl"
            if old_pieces_path not in self._legacy_digest_cache:
                self._legacy_digest_cache[old_pieces_path] = sha256_file(
                    old_pieces_path
                )
            if self._legacy_digest_cache[old_pieces_path] != source.get("piecesSha256"):
                continue
            old = Inventory(old_dir, 512)
            try:
                old_rows = [
                    {
                        "dataSetId": dataset,
                        "pieceId": piece,
                        "keys": keys,
                        "slots": words,
                    }
                    for dataset, piece, keys, words in old.rows()
                ]
                if (
                    old_rows == self.natural_rows
                    and source.get("ranges") == self.records["inventory"]["ranges"]
                    and variant.get("targetWordSetSha256")
                    == self.natural_inventory.word_digest()
                ):
                    return self._reused_record(
                        variant,
                        path,
                        identity,
                        "exact legacy rows, source digest, ranges, roots and runtime",
                    )
            finally:
                old.close()
        return None

    def run(self) -> dict[str, Any]:
        self.output.mkdir(parents=True, exist_ok=True)
        ranges = self.natural_inventory.ranges()
        roots = self.chain.rpc("filecoin_getStateRoots")
        self.records = {
            "schemaVersion": 2,
            "labOnly": True,
            "warning": "Global raw deletion temporarily leaves partial metadata structures inconsistent; this is not production migration safety.",
            "stateRoots": roots,
            "parameters": {
                "gasLimit": GAS_LIMIT,
                "globalWordBatchSize": 256,
                "naturalPieceBatchSize": 64,
                "naturalRawWordBatchSize": 256,
            },
            "inventory": {
                "directory": str(self.natural_inventory.directory),
                "piecesSha256": self.source_digest,
                "summarySha256": sha256_file(self.natural_inventory.summary_path),
                "fullCounts": self.full_counts,
                "provenance": self.natural_inventory.summary.get("provenance"),
                "coverage": self.natural_inventory.summary.get("coverage"),
                "naturalApplicationKeyCount": self.manifests["natural"][
                    "applicationKeyCount"
                ],
                "ranges": [
                    {
                        "dataSetId": dataset,
                        "firstPieceId": first,
                        "endPieceIdExclusive": end,
                    }
                    for dataset, first, end in ranges
                ],
            },
            "selectionManifests": {
                key: {
                    field: value
                    for field, value in manifest.items()
                    if field not in ("words", "pieces")
                }
                for key, manifest in self.manifests.items()
            },
            "overlap": self.overlap,
            "globalSelectionSeconds": self.global_planning_seconds,
            "globalUntargetedSentinels": [
                {
                    "slot": slot,
                    "value": value,
                    "kind": self.global_sentinel_kinds[slot],
                }
                for slot, value in self.global_sentinels.items()
            ],
            "variants": [],
        }
        for key, manifest in self.manifests.items():
            path = self.output / f"{key}-selection.json"
            if path.exists() and json.loads(path.read_text()) != manifest:
                raise RuntimeError(
                    f"selection differs: use a new output directory, not {path}"
                )
            atomic_json(path, manifest)
        try:
            self.records["installation"] = self.compile_and_install(ranges)
            self.inventory = self.global_inventory
            self._verify_inventory_words()
            self._verify_global_sentinels()
            self.inventory = self.natural_inventory
            cases = [
                ("natural-pieces-64", "natural", 64, "natural"),
                ("raw-natural-words-256", "raw-natural", 256, "natural"),
                ("raw-global-prefix-words-256", "raw-global", 256, "global"),
            ]
            requested = set(self.args.variant or [case[0] for case in cases])
            for name, mode, size, selection in cases:
                if name not in requested:
                    continue
                identity = {
                    "selectionManifestSha256": self.manifests[selection][
                        "manifestSha256"
                    ],
                    "slotValueSetSha256": self.manifests[selection][
                        "slotValueSetSha256"
                    ],
                    "stateRoots": roots,
                    "gasLimit": GAS_LIMIT,
                    "mode": mode,
                    "batchSize": size,
                    "proxy": self.proxy.lower(),
                    "labRuntimeSha256": self.records["installation"][
                        "labRuntimeSha256"
                    ],
                    "originalCodeSha256": self.records["installation"][
                        "originalCodeSha256"
                    ],
                }
                reused = self._reuse(name, identity)
                if reused is not None:
                    self.records["variants"].append(reused)
                    continue
                if (self.output / f"{name}.json").exists():
                    raise RuntimeError(
                        f"{name}: incompatible existing result; use a new output directory"
                    )
                self.inventory = (
                    self.global_inventory
                    if selection == "global"
                    else self.natural_inventory
                )
                try:
                    result = self._run_variant(name, mode, size, ranges)
                    result["identity"] = identity
                    self.records["variants"].append(result)
                    atomic_json(self.output / f"{name}.json", result)
                finally:
                    self.inventory = self.natural_inventory
            self.records["outcome"] = "success"
            return self.records
        except BaseException as error:
            self.records["outcome"] = "failure"
            self.records["error"] = repr(error)
            raise
        finally:
            try:
                if self.initial_snapshot is not None:
                    self.chain.revert(self.initial_snapshot)
                    if (
                        self.chain.rpc("eth_getCode", self.proxy, "latest")
                        != self.original_code
                    ):
                        raise RuntimeError("original FWSS runtime not restored")
                    restored_roots = self.chain.rpc("filecoin_getStateRoots")
                    if restored_roots != roots:
                        raise RuntimeError(
                            "snapshot did not restore original state roots"
                        )
                    self.records["restorationVerification"] = {
                        "method": "exact FVM state-root commitment and runtime bytecode",
                        "stateRoots": restored_roots,
                        "naturalTargetWords": self.natural_inventory.word_count(),
                        "globalTargetWords": self.global_inventory.word_count(),
                        "globalUntargetedWords": len(self.global_sentinels),
                    }
                self.records["restoration"] = "restored"
            except BaseException as error:
                self.records["restoration"] = f"FAILED: {error!r}"
                raise
            finally:
                atomic_json(self.output / "summary.json", self.records)
                self.natural_inventory.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--inventory",
        required=True,
        type=Path,
        help="directory containing summary.json and pieces.jsonl",
    )
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18546")
    parser.add_argument("--private-key", default=DEFAULT_KEY)
    parser.add_argument("--fwss-proxy", default=FWSS_PROXY)
    parser.add_argument(
        "--max-pieces",
        type=int,
        default=64,
        help="natural-prefix bound; 0 selects the entire inventory",
    )
    parser.add_argument("--piece-batch-sizes", type=csv_positive, default=[64])
    parser.add_argument("--word-batch-sizes", type=csv_positive, default=[64, 128])
    parser.add_argument(
        "--sentinel-slot",
        action="append",
        default=["0x" + "00" * 31 + "01"],
        help="non-target FWSS storage word to preserve; may repeat",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("benchmarks/fwss/results/metadata-cleanup-6457920"),
    )
    parser.add_argument(
        "--mode",
        choices=("standalone", "global-prefix"),
        default="standalone",
        help="standalone preserves the historical matrix; global-prefix compares equal word counts from different selections",
    )
    parser.add_argument(
        "--variant",
        action="append",
        choices=(
            "natural-pieces-64",
            "raw-natural-words-256",
            "raw-global-prefix-words-256",
        ),
        help="global-prefix variant to run; repeat to select more than one",
    )
    parser.add_argument(
        "--reuse-from",
        action="append",
        type=Path,
        default=[Path("benchmarks/fwss/results/metadata-cleanup-6457920/sample-512")],
        help="prior result directory eligible only after exact identity verification",
    )
    args = parser.parse_args()
    if args.max_pieces < 0:
        parser.error("--max-pieces must be nonnegative")
    if args.max_pieces == 0:
        args.max_pieces = None
    if args.mode == "global-prefix":
        args.max_pieces = 512
    elif args.variant:
        parser.error("--variant is only available with --mode global-prefix")
    return args


if __name__ == "__main__":
    arguments = parse_args()
    try:
        runner = (
            GlobalComparisonRunner(arguments)
            if arguments.mode == "global-prefix"
            else Runner(arguments)
        )
        result = runner.run()
    except BaseException as error:
        print(json.dumps({"outcome": "failure", "error": repr(error)}), file=sys.stderr)
        raise
    print(
        json.dumps(
            {
                "outcome": result["outcome"],
                "output": str(arguments.output.resolve()),
                "variants": [
                    {"name": variant["name"], "gasUsed": variant["gasUsed"]}
                    for variant in result["variants"]
                ],
            },
            sort_keys=True,
        )
    )
