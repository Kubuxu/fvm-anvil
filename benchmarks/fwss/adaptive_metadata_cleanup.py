#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Dependency-aware metadata cleanup: interleave payload clears and whole-object deletes."""

from __future__ import annotations

import argparse
import hashlib
import heapq
import itertools
import json
from pathlib import Path

from chain import Chain, command, quantity
from eth_hash.auto import keccak
from metadata_cleanup import DEFAULT_KEY, FWSS_PROXY, GAS_LIMIT, Runner, atomic_json
from traces import summarize_trace
from typed_metadata_cleanup import (
    calldata,
    check_words,
    mapping_slot,
    operations,
    submit,
    word,
)


def ready_operations(row: dict) -> list[dict]:
    """Only modified words determine priority; unchanged headers remain preservation checks."""
    result = []
    for op in operations(row):
        if not op["words"]:
            continue
        op = {**op, "target": int(op["list"]), "preserveWords": []}
        if not op["list"]:
            base = mapping_slot(op["pieceId"], mapping_slot(op["dataSetId"], 13))
            header_slot = (
                "0x" + keccak(bytes.fromhex(op["keyHex"][2:]) + word(base)).hex()
            )
            header = next(
                entry for entry in op["words"] if entry["slot"] == header_slot
            )
            payload = [entry for entry in op["words"] if entry["slot"] != header_slot]
            if payload:
                deletion = {**op, "words": [header], "priority": header_slot}
                op = {
                    **op,
                    "target": 2,
                    "words": payload,
                    "preserveWords": [header],
                    "followup": deletion,
                }
        # Max, not one representative anchor: all modified words must fit below the priority.
        op["priority"] = max(entry["slot"] for entry in op["words"])
        result.append(op)
    return result


def scheduled_batches(initial: list[dict], size: int, limit: int | None = None):
    """Resume only after the yielded transaction succeeds; only then unlock its deletions."""
    sequence = itertools.count()
    queue = [(op["priority"], next(sequence), op) for op in initial]
    heapq.heapify(queue)
    submitted = 0
    while queue and (limit is None or submitted < limit):
        count = min(size, len(queue), limit - submitted if limit is not None else size)
        batch = [heapq.heappop(queue)[2] for _ in range(count)]
        yield batch
        submitted += len(batch)
        for op in batch:
            if followup := op.get("followup"):
                heapq.heappush(queue, (followup["priority"], next(sequence), followup))


