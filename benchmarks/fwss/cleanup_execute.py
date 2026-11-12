#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Execute an immutable cleanup plan sequentially, or verify it without a key.

Execution requires an explicitly selected network, facet runtime hash, and signing
key environment variable. A journal is deliberately not a retry queue: an unknown
transaction is only rebroadcast with --rebroadcast-pending, using identical bytes.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import sys
import tempfile
import time
import urllib.error
import urllib.request
from itertools import islice
from pathlib import Path
from typing import Any, Iterator

from eth_abi import decode, encode
from eth_account import Account
from eth_abi.exceptions import DecodingError
from eth_account.typed_transactions import TypedTransaction
from eth_hash.auto import keccak
from hexbytes import HexBytes

from chain import JsonRpcError, quantity


CLEANUP_SELECTOR = keccak(b"cleanupLegacyPieceMetadata(bytes)")[:4]
IMPLEMENTATION_SELECTOR = keccak(b"implementation(bytes4)")[:4]
PHASES = ("live-queues", "deleted-queues", "metadata")
QUEUE_PHASES = set(PHASES[:2])


def hex_bytes(value: Any, size: int | None = None) -> bytes:
    if not isinstance(value, str) or not value.startswith("0x"):
        raise ValueError(f"expected 0x-prefixed hex, got {value!r}")
    if len(value[2:]) % 2:
        raise ValueError("hex must contain complete bytes")
    result = bytes.fromhex(value[2:])
    if size is not None and len(result) != size:
        raise ValueError(f"expected {size} bytes, got {len(result)}")
    return result


def hex_value(value: bytes) -> str:
    return "0x" + value.hex()


def address(value: Any) -> str:
    result = hex_value(hex_bytes(value, 20))
    if int(result, 16) == 0:
        raise ValueError("zero contract/signer address is not permitted")
    return result


