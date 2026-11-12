#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Compare gas per cleaned slot for random versus global-queue selections on FEVM."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
from collections import Counter, defaultdict
from pathlib import Path

from eth_abi import decode, encode
from eth_account import Account
from eth_hash.auto import keccak

from cleanup_execute import Rpc
from cleanup_plan import SIGNATURE, calldata, data_slot, mapping_slot, piece_operations, slot_hex, word
from traces import summarize_trace

DELEGATES_ROOT = bytes.fromhex("f27774d37a8b3bf2306f60b561e4e8ec22cfb23796f1f777608c0e466ef52600")


def unpack(payload: bytes):
    offset = 0
    while offset < len(payload):
        start = offset
        kind = payload[offset]
        dataset = int.from_bytes(payload[offset + 1:offset + 6], "big")
        piece = int.from_bytes(payload[offset + 6:offset + 11], "big")
        offset += 11
        if kind == 2:
            slots = [mapping_slot(piece, mapping_slot(dataset, 14))]
        elif kind in (3, 5):
            index = payload[offset]
            offset += 1
            head = data_slot(mapping_slot(piece, mapping_slot(dataset, 14))) + index
            slots = [head if kind == 3 else data_slot(head)]
        elif kind in (0, 1):
            count = payload[offset] if kind == 1 else 0
            offset += int(kind == 1)
            length = payload[offset]
            offset += 1
            key = payload[offset:offset + length]
            offset += length
            base = mapping_slot(piece, mapping_slot(dataset, 13))
            head = int.from_bytes(keccak(key + word(base)), "big")
            slots = [head] if kind == 0 else [data_slot(head) + i for i in range(count)]
        else:
            raise ValueError(f"unexpected metadata operation {kind}")
        if offset > len(payload):
            raise ValueError("truncated packed operation")
        yield {"dataset": dataset, "piece": piece, "kind": kind, "packed": payload[start:offset], "slots": slots}


def global_frontier(plan: Path, dataset: int, words: int):
    owners, ops = set(), []
    writes = 0
    with (plan / "batches.jsonl").open() as stream:
        for line in stream:
            batch = json.loads(line)
            if batch["phase"] != "metadata":
                continue
            payload = decode(["bytes"], bytes.fromhex(batch["data"][2:])[4:])[0]
            for op in unpack(payload):
                if op["dataset"] != dataset:
                    continue
                owners.add(op["piece"])
                ops.append(op)
                writes += len(op["slots"])
                if writes >= words:
                    return owners, ops
    raise ValueError("not enough global-queue words in dataset")


def select(inventory: Path, plan: Path, dataset: int, pieces: int, words: int, seed: int):
    summary = json.loads((inventory / "summary.json").read_text())
    population = next(row["piecesWithMetadata"] for row in summary["datasets"] if row["dataSetId"] == dataset)
    rng = random.Random(seed)
    positions = set(rng.sample(range(population), pieces))
    owners, frontier = global_frontier(plan, dataset, words)
    random_rows, global_rows = [], []
    index = 0
    digest = hashlib.sha256()
    with (inventory / "pieces.jsonl").open("rb") as stream:
        for line in stream:
            digest.update(line)
            row = json.loads(line)
            if row["dataSetId"] != dataset:
                continue
            if index in positions:
                random_rows.append(row)
            if row["pieceId"] in owners:
                global_rows.append(row)
            index += 1
    manifest = json.loads((plan / "manifest.json").read_text())
    if digest.hexdigest() != manifest["inventorySha256"]["pieces.jsonl"]:
        raise ValueError("inventory digest no longer matches the frozen global plan")
    if index != population or len(global_rows) != len(owners) or len(random_rows) != pieces:
        raise ValueError("inventory selection counts do not reconcile")
    rng.shuffle(random_rows)

    def ops_for(rows):
        return [{"dataset": row["dataSetId"], "piece": row["pieceId"], "kind": packed[0],
                 "packed": packed, "slots": slots}
                for row in rows for _, packed, slots in piece_operations(row)]

    natural = ops_for(random_rows)
    profile = lambda op: (op["kind"], len(op["packed"]), len(op["slots"]))
    needs = Counter(profile(op) for op in frontier)
    candidates = defaultdict(list)
    for op in natural:
        candidates[profile(op)].append(op)
    chosen = set()
    for group, count in needs.items():
        if len(candidates[group]) < count:
            raise ValueError("random sample cannot match frontier's operation/byte/write profile")
        chosen.update(op["packed"] for op in rng.sample(candidates[group], count))
    matched = [op for op in natural if op["packed"] in chosen]
    if Counter(profile(op) for op in matched) != needs:
        raise ValueError("matched random operation profile differs")
    ordered = lambda ops: sorted(ops, key=lambda op: (min(op["slots"]), op["packed"]))
    cases = [
        ("random-matched-piece-order", matched, random_rows, 256),
        ("random-matched-slot-order", ordered(matched), random_rows, 256),
        ("global-frontier-256", frontier, global_rows, 256),
        ("global-frontier-512", frontier, global_rows, 512),
        ("global-frontier-1024", frontier, global_rows, 1024),
    ]
    return cases, {"dataset": dataset, "population": population, "randomPoolPieces": pieces,
                   "requestedSlots": words, "actualSlots": sum(len(op["slots"]) for op in frontier), "seed": seed,
                   "inventorySha256": digest.hexdigest(), "randomPieceIds": [r["pieceId"] for r in random_rows],
                   "queueOwnerPieceIds": [r["pieceId"] for r in global_rows]}


