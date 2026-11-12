#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Execute the complete immutable cleanup inventory locally and record every receipt."""

from __future__ import annotations

import argparse
import contextlib
import gzip
import hashlib
import json
import os
import sys
import time
from pathlib import Path

from eth_abi import decode, encode
from eth_account import Account
from eth_hash.auto import keccak

from cleanup_execute import Cleanup, Journal, Plan, Rpc, durable_json
from cleanup_plan import SIGNATURE, calldata

DELEGATES_ROOT = bytes.fromhex("f27774d37a8b3bf2306f60b561e4e8ec22cfb23796f1f777608c0e466ef52600")


def repack(source: Path, destination: Path) -> Plan:
    """Coalesce adjacent metadata batches without changing one packed operation."""
    original = Plan(source)
    destination.mkdir(parents=True, exist_ok=False)
    digest, stream_digest = hashlib.sha256(), hashlib.sha256()
    old_operations = old_slots = new_slots = count = 0
    pending = None
    with (destination / "batches.jsonl").open("wb") as output:
        def flush():
            nonlocal pending, count, new_slots
            if pending is None:
                return
            payload = pending.pop("payload")
            pending["index"] = count
            pending["data"] = calldata(payload)
            slots = sorted(set(pending["slots"]))
            if len(slots) != len(pending["slots"]):
                raise ValueError("source batches have overlapping targets; coalescing requires disjoint metadata operations")
            pending["slots"] = slots
            raw = (json.dumps(pending, separators=(",", ":")) + "\n").encode()
            output.write(raw)
            digest.update(raw)
            new_slots += len(slots)
            count += 1
            pending = None

        for row in original.batches():
            payload = decode(["bytes"], bytes.fromhex(row["data"][2:])[4:])[0]
            stream_digest.update(payload)
            old_operations += row["operations"]
            old_slots += len(row["slots"])
            if row["phase"] != "metadata":
                flush()
                pending = {**row, "payload": payload}
                flush()
                continue
            if pending is not None and (pending["phase"] != row["phase"]
                    or pending["operations"] + row["operations"] > 1024
                    or len(pending["slots"]) + len(row["slots"]) > 1024
                    or len(pending["payload"]) + len(payload) > 16000):
                flush()
            if pending is None:
                pending = {**row, "slots": list(row["slots"]), "queues": list(row["queues"]), "payload": payload}
            else:
                pending["payload"] += payload
                pending["operations"] += row["operations"]
                pending["slots"].extend(row["slots"])
        flush()
    if new_slots != old_slots or old_operations != sum(original.manifest["operationCounts"].values()):
        raise ValueError("coalesced operation/target counts differ from source plan")
    manifest = {key: value for key, value in original.manifest.items()
                if key not in {"batchCount", "batchesSha256", "batchPrefixTouches", "metadataLocalityBound", "optimality"}}
    manifest.update({"batchCount": count, "batchesSha256": digest.hexdigest(),
        "sourcePlanSha256": original.plan_hash,
        "limits": {**manifest["limits"], "maxOperations": 1024, "maxWrites": 1024},
        "repacking": {"method": "coalesce consecutive original metadata batches; queue batches unchanged",
                      "sourceBatches": original.batch_count, "operations": old_operations,
                      "slotReferences": new_slots, "packedOperationStreamSha256": stream_digest.hexdigest()}})
    durable_json(destination / "manifest.json", manifest)
    result = Plan(destination)
    # Independently authenticate the resulting packed stream, not just its counts.
    emitted = hashlib.sha256()
    for row in result.batches():
        emitted.update(decode(["bytes"], bytes.fromhex(row["data"][2:])[4:])[0])
    if emitted.digest() != stream_digest.digest():
        raise ValueError("coalescing changed the packed operation stream")
    return result