def nonnegative(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise ValueError(f"expected an integer, got {value!r}")
    result = quantity(value)
    if result < 0:
        raise ValueError("integer must not be negative")
    return result


def positive_cli(value: str) -> int:
    result = int(value, 0)
    if result <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def positive_seconds(value: str) -> float:
    result = float(value)
    if not 0 < result < float("inf"):
        raise argparse.ArgumentTypeError("must be a positive finite number")
    return result


def durable_directory(path: Path) -> None:
    if path.exists():
        return
    durable_directory(path.parent)
    path.mkdir(mode=0o700)
    parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def durable_json(path: Path, value: Any) -> None:
    """Replace atomically and fsync both the file and directory before returning."""
    fd, temporary = tempfile.mkstemp(prefix=".cleanup-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as output:
            json.dump(value, output, sort_keys=True, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def fingerprint(path: Path) -> tuple[int, ...]:
    stat = path.stat()
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


class Plan:
    def __init__(self, directory: Path) -> None:
        self.directory = directory
        manifest_path = directory / "manifest.json"
        self.batches_path = directory / "batches.jsonl"
        self.files = {
            manifest_path: fingerprint(manifest_path),
            self.batches_path: fingerprint(self.batches_path),
        }
        manifest_bytes = manifest_path.read_bytes()
        self.manifest = json.loads(manifest_bytes)
        if self.manifest.get("version") != 1:
            raise ValueError("unsupported cleanup plan version (expected 1)")
        self.fwss = address(self.manifest["fwss"])
        source = self.manifest.get("source")
        if not isinstance(source, dict) or address(source["address"]) != self.fwss:
            raise ValueError("plan FWSS address does not match source inventory")
        self.batch_count = nonnegative(self.manifest["batchCount"])
        expected_hash = self.manifest["batchesSha256"]
        if isinstance(expected_hash, str) and not expected_hash.startswith("0x"):
            expected_hash = "0x" + expected_hash
        self.batches_hash = hex_bytes(expected_hash, 32).hex()
        digest = hashlib.sha256()
        phase_index = 0
        count = 0
        with self.batches_path.open("rb") as batches:
            for line in batches:
                digest.update(line)
                batch = self.parse_batch(line, count)
                current = PHASES.index(batch["phase"])
                if current < phase_index:
                    raise ValueError("plan phases must be live-queues, deleted-queues, metadata")
                phase_index = current
                count += 1
        if digest.hexdigest() != self.batches_hash:
            raise ValueError("batches.jsonl SHA-256 differs from manifest; regenerate the plan")
        if count != self.batch_count:
            raise ValueError(f"plan batchCount is {self.batch_count}, read {count}")
        # The manifest commits to the validated batches hash, so its byte hash
        # identifies both files without loading a large plan into memory.
        self.plan_hash = hashlib.sha256(manifest_bytes).hexdigest()
        self.assert_unchanged()

    def assert_unchanged(self) -> None:
        for path, expected in self.files.items():
            if fingerprint(path) != expected:
                raise RuntimeError(f"plan changed during use: {path}; stop and use a new journal")

    @staticmethod
    def parse_batch(line: bytes, expected_index: int) -> dict[str, Any]:
        row = json.loads(line)
        if nonnegative(row["index"]) != expected_index:
            raise ValueError(f"batch indexes must be contiguous from zero (expected {expected_index})")
        if row["phase"] not in PHASES:
            raise ValueError(f"batch {expected_index}: unknown phase")
        if nonnegative(row["operations"]) == 0:
            raise ValueError(f"batch {expected_index}: operations must be positive")
        calldata = hex_bytes(row["data"])
        if calldata[:4] != CLEANUP_SELECTOR:
            raise ValueError(f"batch {expected_index}: not cleanupLegacyPieceMetadata(bytes)")
        operations = decode(["bytes"], calldata[4:])[0]
        if not operations or encode(["bytes"], [operations]) != calldata[4:]:
            raise ValueError(f"batch {expected_index}: malformed/noncanonical calldata")
        row["data"] = hex_value(calldata)
        if not isinstance(row["slots"], list) or not isinstance(row["queues"], list):
            raise ValueError(f"batch {expected_index}: slots and queues must be arrays")
        row["slots"] = [hex_value(hex_bytes(slot, 32)) for slot in row["slots"]]
        for queue in row["queues"]:
            nonnegative(queue["dataSetId"])
            queue["lengthSlot"] = hex_value(hex_bytes(queue["lengthSlot"], 32))
        if row["phase"] == "metadata" and row["queues"]:
            raise ValueError(f"batch {expected_index}: metadata cannot contain queue operations")
        row["index"] = expected_index
        return row

    def batches(self) -> Iterator[dict[str, Any]]:
        self.assert_unchanged()
        with self.batches_path.open("rb") as source:
            for index, line in enumerate(source):
                self.assert_unchanged()
                yield self.parse_batch(line, index)
        self.assert_unchanged()


class Rpc:
    def __init__(self, url: str, timeout: float) -> None:
        self.url = url
        self.timeout = timeout

    def request(self, payload: Any, label: str, timeout: float | None = None) -> Any:
        request = urllib.request.Request(
            self.url,
            data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout or self.timeout) as response:
                return json.load(response)
        except (urllib.error.URLError, TimeoutError, ValueError) as error:
            raise RuntimeError(f"{label}: RPC transport/JSON response failed: {error}") from error

    @staticmethod
    def result(response: Any, request_id: int, method: str) -> Any:
        if (
            not isinstance(response, dict)
            or response.get("jsonrpc") != "2.0"
            or type(response.get("id")) is not int
            or response["id"] != request_id
            or ("result" in response) == ("error" in response)
        ):
            raise RuntimeError(f"{method}: malformed JSON-RPC response for request ID {request_id}")
        if "error" in response:
            error = response["error"]
            if (
                not isinstance(error, dict)
                or type(error.get("code")) is not int
                or not isinstance(error.get("message"), str)
            ):
                raise RuntimeError(f"{method}: malformed JSON-RPC error for request ID {request_id}")
            raise JsonRpcError(method, error)
        return response["result"]

    def call(self, method: str, *params: Any, timeout: float | None = None) -> Any:
        response = self.request(
            {"jsonrpc": "2.0", "id": 1, "method": method, "params": params},
            method, timeout,
        )
        return self.result(response, 1, method)

    def batch(self, requests: list[tuple[str, list[Any]]]) -> list[Any]:
        """Match responses by ID, never by their unspecified response order."""
        if not requests:
            return []
        payload = [
            {"jsonrpc": "2.0", "id": index, "method": method, "params": params}
            for index, (method, params) in enumerate(requests, 1)
        ]
        responses = self.request(payload, "JSON-RPC batch")
        if not isinstance(responses, list) or len(responses) != len(requests):
            raise RuntimeError("JSON-RPC batch response must contain exactly one result per request; use an endpoint supporting JSON-RPC batches (no single-request fallback)")
        by_id: dict[int, Any] = {}
        for response in responses:
            if not isinstance(response, dict):
                raise RuntimeError("JSON-RPC batch contains a non-object response")
            request_id = response.get("id")
            if type(request_id) is not int or not 1 <= request_id <= len(requests):
                raise RuntimeError("JSON-RPC batch contains an unknown or invalid request ID")
            if request_id in by_id:
                raise RuntimeError(f"JSON-RPC batch contains duplicate request ID {request_id}")
            by_id[request_id] = response
        # Cardinality, range, and uniqueness above prove there are no missing
        # responses. Validate every envelope/error and restore request order.
        return [
            self.result(by_id[index], index, method)
            for index, (method, _) in enumerate(requests, 1)
        ]


class Journal:
    def __init__(self, directory: Path, plan: Plan, args: argparse.Namespace, signer: str | None) -> None:
        self.directory = directory
        self.plan = plan
        self.args = args
        self.signer = signer
        self.identity: dict[str, Any] | None = None
        self.lock_fd: int | None = None

    def __enter__(self) -> Journal:
        executing = self.args.action == "execute"
        if not self.directory.exists() and not executing:
            return self
        if executing:
            durable_directory(self.directory)
        flags = os.O_CREAT | os.O_RDWR if executing else os.O_RDONLY
        self.lock_fd = os.open(self.directory / ".lock", flags, 0o600)
        try:
            fcntl.flock(self.lock_fd, (fcntl.LOCK_EX if executing else fcntl.LOCK_SH) | fcntl.LOCK_NB)
            expected = {
                "version": 1,
                "planSha256": self.plan.plan_hash,
                "batchesSha256": self.plan.batches_hash,
                "chainId": self.args.expected_chain_id,
                "fwss": self.plan.fwss,
                "facetCodeHash": self.args.expected_facet_code_hash,
            }
            path = self.directory / "manifest.json"
            if path.exists():
                self.identity = json.loads(path.read_text())
                if any(self.identity.get(key) != value for key, value in expected.items()):
                    raise RuntimeError("journal belongs to a different plan, chain, proxy, or facet hash")
                recorded_signer = address(self.identity["signer"])
                if self.signer is not None and recorded_signer != self.signer:
                    raise RuntimeError("journal signer differs from the explicitly supplied key")
                self.signer = recorded_signer
            elif executing:
                # Never adopt orphaned transaction files as a new execution.
                if any(self.directory.glob("batch-*.json")):
                    raise RuntimeError("journal identity is missing but batch files exist; restore its manifest")
                self.identity = {**expected, "signer": self.signer}
                durable_json(path, self.identity)
            else:
                raise RuntimeError("journal exists without its manifest")
            count = 0
            largest = -1
            for batch_path in self.directory.glob("batch-*.json"):
                index = int(batch_path.stem.removeprefix("batch-"))
                if batch_path != self.path(index) or not 0 <= index < self.plan.batch_count:
                    raise RuntimeError(f"unexpected journal batch file: {batch_path.name}")
                count += 1
                largest = max(largest, index)
            if largest + 1 != count:
                raise RuntimeError("journal has missing batch files; restore them before executing")
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_: Any) -> None:
        if self.lock_fd is not None:
            os.close(self.lock_fd)
            self.lock_fd = None

    def path(self, index: int) -> Path:
        return self.directory / f"batch-{index:012d}.json"

    def load(self, batch: dict[str, Any]) -> dict[str, Any] | None:
        path = self.path(batch["index"])
        if not path.exists():
            return None
        saved = json.loads(path.read_text())
        raw = hex_bytes(saved["rawTransaction"])
        if hex_value(keccak(raw)) != saved["transactionHash"]:
            raise RuntimeError(f"batch {batch['index']}: journal raw transaction/hash mismatch")
        if address(Account.recover_transaction(raw)) != self.signer:
            raise RuntimeError(f"batch {batch['index']}: signed transaction has a different signer")
        transaction = TypedTransaction.from_bytes(HexBytes(raw)).as_dict()
        if (
            saved["index"] != batch["index"]
            or transaction["chainId"] != self.args.expected_chain_id
            or hex_value(transaction["to"]) != self.plan.fwss
            or hex_value(transaction["data"]) != batch["data"]
            or transaction["value"] != 0
            or transaction["nonce"] != saved["nonce"]
            or transaction["gas"] != saved["gasLimit"]
        ):
            raise RuntimeError(f"batch {batch['index']}: signed transaction does not match the plan/journal")
        if saved["state"] not in {"prepared", "pending", "mined", "confirmed", "reverted", "verification-failed"}:
            raise RuntimeError(f"batch {batch['index']}: unknown journal state")
        return saved

    def save(self, saved: dict[str, Any]) -> None:
        durable_json(self.path(saved["index"]), saved)


class Cleanup:
    def __init__(self, args: argparse.Namespace, plan: Plan, journal: Journal, key: str | None) -> None:
        self.args = args
        self.plan = plan
        self.journal = journal
        self.key = key
        self.rpc = Rpc(args.rpc_url, args.rpc_timeout)

    def block(self, number: str = "latest") -> dict[str, Any]:
        block = self.rpc.call("eth_getBlockByNumber", number, False)
        if not block or not block.get("hash"):
            raise RuntimeError(f"canonical block unavailable: {number}")
        return block

    def canonical(self, block: dict[str, Any]) -> None:
        if self.block(block["number"])["hash"].lower() != block["hash"].lower():
            raise RuntimeError(f"block {block['number']} is no longer canonical; stop and investigate the reorg")

    def confirmed_head(self) -> dict[str, Any]:
        latest = quantity(self.block()["number"])
        number = latest - self.args.confirmations + 1
        if number < 0:
            raise RuntimeError("chain has too few blocks for the requested confirmations")
        return self.block(hex(number))

    def validate_network(self) -> None:
        actual = quantity(self.rpc.call("eth_chainId"))
        if actual != self.args.expected_chain_id:
            raise RuntimeError(f"RPC chain ID is {actual}, expected {self.args.expected_chain_id}")
        self.validate_facet(self.block())

    def validate_facet(self, block: dict[str, Any]) -> str:
        call_data = hex_value(IMPLEMENTATION_SELECTOR + encode(["bytes4"], [CLEANUP_SELECTOR]))
        try:
            result = self.rpc.call("eth_call", {"to": self.plan.fwss, "data": call_data}, block["number"])
            facet = address(decode(["address"], hex_bytes(result))[0])
        except (JsonRpcError, DecodingError, ValueError, TypeError) as error:
            raise RuntimeError("cleanup selector has no usable implementation(bytes4) route; install the intended cleanup facet first") from error
        runtime = hex_bytes(self.rpc.call("eth_getCode", facet, block["number"]))
        actual = hex_value(keccak(runtime))
        if not runtime or actual != self.args.expected_facet_code_hash:
            raise RuntimeError(f"cleanup facet {facet} runtime hash is {actual}, expected {self.args.expected_facet_code_hash}")
        self.canonical(block)
        return facet

    def zero_slots(self, slots: Iterator[str] | list[str], block: dict[str, Any], context: str) -> int:
        self.canonical(block)
        count = 0
        source = iter(slots)
        while chunk := list(islice(source, self.args.read_batch_size)):
            values = self.rpc.batch([
                ("eth_getStorageAt", [self.plan.fwss, hex(int(slot, 16)), block["number"]])
                for slot in chunk
            ])
            for slot, value in zip(chunk, values, strict=True):
                if int.from_bytes(hex_bytes(value, 32)) != 0:
                    raise RuntimeError(f"{context}: slot {slot} is nonzero ({value}) at canonical block {block['number']}; concurrent writes or an incomplete cleanup require investigation")
                count += 1
        self.canonical(block)
        return count

    def check_queues(self, phases: set[str]) -> None:
        if not phases:
            return
        block = self.confirmed_head()
        slots = (
            queue["lengthSlot"]
            for batch in self.plan.batches() if batch["phase"] in phases
            for queue in batch["queues"]
        )
        count = self.zero_slots(slots, block, "queue phase boundary")
        print(f"Verified {count} queue length checks at block {quantity(block['number'])}", file=sys.stderr)

    def receipt_block(self, receipt: dict[str, Any], saved: dict[str, Any]) -> dict[str, Any]:
        if receipt["transactionHash"].lower() != saved["transactionHash"]:
            raise RuntimeError("RPC returned a receipt for a different transaction")
        if address(receipt["to"]) != self.plan.fwss or address(receipt["from"]) != self.journal.signer:
            raise RuntimeError("receipt sender/destination differs from signed transaction")
        block = self.block(receipt["blockNumber"])
        if block["hash"].lower() != receipt["blockHash"].lower():
            raise RuntimeError(f"batch {saved['index']}: receipt is no longer canonical; no automatic resubmission")
        return block

    def wait_receipt(self, saved: dict[str, Any], *, persist: bool) -> tuple[dict[str, Any], dict[str, Any]]:
        deadline = time.monotonic() + self.args.receipt_timeout
        previous = saved.get("receipt")
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(f"batch {saved['index']}: receipt/confirmation timeout for {saved['transactionHash']}; journal retained, rerun to recover; unknown transactions require --rebroadcast-pending")
            receipt = self.rpc.call("eth_getTransactionReceipt", saved["transactionHash"], timeout=min(self.args.rpc_timeout, remaining))
            if previous and (receipt is None or receipt["blockHash"].lower() != previous["blockHash"].lower()):
                raise RuntimeError(f"batch {saved['index']}: previously recorded receipt disappeared or moved; investigate reorg, no automatic resubmission")
            if receipt:
                block = self.receipt_block(receipt, saved)
                status = quantity(receipt["status"])
                if status not in (0, 1):
                    raise RuntimeError("receipt has an unknown status")
                saved.update({"receipt": receipt, "gasUsed": quantity(receipt["gasUsed"])})
                if receipt.get("effectiveGasPrice") is not None:
                    saved["effectiveGasPrice"] = quantity(receipt["effectiveGasPrice"])
                    saved["gasCost"] = saved["gasUsed"] * saved["effectiveGasPrice"]
                if status == 0:
                    saved["state"] = "reverted"
                    if persist:
                        self.journal.save(saved)
                    raise RuntimeError(f"batch {saved['index']} reverted ({saved['transactionHash']}); journal retained; never automatically retried")
                if persist and previous is None:
                    saved["state"] = "mined"
                    self.journal.save(saved)
                previous = receipt
                head = quantity(self.block()["number"])
                if head >= quantity(receipt["blockNumber"]) + self.args.confirmations - 1:
                    self.canonical(block)
                    return receipt, block
            time.sleep(min(self.args.poll_interval, max(0, deadline - time.monotonic())))

    def broadcast(self, saved: dict[str, Any]) -> None:
        self.plan.assert_unchanged()
        raw_size = len(hex_bytes(saved["rawTransaction"]))
        if raw_size > self.args.max_tx_bytes or saved["gasLimit"] > self.args.max_gas:
            raise RuntimeError(f"saved batch {saved['index']} exceeds the current --max-tx-bytes or --max-gas; explicitly restore its original limits before rebroadcasting")
        # 'prepared' and the complete signed bytes are already durable here.
        try:
            result = self.rpc.call("eth_sendRawTransaction", saved["rawTransaction"])
            if not isinstance(result, str) or result.lower() != saved["transactionHash"]:
                raise RuntimeError("RPC returned an unexpected broadcast hash")
        except (RuntimeError, JsonRpcError) as error:
            saved["broadcastError"] = str(error)
            self.journal.save(saved)
            raise RuntimeError(f"broadcast outcome uncertain/rejected for {saved['transactionHash']}: {error}; raw transaction retained; rerun to recover, use --rebroadcast-pending only for an identical-byte retry") from error
        saved["state"] = "pending"
        saved.pop("broadcastError", None)
        self.journal.save(saved)

    def prepare(self, batch: dict[str, Any]) -> dict[str, Any]:
        head = self.block()
        facet = self.validate_facet(head)
        if quantity(self.rpc.call("eth_chainId")) != self.args.expected_chain_id:
            raise RuntimeError("RPC chain ID changed during execution")
        sender = self.journal.signer
        latest = quantity(self.rpc.call("eth_getTransactionCount", sender, "latest"))
        pending = quantity(self.rpc.call("eth_getTransactionCount", sender, "pending"))
        if latest != pending:
            raise RuntimeError(f"signer has unrelated pending transactions (latest nonce {latest}, pending {pending}); settle them or use a dedicated signer")
        estimate = quantity(self.rpc.call("eth_estimateGas", {"from": sender, "to": self.plan.fwss, "data": batch["data"], "value": "0x0"}))
        gas = (estimate * (100 + self.args.gas_margin_percent) + 99) // 100
        cap = min(self.args.max_gas, quantity(head["gasLimit"]))
        if gas > cap:
            raise RuntimeError(f"batch {batch['index']}: live gas estimate {estimate}, padded gas {gas} exceeds cap {cap}; regenerate a smaller plan with lower planner batch limits (use a NEW journal), or explicitly raise --max-gas")
        priority = self.args.priority_fee_per_gas
        base_fee = quantity(head.get("baseFeePerGas", "0x0"))
        fee = self.args.max_fee_per_gas
        if fee is None:
            fee = max(quantity(self.rpc.call("eth_gasPrice")), 2 * base_fee + priority)
        if fee < base_fee + priority:
            raise RuntimeError("--max-fee-per-gas is below current base fee plus priority fee")
        transaction = {
            "type": 2, "chainId": self.args.expected_chain_id, "nonce": latest,
            "to": hex_bytes(self.plan.fwss, 20), "value": 0, "data": hex_bytes(batch["data"]),
            "gas": gas, "maxFeePerGas": fee, "maxPriorityFeePerGas": priority, "accessList": [],
        }
        signed = Account.sign_transaction(transaction, self.key)
        raw = bytes(signed.raw_transaction)
        if len(raw) > self.args.max_tx_bytes:
            raise RuntimeError(f"batch {batch['index']}: signed transaction is {len(raw)} bytes, exceeds --max-tx-bytes {self.args.max_tx_bytes}; regenerate with a lower planner --max-packed-bytes and a NEW journal")
        self.plan.assert_unchanged()
        saved = {
            "index": batch["index"], "phase": batch["phase"], "state": "prepared",
            "transactionHash": hex_value(keccak(raw)), "rawTransaction": hex_value(raw),
            "nonce": latest, "gasEstimate": estimate, "gasLimit": gas,
            "maxFeePerGas": fee, "maxPriorityFeePerGas": priority,
            "signedBytes": len(raw), "facet": facet,
        }
        self.journal.save(saved)
        return saved

    def execute_batch(self, batch: dict[str, Any]) -> dict[str, Any]:
        saved = self.journal.load(batch)
        if saved is None:
            saved = self.prepare(batch)
            self.broadcast(saved)
        else:
            if saved["state"] == "reverted":
                raise RuntimeError(f"batch {batch['index']}: journal records a revert; no automatic resubmission")
            receipt = self.rpc.call("eth_getTransactionReceipt", saved["transactionHash"])
            if receipt is None:
                if saved.get("receipt"):
                    raise RuntimeError(f"batch {batch['index']}: recorded receipt disappeared; investigate reorg")
                pending = self.rpc.call("eth_getTransactionByHash", saved["transactionHash"])
                if pending is None:
                    latest_nonce = quantity(self.rpc.call("eth_getTransactionCount", self.journal.signer, "latest"))
                    pending_nonce = quantity(self.rpc.call("eth_getTransactionCount", self.journal.signer, "pending"))
                    if latest_nonce != saved["nonce"] or pending_nonce != saved["nonce"]:
                        raise RuntimeError(f"batch {batch['index']}: transaction is unknown and saved nonce {saved['nonce']} differs from latest/pending {latest_nonce}/{pending_nonce}; investigate, no automatic replacement")
                    if not self.args.rebroadcast_pending:
                        raise RuntimeError(f"batch {batch['index']}: journal transaction {saved['transactionHash']} is unknown to the node; inspect it, then use --rebroadcast-pending to send exactly the saved bytes (never a new nonce)")
                    self.validate_facet(self.block())
                    self.broadcast(saved)
                elif pending.get("blockHash"):
                    raise RuntimeError("transaction is mined but its receipt is unavailable; use an RPC endpoint with receipt history")
        _, block = self.wait_receipt(saved, persist=True)
        try:
            self.zero_slots(batch["slots"], block, f"batch {batch['index']}")
        except RuntimeError as error:
            saved["state"] = "verification-failed"
            saved["verificationError"] = str(error)
            self.journal.save(saved)
            raise
        saved["state"] = "confirmed"
        saved.pop("verificationError", None)
        self.journal.save(saved)
        print(f"Confirmed batch {batch['index']} ({batch['phase']}), {saved['transactionHash']}, gas {saved['gasUsed']}", file=sys.stderr)
        return saved

    def verify_final(self) -> dict[str, Any]:
        block = self.confirmed_head()
        slots = (slot for batch in self.plan.batches() for slot in batch["slots"])
        slot_count = self.zero_slots(slots, block, "final plan verification")
        queue_slots = (queue["lengthSlot"] for batch in self.plan.batches() for queue in batch["queues"])
        queue_count = self.zero_slots(queue_slots, block, "final queue verification")
        self.plan.assert_unchanged()
        return {"blockNumber": quantity(block["number"]), "blockHash": block["hash"], "slotChecks": slot_count, "queueChecks": queue_count}

    def run(self) -> dict[str, Any]:
        self.validate_network()
        completed_phases: set[str] = set()
        previous_phase: str | None = None
        receipt_count = 0
        gas_used = 0
        for batch in self.plan.batches():
            if self.args.action == "execute":
                if batch["phase"] != previous_phase:
                    if previous_phase in QUEUE_PHASES:
                        completed_phases.add(previous_phase)
                        self.check_queues(completed_phases)
                    previous_phase = batch["phase"]
                saved = self.execute_batch(batch)
            else:
                saved = self.journal.load(batch) if self.journal.identity else None
                if saved is not None:
                    if saved["state"] == "reverted":
                        raise RuntimeError(f"batch {batch['index']}: journal records a reverted transaction")
                    receipt = self.rpc.call("eth_getTransactionReceipt", saved["transactionHash"])
                    if receipt is None:
                        raise RuntimeError(f"batch {batch['index']}: journal transaction is unresolved; use execute to recover it")
                    _, block = self.wait_receipt(saved, persist=False)
                    self.zero_slots(batch["slots"], block, f"journal batch {batch['index']}")
            if saved is not None:
                receipt_count += 1
                gas_used += saved["gasUsed"]
        if self.args.action == "execute" and previous_phase in QUEUE_PHASES:
            completed_phases.add(previous_phase)
            self.check_queues(completed_phases)
        verification = self.verify_final()
        return {
            "status": "verified", "action": self.args.action, "planSha256": self.plan.plan_hash,
            "chainId": self.args.expected_chain_id, "fwss": self.plan.fwss,
            "facetCodeHash": self.args.expected_facet_code_hash, "batchCount": self.plan.batch_count,
            "journal": str(self.journal.directory) if self.journal.identity else None,
            "signer": self.journal.signer, "receiptsVerified": receipt_count, "gasUsed": gas_used,
            "confirmations": self.args.confirmations, "verification": verification,
        }


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    for action in ("execute", "verify"):
        sub = commands.add_parser(action, help="send sequential signed batches" if action == "execute" else "read-only verification; no private key required")
        sub.add_argument("plan", type=Path, help="directory containing manifest.json and batches.jsonl")
        sub.add_argument("--rpc-url", required=True)
        sub.add_argument("--expected-chain-id", required=True, type=positive_cli)
        sub.add_argument("--expected-facet-code-hash", required=True, help="0x-prefixed keccak256 of cleanup facet runtime bytecode")
        sub.add_argument("--journal", type=Path, help="journal directory (default: PLAN/execution-journal); verify reads it if present")
        sub.add_argument("--confirmations", type=positive_cli, default=1, help="including the receipt block (default: 1)")
        sub.add_argument("--receipt-timeout", type=positive_seconds, default=300, help="seconds per receipt and confirmation wait")
        sub.add_argument("--poll-interval", type=positive_seconds, default=2)
        sub.add_argument("--rpc-timeout", type=positive_seconds, default=30)
        sub.add_argument("--read-batch-size", type=positive_cli, default=128, help="maximum eth_getStorageAt requests per JSON-RPC batch (default: 128); endpoint must support batches and historical receipt-block reads")
        if action == "execute":
            sub.add_argument("--private-key-env", default="FWSS_CLEANUP_PRIVATE_KEY", help="name of required signing-key environment variable; no development key fallback")
            sub.add_argument("--rebroadcast-pending", action="store_true", help="explicitly rebroadcast unknown journal transactions using only the original signed bytes")
            sub.add_argument("--max-gas", type=positive_cli, default=10_000_000_000)
            sub.add_argument("--max-tx-bytes", type=positive_cli, default=32_768, help="maximum signed Ethereum transaction size")
            sub.add_argument("--gas-margin-percent", type=int, default=20)
            sub.add_argument("--max-fee-per-gas", type=positive_cli, help="wei; default max(eth_gasPrice, 2*baseFee+priority)")
            sub.add_argument("--priority-fee-per-gas", type=positive_cli, default=1, help="wei (default: 1)")
    args = parser.parse_args()
    if args.action == "execute" and args.gas_margin_percent < 0:
        parser.error("--gas-margin-percent must not be negative")
    args.expected_facet_code_hash = hex_value(hex_bytes(args.expected_facet_code_hash, 32))
    args.plan = args.plan.resolve()
    args.journal = (args.journal or args.plan / "execution-journal").resolve()
    return args


def main() -> int:
    try:
        args = arguments()
        plan = Plan(args.plan)
        key = None
        signer = None
        if args.action == "execute":
            key = os.environ.get(args.private_key_env)
            if not key:
                raise ValueError(f"set the explicitly required key environment variable {args.private_key_env}")
            signer = address(Account.from_key(key).address)
        with Journal(args.journal, plan, args, signer) as journal:
            result = Cleanup(args, plan, journal, key).run()
        print(json.dumps(result, sort_keys=True))
        return 0
    except (DecodingError, ValueError, KeyError, OSError, RuntimeError, JsonRpcError) as error:
        print(f"cleanup: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("cleanup: interrupted; journal retained, rerun the same command to recover", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
