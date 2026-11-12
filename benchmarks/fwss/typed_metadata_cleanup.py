#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Measure metadata-identity batches, never caller-selected storage writes."""

from __future__ import annotations

import argparse
import hashlib
import heapq
import itertools
import json
from pathlib import Path

from chain import Chain, command, quantity
from eth_abi import encode
from eth_account import Account
from eth_hash.auto import keccak
from metadata_cleanup import (
    DEFAULT_KEY,
    FWSS_PROXY,
    GAS_LIMIT,
    Runner,
    atomic_json,
    solidity_array,
)
from traces import summarize_trace

SIGNATURE = "cleanup((uint32,uint32,uint8,bytes)[])"


def word(value: int) -> bytes:
    return value.to_bytes(32, "big")


def mapping_slot(identifier: int, root: int) -> int:
    return int.from_bytes(keccak(word(identifier) + word(root)), "big")


def string_slots(root: int, value: bytes) -> set[int]:
    slots = {root}
    if len(value) >= 32:
        first = int.from_bytes(keccak(word(root)), "big")
        slots.update((first + i) % (1 << 256) for i in range((len(value) + 31) // 32))
    return slots


def anchor(words: list[dict]) -> str:
    """Start of the longest occupied contiguous span; lowest start breaks ties."""
    slots = sorted(int(item["slot"], 16) for item in words)
    best_start = start = previous = slots[0]
    best_length = length = 1
    for slot in slots[1:]:
        if slot == previous + 1:
            length += 1
        else:
            start, length = slot, 1
        if length > best_length:
            best_start, best_length = start, length
        previous = slot
    return "0x" + word(best_start).hex()


def operations(row: dict) -> list[dict]:
    dataset, piece = row["dataSetId"], row["pieceId"]
    remaining = {int(item["slot"], 16): item for item in row["slots"]}
    base = mapping_slot(piece, mapping_slot(dataset, 13))
    result = []
    for entry in row["keys"]:
        key = bytes.fromhex(entry["keyHex"][2:])
        root = int.from_bytes(keccak(key + word(base)), "big")
        slots = string_slots(root, bytes.fromhex(entry["valueHex"][2:]))
        owned = [remaining.pop(slot) for slot in sorted(slots) if slot in remaining]
        # Empty values still get an operation; use their derived header as scheduling anchor.
        result.append(
            {
                "dataSetId": dataset,
                "pieceId": piece,
                "list": False,
                "keyHex": entry["keyHex"],
                "words": owned,
                "anchor": anchor(owned) if owned else "0x" + word(root).hex(),
            }
        )
    owned = list(remaining.values())
    result.append(
        {
            "dataSetId": dataset,
            "pieceId": piece,
            "list": True,
            "keyHex": "0x",
            "words": owned,
            "anchor": anchor(owned),
        }
    )
    return result


def calldata(batch: list[dict], *, packed: bool = False) -> bytes:
    if packed:
        payload = bytearray()
        for op in batch:
            key = bytes.fromhex(op["keyHex"][2:])
            if len(key) > 65535:
                raise ValueError("packed key exceeds uint16 length")
            payload.extend(op["dataSetId"].to_bytes(4, "big"))
            payload.extend(op["pieceId"].to_bytes(4, "big"))
            payload.append(op.get("target", int(op["list"])))
            payload.extend(len(key).to_bytes(2, "big"))
            payload.extend(key)
        return keccak(b"cleanupPacked(bytes)")[:4] + encode(["bytes"], [bytes(payload)])
    tuples = [
        (
            op["dataSetId"],
            op["pieceId"],
            op.get("target", int(op["list"])),
            bytes.fromhex(op["keyHex"][2:]),
        )
        for op in batch
    ]
    return keccak(SIGNATURE.encode())[:4] + encode(
        ["(uint32,uint32,uint8,bytes)[]"], [tuples]
    )


def chunks(items: list, size: int):
    for offset in range(0, len(items), size):
        yield items[offset : offset + size]


def check_words(chain: Chain, words: list[dict], *, zero: bool = False) -> None:
    for batch in chunks(words, 128):
        slots = solidity_array(item["slot"] for item in batch)
        if zero:
            result = chain.call(FWSS_PROXY, "slotsAreZero(bytes32[])(bool)", slots)
        else:
            result = chain.call(
                FWSS_PROXY,
                "slotsMatch(bytes32[],bytes32[])(bool)",
                slots,
                solidity_array(item["value"] for item in batch),
            )
        if result.strip() != "true":
            raise RuntimeError("storage verification failed")


def submit(chain: Chain, data: bytes) -> tuple[str, dict, int]:
    signed = Account.sign_transaction(
        {
            "type": 2,
            "chainId": chain.chain_id,
            "nonce": quantity(
                chain.rpc("eth_getTransactionCount", chain.sender, "pending")
            ),
            "gas": GAS_LIMIT,
            "maxFeePerGas": chain.max_fee_per_gas,
            "maxPriorityFeePerGas": 1,
            "to": bytes.fromhex(FWSS_PROXY[2:]),
            "value": 0,
            "data": data,
            "accessList": [],
        },
        chain.private_key,
    )
    raw = bytes(signed.raw_transaction)
    tx = chain.rpc_http("eth_sendRawTransaction", ["0x" + raw.hex()])
    receipt = json.loads(
        chain.cast("receipt", "--rpc-url", chain.rpc_url, "--json", tx)
    )
    return tx, receipt, len(raw)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18549")
    parser.add_argument("--pieces", type=int, default=512)
    parser.add_argument("--batch-size", type=int, default=128)
    args = parser.parse_args()
    if args.pieces <= 0 or args.batch_size <= 0:
        parser.error("pieces and batch-size must be positive")
    if args.output.exists():
        parser.error("use a fresh output directory to preserve measurements")
    args.output.mkdir(parents=True)
    source = args.inventory / "pieces.jsonl"
    original_stat = source.stat()
    natural, natural_rows, heap, sequence = [], {}, [], itertools.count()
    digest = hashlib.sha256()
    piece_count = key_count = word_count = maximum_dataset = maximum_piece = (
        maximum_key_bytes
    ) = 0
    with source.open("rb") as stream:
        for line in stream:
            digest.update(line)
            row = json.loads(line)
            piece_count += 1
            dataset, piece = row["dataSetId"], row["pieceId"]
            if not 0 <= dataset < 2**32 or not 0 <= piece < 2**32:
                raise RuntimeError("snapshot IDs do not fit uint32")
            maximum_dataset = max(maximum_dataset, dataset)
            maximum_piece = max(maximum_piece, piece)
            key_count += len(row["keys"])
            word_count += len(row["slots"])
            maximum_key_bytes = max(
                maximum_key_bytes,
                *(len(entry["keyHex"][2:]) // 2 for entry in row["keys"]),
            )
            if piece_count <= args.pieces:
                ops = operations(row)
                natural.extend(ops)
                natural_rows[dataset, piece] = row
            else:
                if (
                    len(heap) == len(natural)
                    and min(item["slot"] for item in row["slots"])
                    > "0x" + word(-heap[0][0]).hex()
                ):
                    continue
                ops = operations(row)
            if piece_count < args.pieces:
                continue
            candidates = natural if piece_count == args.pieces else ops
            budget = len(natural)
            for op in candidates:
                if not op["words"]:
                    continue
                priority = int(op["anchor"], 16)
                owner = (
                    natural_rows[op["dataSetId"], op["pieceId"]]
                    if piece_count == args.pieces
                    else row
                )
                candidate = (-priority, next(sequence), op, owner)
                if len(heap) < budget:
                    heapq.heappush(heap, candidate)
                elif priority < -heap[0][0]:
                    heapq.heapreplace(heap, candidate)
    if source.stat() != original_stat:
        raise RuntimeError("inventory changed while scanning")
    inventory_summary = json.loads((args.inventory / "summary.json").read_text())
    if (piece_count, key_count, word_count) != (
        inventory_summary["piecesWithMetadata"],
        inventory_summary["keys"],
        inventory_summary["nonzeroWords"],
    ):
        raise RuntimeError("inventory counts mismatch")
    if piece_count < args.pieces:
        raise RuntimeError("not enough pieces")
    global_ops = sorted((item[2] for item in heap), key=lambda op: op["anchor"])
    cases = [
        ("typed-natural", natural, False),
        (
            "typed-sorted-same-pieces",
            sorted(natural, key=lambda op: op["anchor"]),
            False,
        ),
        (
            "packed-sorted-same-pieces",
            sorted(natural, key=lambda op: op["anchor"]),
            True,
        ),
        ("packed-global-prefix", global_ops, True),
    ]
    global_targets = {item["slot"] for op in global_ops for item in op["words"]}
    siblings = {
        item["slot"]: item
        for entry in heap
        for item in entry[3]["slots"]
        if item["slot"] not in global_targets
    }
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
    original_code = chain.rpc("eth_getCode", FWSS_PROXY, "latest")
    original_sentinels = [
        {
            "slot": "0x" + word(i).hex(),
            "value": chain.rpc("eth_getStorageAt", FWSS_PROXY, hex(i), "latest"),
        }
        for i in (0, 11, 12)
    ]
    snapshot = chain.snapshot()
    report = {
        "labOnly": True,
        "sourceSha256": digest.hexdigest(),
        "sourcePieces": piece_count,
        "maximumDataSetId": maximum_dataset,
        "maximumPieceId": maximum_piece,
        "maximumKeyBytes": maximum_key_bytes,
        "stateRoots": roots,
        "batchSize": args.batch_size,
        "cases": [],
    }
    try:
        deployment, _ = chain.deploy(
            creation + chain.abi_encode("constructor(address)", chain.sender)[2:]
        )
        runtime = chain.rpc("eth_getCode", deployment, "latest")
        chain.rpc("anvil_setCode", FWSS_PROXY, runtime)
        if chain.rpc("eth_getCode", FWSS_PROXY, "latest") != runtime:
            raise RuntimeError("typed runtime installation failed")
        report["runtimeSha256"] = hashlib.sha256(bytes.fromhex(runtime[2:])).hexdigest()
        report["originalCodeSha256"] = hashlib.sha256(
            bytes.fromhex(original_code[2:])
        ).hexdigest()
        baseline = chain.snapshot()
        for name, ops, packed in cases:
            chain.revert(baseline)
            baseline = chain.snapshot()
            targets = {item["slot"]: item for op in ops for item in op["words"]}
            sentinels = original_sentinels + (
                list(siblings.values()) if name == "packed-global-prefix" else []
            )
            check_words(chain, list(targets.values()) + sentinels)
            atomic_json(args.output / f"{name}-selection.json", ops)
            transactions = []
            for index, batch in enumerate(chunks(ops, args.batch_size)):
                data = calldata(batch, packed=packed)
                tx, receipt, raw_bytes = submit(chain, data)
                trace = chain.trace(tx)
                trace_summary = summarize_trace(
                    trace, {FWSS_PROXY: "typed-metadata-cleanup"}
                )
                record = {
                    "hash": tx,
                    "receipt": receipt,
                    "operations": len(batch),
                    "calldataBytes": len(data),
                    "calldataZeroBytes": data.count(0),
                    "signedTransactionBytes": raw_bytes,
                    "ipldCharges": Runner._ipld_charges(trace_summary),
                    "traceSummary": trace_summary,
                }
                atomic_json(
                    args.output / f"{name}-{index}.json", {**record, "trace": trace}
                )
                if quantity(receipt["status"]) != 1:
                    raise RuntimeError(f"{name}/{index} reverted")
                transactions.append(record)
            check_words(chain, list(targets.values()), zero=True)
            check_words(chain, sentinels)
            report["cases"].append(
                {
                    "name": name,
                    "operations": len(ops),
                    "valueOperations": sum(not op["list"] for op in ops),
                    "listOperations": sum(op["list"] for op in ops),
                    "pieces": len({(op["dataSetId"], op["pieceId"]) for op in ops}),
                    "datasets": len({op["dataSetId"] for op in ops}),
                    "targetWords": len(targets),
                    "preservedSentinels": len(sentinels),
                    "gasUsed": sum(
                        quantity(tx["receipt"]["gasUsed"]) for tx in transactions
                    ),
                    "calldataBytes": sum(tx["calldataBytes"] for tx in transactions),
                    "signedTransactionBytes": sum(
                        tx["signedTransactionBytes"] for tx in transactions
                    ),
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
                or chain.rpc("eth_getCode", FWSS_PROXY, "latest") != original_code
            ):
                raise RuntimeError("exact imported state/runtime restoration failed")
            report["restoration"] = "exact state roots and original runtime restored"
        finally:
            atomic_json(args.output / "summary.json", report)
    print(json.dumps({"output": str(args.output), "outcome": report["outcome"]}))


if __name__ == "__main__":
    main()