def batches(ops, max_writes):
    current, writes, size = [], 0, 0
    for op in ops:
        if current and (writes + len(op["slots"]) > max_writes or size + len(op["packed"]) > 16000):
            yield current
            current, writes, size = [], 0, 0
        current.append(op)
        writes += len(op["slots"])
        size += len(op["packed"])
    if current:
        yield current


def read_words(rpc, address, slots):
    values = {}
    sequence = list(slots)
    for start in range(0, len(sequence), 128):
        chunk = sequence[start:start + 128]
        result = rpc.batch([("eth_getStorageAt", [address, hex(slot), "latest"]) for slot in chunk])
        values.update(zip(chunk, (int(value, 16) for value in result), strict=True))
    return values


def run(args):
    if args.output.exists():
        raise ValueError("use a new output directory; completed measurements are immutable")
    cases, selection = select(args.inventory, args.plan, args.dataset, args.pieces, args.slots, args.seed)
    args.output.mkdir(parents=True)
    manifest = json.loads((args.plan / "manifest.json").read_text())
    build = json.loads(args.facet_build.read_text())
    artifact = json.loads(args.facet_artifact.read_text())
    code = bytes.fromhex(artifact["bytecode"]["object"].removeprefix("0x"))
    if hashlib.sha256(code).hexdigest() != build["creationSha256"]:
        raise ValueError("facet artifact differs from recorded pinned build")
    rpc = Rpc(args.rpc_url, 120)
    if int(rpc.call("eth_chainId"), 16) != 31337:
        raise ValueError("lab-only runner requires chain ID 31337")
    roots = rpc.call("filecoin_getStateRoots")
    if manifest["source"]["preparedStateRoot"] not in roots.values():
        raise ValueError("node does not use the plan's prepared archive")
    fwss = manifest["fwss"]
    key = os.environ.get("FWSS_CLEANUP_PRIVATE_KEY")
    if not key:
        raise ValueError("set FWSS_CLEANUP_PRIVATE_KEY to the local funded signer")
    sender = Account.from_key(key).address
    original = rpc.call("anvil_snapshot")
    report = {"selection": selection, "source": manifest["source"], "candidate": build,
              "labOnly": True, "cases": []}

    def send(data, to=None):
        signed = Account.sign_transaction({"type": 2, "chainId": 31337,
            "nonce": int(rpc.call("eth_getTransactionCount", sender, "latest"), 16),
            "gas": 8_000_000_000, "maxFeePerGas": 2000, "maxPriorityFeePerGas": 1,
            "to": bytes.fromhex(to[2:]) if to else b"", "value": 0,
            "data": data, "accessList": []}, key)
        if len(signed.raw_transaction) > 32768:
            raise ValueError("signed envelope exceeds experiment size cap")
        tx = rpc.call("eth_sendRawTransaction", "0x" + bytes(signed.raw_transaction).hex())
        receipt = rpc.call("eth_getTransactionReceipt", tx)
        if receipt is None:
            raise RuntimeError("lab runner requires instant mining")
        return tx, receipt, len(signed.raw_transaction)

    try:
        # Validate baseline via the public proxy before changing the route.
        version_data = "0x" + keccak(b"VERSION()")[:4].hex()
        version = decode(["string"], bytes.fromhex(rpc.call("eth_call", {"to": fwss, "data": version_data}, "latest")[2:]))[0]
        if version != "1.5.0":
            raise ValueError(f"unexpected baseline FWSS version {version}")
        report["baselineFwss"] = version
        _, deployment, _ = send(code)
        if int(deployment["status"], 16) != 1:
            raise RuntimeError("facet deployment reverted")
        facet = deployment["contractAddress"]
        runtime = bytes.fromhex(rpc.call("eth_getCode", facet, "latest")[2:])
        if "0x" + keccak(runtime).hex() != build["facetCodeHash"]:
            raise ValueError("deployed runtime hash mismatch")
        selector = keccak(SIGNATURE)[:4]
        route = keccak(selector.ljust(32, b"\0") + DELEGATES_ROOT)
        rpc.call("anvil_setStorageAt", fwss, "0x" + route.hex(), slot_hex(int(facet, 16)))
        rpc.call("evm_mine")
        route_data = keccak(b"implementation(bytes4)")[:4] + encode(["bytes4"], [selector])
        adopted = decode(["address"], bytes.fromhex(rpc.call("eth_call", {"to": fwss, "data": "0x" + route_data.hex()}, "latest")[2:]))[0]
        if adopted.lower() != facet.lower():
            raise ValueError("cleanup route not adopted")
        baseline = rpc.call("filecoin_getStateRoots")
        sentinels = read_words(rpc, fwss, (0, 7, 11, 12, 13, 14, 22, 23))
        for name, ops, rows, limit in cases:
            snapshot = rpc.call("anvil_snapshot")
            targets = {slot for op in ops for slot in op["slots"]}
            siblings = {int(s["slot"], 16): int(s["value"], 16) for row in rows for s in row["slots"] if int(s["slot"], 16) not in targets}
            expected = {int(s["slot"], 16): int(s["value"], 16) for row in rows for s in row["slots"] if int(s["slot"], 16) in targets}
            if read_words(rpc, fwss, targets) != expected:
                raise ValueError(f"{name}: selected source words differ from imported state")
            result = {"name": name, "maxWrites": limit, "operations": len(ops), "words": len(targets),
                      "touchedPieces": len({op["piece"] for op in ops}),
                      "completePieces": sum(all(int(s["slot"], 16) in targets for s in row["slots"]) for row in rows),
                      "operationCounts": dict(Counter(str(op["kind"]) for op in ops)), "transactions": []}
            result["packedBytes"] = sum(len(op["packed"]) for op in ops)
            try:
                for index, batch in enumerate(batches(ops, limit)):
                    data = bytes.fromhex(calldata(b"".join(op["packed"] for op in batch))[2:])
                    tx, receipt, size = send(data, fwss)
                    trace = rpc.call("filecoin_getGasTrace", tx)
                    record = {"hash": tx, "receipt": receipt, "signedBytes": size, "calldata": "0x" + data.hex(),
                              "traceSummary": summarize_trace(trace, {fwss: "FWSS-cleanup"})}
                    (args.output / f"{name}-{index}.json").write_text(json.dumps({**record, "trace": trace}) + "\n")
                    result["transactions"].append(record)
                    if int(receipt["status"], 16) != 1:
                        raise RuntimeError(f"{name}/{index}: contract revert or FVM out of gas; preserved receipt/trace")
                if any(read_words(rpc, fwss, targets).values()):
                    raise RuntimeError(f"{name}: cleanup targets remain nonzero")
                if read_words(rpc, fwss, siblings) != siblings or read_words(rpc, fwss, sentinels) != sentinels:
                    raise RuntimeError(f"{name}: non-target state changed")
                result["gasUsed"] = sum(int(t["receipt"]["gasUsed"], 16) for t in result["transactions"])
                result["gasPerSlot"] = result["gasUsed"] / result["words"]
                result["outcome"] = "success"
                result["preservedSiblingWords"] = len(siblings)
                report["cases"].append(result)
                print(json.dumps({k: v for k, v in result.items() if k != "transactions"}), flush=True)
            finally:
                if rpc.call("anvil_revert", snapshot) is not True or rpc.call("filecoin_getStateRoots") != baseline:
                    raise RuntimeError("case baseline roots not restored")
        report["restoration"] = "Exact baseline roots restored between every case"
    finally:
        if rpc.call("anvil_revert", original) is not True or rpc.call("filecoin_getStateRoots") != roots:
            raise RuntimeError("original imported roots not restored")
        report["originalRestoration"] = "Exact original roots restored"
        (args.output / "comparison.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--facet-artifact", type=Path, required=True)
    parser.add_argument("--facet-build", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rpc-url", required=True)
    parser.add_argument("--dataset", type=int, default=1293)
    parser.add_argument("--random-pieces", dest="pieces", type=int, default=1024)
    parser.add_argument("--slots", type=int, default=2560)
    parser.add_argument("--seed", type=int, default=658)
    args = parser.parse_args()
    if args.pieces <= 0 or args.slots <= 0:
        parser.error("--random-pieces and --slots must be positive")
    run(args)


if __name__ == "__main__":
    main()