def setup(args, plan: Plan, key: str) -> dict:
    rpc = Rpc(args.rpc_url, 120)
    if int(rpc.call("eth_chainId"), 16) != 31337:
        raise ValueError("rehearsal requires local chain ID 31337")
    roots = rpc.call("filecoin_getStateRoots")
    if plan.manifest["source"]["preparedStateRoot"] not in roots.values():
        raise ValueError("node archive does not match source plan")
    build = json.loads(args.facet_build.read_text())
    artifact = json.loads(args.facet_artifact.read_text())
    code = bytes.fromhex(artifact["bytecode"]["object"].removeprefix("0x"))
    if hashlib.sha256(code).hexdigest() != build["creationSha256"]:
        raise ValueError("artifact differs from pinned facet build")
    fwss = plan.fwss
    version_data = "0x" + keccak(b"VERSION()")[:4].hex()
    version = decode(["string"], bytes.fromhex(rpc.call("eth_call", {"to": fwss, "data": version_data}, "latest")[2:]))[0]
    if version != "1.5.0":
        raise ValueError(f"unexpected baseline FWSS version {version}")
    account = Account.from_key(key)
    fee = max(int(rpc.call("eth_gasPrice"), 16), 2000)
    signed = Account.sign_transaction({"type": 2, "chainId": 31337,
        "nonce": int(rpc.call("eth_getTransactionCount", account.address, "latest"), 16),
        "gas": 1_000_000_000, "maxFeePerGas": fee, "maxPriorityFeePerGas": 1,
        "to": b"", "value": 0, "data": code, "accessList": []}, key)
    tx = rpc.call("eth_sendRawTransaction", "0x" + bytes(signed.raw_transaction).hex())
    receipt = rpc.call("eth_getTransactionReceipt", tx)
    if receipt is None or int(receipt["status"], 16) != 1:
        raise RuntimeError("facet deployment failed or instant mining is disabled")
    facet = receipt["contractAddress"]
    runtime = bytes.fromhex(rpc.call("eth_getCode", facet, "latest")[2:])
    if "0x" + keccak(runtime).hex() != build["facetCodeHash"]:
        raise ValueError("adopted facet runtime differs from pinned build")
    selector = keccak(SIGNATURE)[:4]
    route = "0x" + keccak(selector.ljust(32, b"\0") + DELEGATES_ROOT).hex()
    rpc.call("anvil_setStorageAt", fwss, route, "0x" + bytes.fromhex(facet[2:]).rjust(32, b"\0").hex())
    rpc.call("evm_mine")
    result = {"fwss": fwss, "facet": facet, "facetCodeHash": build["facetCodeHash"],
        "sourcePlanSha256": plan.manifest["sourcePlanSha256"], "planSha256": plan.plan_hash,
        "initialRoots": roots, "baselineRoots": rpc.call("filecoin_getStateRoots"),
        "routeSlot": route, "source": plan.manifest["source"], "candidate": build,
        "baselineFwss": version, "deploymentReceipt": receipt,
        "nodeBinarySha256": hashlib.sha256(args.node_binary.read_bytes()).hexdigest(),
        "startedAtUnix": time.time(), "labOnly": True}
    durable_json(args.output / "setup.json", result)
    return result