def plan(
    inventory: Path, pieces: int, budget: int
) -> tuple[list, list, tuple[list, list], dict]:
    source = inventory / "pieces.jsonl"
    stat = source.stat()
    digest = hashlib.sha256()
    heap, natural_rows, natural, sequence = [], [], [], itertools.count()
    counts = {"pieces": 0, "keys": 0, "words": 0}
    for line in source.open("rb"):
        digest.update(line)
        row = json.loads(line)
        if not 0 <= row["dataSetId"] < 2**32 or not 0 <= row["pieceId"] < 2**32:
            raise ValueError("inventory IDs exceed uint32")
        counts["pieces"] += 1
        counts["keys"] += len(row["keys"])
        counts["words"] += len(row["slots"])
        if counts["pieces"] <= pieces:
            natural_rows.append(row)
            natural.extend(operations(row))
        if (
            len(heap) == budget + 1
            and min(entry["slot"] for entry in row["slots"]) > heap[0][2]
        ):
            continue
        for op in ready_operations(row):
            priority = int(op["priority"], 16)
            item = (-priority, next(sequence), op["priority"], op, row)
            if len(heap) < budget + 1:
                heapq.heappush(heap, item)
            elif priority < -heap[0][0]:
                heapq.heapreplace(heap, item)
    summary = json.loads((inventory / "summary.json").read_text())
    if counts != {
        "pieces": summary["piecesWithMetadata"],
        "keys": summary["keys"],
        "words": summary["nonzeroWords"],
    }:
        raise RuntimeError("inventory count mismatch")
    if source.stat() != stat or len(natural_rows) != pieces:
        raise RuntimeError("inventory changed or insufficient pieces")
    initial = [item[3] for item in heap]
    owners = [item[4] for item in heap]
    return (
        natural_rows,
        natural,
        (initial, owners),
        {"sourceSha256": digest.hexdigest(), "counts": counts},
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18553")
    parser.add_argument("--pieces", type=int, default=512)
    parser.add_argument("--operations", type=int, default=1024)
    parser.add_argument("--batch-size", type=int, default=128)
    args = parser.parse_args()
    if min(args.pieces, args.operations, args.batch_size) <= 0:
        parser.error("counts must be positive")
    if args.output.exists():
        parser.error("use a new output directory")
    args.output.mkdir(parents=True)
    natural_rows, natural, (initial, global_owners), provenance = plan(
        args.inventory, args.pieces, args.operations
    )
    contracts = Path(__file__).with_name("contracts")
    command(["forge", "build", "--root", str(contracts)])
    creation = command(
        [
            "forge",
            "inspect",
            "TypedMetadataCleanup",
            "bytecode",
            "--root",
            str(contracts),
        ]
    )
    chain = Chain(args.rpc_url, DEFAULT_KEY, GAS_LIMIT)
    roots = chain.rpc("filecoin_getStateRoots")
    original = chain.rpc("eth_getCode", FWSS_PROXY, "latest")
    snapshot = chain.snapshot()
    report = {
        **provenance,
        "stateRoots": roots,
        "batchSize": args.batch_size,
        "cases": [],
        "labOnly": True,
    }
    try:
        deployment, _ = chain.deploy(
            creation + chain.abi_encode("constructor(address)", chain.sender)[2:]
        )
        runtime = chain.rpc("eth_getCode", deployment, "latest")
        chain.rpc("anvil_setCode", FWSS_PROXY, runtime)
        if chain.rpc("eth_getCode", FWSS_PROXY, "latest") != runtime:
            raise RuntimeError("runtime installation failed")
        report["runtimeSha256"] = hashlib.sha256(bytes.fromhex(runtime[2:])).hexdigest()
        baseline = chain.snapshot()
        adaptive_natural = [op for row in natural_rows for op in ready_operations(row)]
        direct = sorted(natural, key=lambda op: op["anchor"])
        cases = [
            ("direct-same-pieces", direct, natural_rows, None),
            ("adaptive-same-pieces", adaptive_natural, natural_rows, None),
            ("adaptive-global-ready-prefix", initial, global_owners, args.operations),
        ]
        for name, ops, owners, limit in cases:
            chain.revert(baseline)
            baseline = chain.snapshot()
            all_words = {
                entry["slot"]: entry for row in owners for entry in row["slots"]
            }
            check_words(chain, list(all_words.values()))
            sentinels = [
                {
                    "slot": "0x" + word(i).hex(),
                    "value": chain.rpc(
                        "eth_getStorageAt", FWSS_PROXY, hex(i), "latest"
                    ),
                }
                for i in (0, 11, 12)
            ]
            atomic_json(args.output / f"{name}-initial.json", ops)
            batches = (
                iter(
                    [
                        direct[start : start + args.batch_size]
                        for start in range(0, len(direct), args.batch_size)
                    ]
                )
                if name == "direct-same-pieces"
                else scheduled_batches(ops, args.batch_size, limit)
            )
            transactions, executed, modified = [], [], {}
            for index, batch in enumerate(batches):
                data = calldata(batch, packed=True)
                tx, receipt, signed_bytes = submit(chain, data)
                trace = chain.trace(tx)
                trace_summary = summarize_trace(
                    trace, {FWSS_PROXY: "adaptive-metadata-cleanup"}
                )
                record = {
                    "hash": tx,
                    "receipt": receipt,
                    "calldata": "0x" + data.hex(),
                    "calldataBytes": len(data),
                    "signedTransactionBytes": signed_bytes,
                    "operationCounts": {
                        str(target): sum(
                            op.get("target", int(op["list"])) == target for op in batch
                        )
                        for target in range(3)
                    },
                    "ipldCharges": Runner._ipld_charges(trace_summary),
                }
                atomic_json(
                    args.output / f"{name}-{index}.json", {**record, "trace": trace}
                )
                if quantity(receipt["status"]) != 1:
                    raise RuntimeError(
                        f"{name}/{index} reverted; dependent deletes remain locked"
                    )
                writes = [entry for op in batch for entry in op["words"]]
                preserves = [
                    entry for op in batch for entry in op.get("preserveWords", [])
                ]
                check_words(chain, writes, zero=True)
                check_words(chain, preserves)
                modified.update((entry["slot"], entry) for entry in writes)
                executed.extend(batch)
                transactions.append(record)
            remaining = [
                entry for slot, entry in all_words.items() if slot not in modified
            ]
            check_words(chain, list(modified.values()), zero=True)
            check_words(chain, remaining + sentinels)
            if limit is None and modified.keys() != all_words.keys():
                raise RuntimeError(
                    "complete cleanup did not cover every selected metadata word"
                )
            atomic_json(args.output / f"{name}-executed.json", executed)
            report["cases"].append(
                {
                    "name": name,
                    "operations": len(executed),
                    "operationCounts": {
                        str(target): sum(
                            op.get("target", int(op["list"])) == target
                            for op in executed
                        )
                        for target in range(3)
                    },
                    "modifiedWords": len(modified),
                    "preservedOwnerWords": len(remaining),
                    "firstModifiedSlot": min(modified),
                    "lastModifiedSlot": max(modified),
                    "firstBytePrefixes": len({slot[2:4] for slot in modified}),
                    "firstTwoBytePrefixes": len({slot[2:6] for slot in modified}),
                    "gasUsed": sum(
                        quantity(tx["receipt"]["gasUsed"]) for tx in transactions
                    ),
                    "calldataBytes": sum(tx["calldataBytes"] for tx in transactions),
                    "ipldCharges": Runner._sum_ipld_charges(transactions),
                    "transactions": transactions,
                }
            )
        report["outcome"] = "success"
    except BaseException as error:
        report["outcome"], report["error"] = "failure", repr(error)
        raise
    finally:
        try:
            chain.revert(snapshot)
            if (
                chain.rpc("filecoin_getStateRoots") != roots
                or chain.rpc("eth_getCode", FWSS_PROXY, "latest") != original
            ):
                raise RuntimeError("original state/runtime not restored")
            report["restoration"] = "exact original roots and runtime restored"
        finally:
            atomic_json(args.output / "summary.json", report)
    print(json.dumps({"outcome": report["outcome"], "output": str(args.output)}))


if __name__ == "__main__":
    main()