class MeasuredCleanup(Cleanup):
    def __init__(self, *args, output: Path, **kwargs):
        super().__init__(*args, **kwargs)
        self.output = output
        self.trace_dir = output / "traces"
        self.trace_dir.mkdir(exist_ok=True)
        self.completed = set()
        self.total_gas = self.total_limit = 0
        timings = output / "messages.jsonl"
        if timings.exists():
            for line in timings.open():
                row = json.loads(line)
                self.completed.add(row["index"])
                self.total_gas += row["gasUsed"]
                self.total_limit += row["gasLimit"]
        self.timing_stream = timings.open("a", buffering=1)
        self.log = (output / "execution.log").open("a", buffering=1)
        self.previous_phase = None

    def execute_batch(self, batch):
        start = time.monotonic()
        with contextlib.redirect_stderr(self.log):
            saved = super().execute_batch(batch)
        if batch["index"] not in self.completed:
            self.total_gas += saved["gasUsed"]
            self.total_limit += saved["gasLimit"]
            row = {"index": batch["index"], "phase": batch["phase"],
                "operations": batch["operations"], "slots": len(batch["slots"]),
                "gasUsed": saved["gasUsed"], "gasLimit": saved["gasLimit"],
                "gasEstimate": saved["gasEstimate"], "signedBytes": saved["signedBytes"],
                "elapsedSeconds": time.monotonic() - start, "completedAtUnix": time.time(),
                "cumulativeGasUsed": self.total_gas, "cumulativeGasLimit": self.total_limit}
            self.timing_stream.write(json.dumps(row, separators=(",", ":")) + "\n")
            if batch["index"] % 100 == 0:
                os.fsync(self.timing_stream.fileno())
            self.completed.add(batch["index"])
        if batch["index"] % 1000 == 0 or batch["phase"] != self.previous_phase or batch["index"] == self.plan.batch_count - 1:
            trace = self.rpc.call("filecoin_getGasTrace", saved["transactionHash"])
            actual = int(trace["gasUsed"], 16) if isinstance(trace["gasUsed"], str) else trace["gasUsed"]
            if actual != saved["gasUsed"]:
                raise RuntimeError("checkpoint trace gas differs from receipt")
            with gzip.open(self.trace_dir / f"batch-{batch['index']:012d}.json.gz", "wt") as stream:
                json.dump(trace, stream)
            print(json.dumps({"completed": batch["index"] + 1, "total": self.plan.batch_count,
                              "phase": batch["phase"], "gasUsed": self.total_gas, "gasLimit": self.total_limit}), flush=True)
        self.previous_phase = batch["phase"]
        return saved


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-plan", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--facet-artifact", type=Path, required=True)
    parser.add_argument("--facet-build", type=Path, required=True)
    parser.add_argument("--node-binary", type=Path, default=Path("target/release/fvm-anvil"))
    parser.add_argument("--rpc-url", required=True)
    args = parser.parse_args()
    key = os.environ.get("FWSS_CLEANUP_PRIVATE_KEY")
    if not key:
        parser.error("set FWSS_CLEANUP_PRIVATE_KEY to the funded local signer")
    args.output.mkdir(parents=True, exist_ok=True)
    if (args.output / "plan").exists():
        plan = Plan(args.output / "plan")
    else:
        plan = repack(args.source_plan, args.output / "plan")
    if (args.output / "setup.json").exists():
        prepared = json.loads((args.output / "setup.json").read_text())
    else:
        prepared = setup(args, plan, key)
    executor_args = argparse.Namespace(action="execute", expected_chain_id=31337,
        expected_facet_code_hash=prepared["facetCodeHash"], rpc_url=args.rpc_url,
        rpc_timeout=120, read_batch_size=128, confirmations=1, receipt_timeout=300,
        poll_interval=1, max_gas=8_000_000_000, max_tx_bytes=32768,
        gas_margin_percent=20, priority_fee_per_gas=1, max_fee_per_gas=None,
        rebroadcast_pending=False)
    start = time.monotonic()
    with Journal(args.output / "journal", plan, executor_args, Account.from_key(key).address.lower()) as journal:
        cleanup = MeasuredCleanup(executor_args, plan, journal, key, output=args.output)
        try:
            result = cleanup.run()
            result["elapsedSeconds"] = time.monotonic() - start
            result["wallSecondsSinceSetup"] = time.time() - prepared["startedAtUnix"]
            result["finalRoots"] = cleanup.rpc.call("filecoin_getStateRoots")
            durable_json(args.output / "execution.json", result)
            print(json.dumps(result, sort_keys=True), flush=True)
        finally:
            cleanup.timing_stream.close()
            cleanup.log.close()


if __name__ == "__main__":
    main()
