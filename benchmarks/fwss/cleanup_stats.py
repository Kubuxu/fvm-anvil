#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Report persisted cleanup receipts and theoretical chain capacity, without RPC."""

from __future__ import annotations

import argparse
import csv
import json
import math
import sys
from collections import Counter
from pathlib import Path
from typing import Any

from cleanup_execute import Journal, PHASES, Plan, address, hex_bytes, nonnegative


COLUMNS = (
    "index", "phase", "operations", "slotChecks", "gasEstimate", "gasLimit",
    "gasUsed", "utilization", "signedBytes", "transactionHash", "receiptBlock",
    "elapsedSeconds", "cumulativeGasUsed", "cumulativeGasLimit",
)


def load_object(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_bytes())
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected a JSON object")
    return value


def distribution(values: list[int]) -> dict[str, int | None]:
    """Nearest rank: sorted[ceil(percentile * count / 100) - 1]."""
    ordered = sorted(values)
    if not ordered:
        return dict.fromkeys(("min", "p50", "p90", "p95", "p99", "max"))
    return {
        "min": ordered[0],
        **{f"p{p}": ordered[(p * len(ordered) + 99) // 100 - 1]
           for p in (50, 90, 95, 99)},
        "max": ordered[-1],
    }


def aggregate(rows: list[dict[str, Any]]) -> dict[str, Any]:
    result = {
        "messages": len(rows),
        **{key: sum(row[key] for row in rows) for key in
           ("operations", "slotChecks", "gasEstimate", "gasLimit", "gasUsed", "signedBytes")},
        "gasUsedDistribution": distribution([row["gasUsed"] for row in rows]),
        "gasLimitDistribution": distribution([row["gasLimit"] for row in rows]),
    }
    result["utilization"] = result["gasUsed"] / result["gasLimit"] if result["gasLimit"] else None
    result["firstIndex"] = rows[0]["index"] if rows else None
    result["lastIndex"] = rows[-1]["index"] if rows else None
    return result


def deciles(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    # Integer boundaries partition all messages exactly once, including tiny runs.
    return [{"decile": i + 1, **aggregate(rows[len(rows) * i // 10:len(rows) * (i + 1) // 10])}
            for i in range(10)]


def chain_time(gas_used: int, gas_limit: int, messages: int, complete: bool) -> dict[str, Any]:
    ceiling = 50_000_000_000
    scenarios = []
    for percent in (1, 5, 10, 100):
        capacity = ceiling * percent // 100
        scenario: dict[str, Any] = {"capacityPercent": percent, "gasPerEpoch": capacity}
        for key, total in (("gasUsed", gas_used), ("summedGasLimits", gas_limit)):
            epochs = (total + capacity - 1) // capacity
            scenario[key] = {
                "totalGas": total,
                "fractionalEpochs": total / capacity,
                "gasOnlySeconds": total * 30 / capacity,
                "roundedEpochs": epochs,
                "roundedEpochSeconds": epochs * 30,
            }
        scenarios.append(scenario)
    return {
        "label": "Theoretical gas-only capacity scenarios, not predicted mainnet wall time",
        "scope": "full run" if complete else "confirmed contiguous prefix only; not a full-run estimate",
        "assumptions": {
            "epochSeconds": 30, "blockGasLimit": 10_000_000_000,
            "expectedElectionWinsPerEpoch": 5,
            "nominalOptimisticBlocksPerTipset": 5, "nominalOptimisticGasCeilingPerEpoch": ceiling,
            "targetUtilization": 0.5, "blockGasTarget": 5_000_000_000,
            "nominalOptimisticTargetGasPerEpoch": 25_000_000_000,
            "ceilingIsMeasuredThroughput": False, "ceilingIsGuaranteed": False,
        },
        "scenarios": scenarios,
        "sequentialOneConfirmationPerMessage": {
            "messages": messages, "secondsPerMessage": 30, "seconds": messages * 30,
            "label": "Separate idealized one-receipt-per-epoch estimate; inclusion delays, null rounds, congestion, and additional confirmations can extend it; not finality",
        },
        "limitations": [
            "Summed gas limits, not gas used, govern message admission and block packing.",
            "The five-block/50B-gas epoch ceiling is a nominal optimistic model, not measured throughput or an expected number of distinct blocks.",
            "Expected Consensus e=5 counts expected election wins; a miner winning multiple times still produces only one block.",
            "Null rounds, block failures, and duplicate messages across blocks reduce actual useful epoch capacity.",
            "Block packing, message overlap/deduplication across blocks, inclusion delay, congestion, and competing fee demand are not measured.",
            "A message cannot be split across blocks; aggregate gas division is not a packing simulation.",
            "Local execution elapsed time is not a mainnet wall-time measurement.",
        ],
        "sources": {
            "gasFees": "https://spec.filecoin.io/systems/filecoin_vm/gas_fee/",
            "expectedConsensus": "https://spec.filecoin.io/algorithms/expected_consensus/",
            "gasParameters": "https://raw.githubusercontent.com/filecoin-project/lotus/master/build/buildconstants/params_shared_vals.go",
            "epochDuration": "https://raw.githubusercontent.com/filecoin-project/filecoin-docs/main/core-concepts/filecoin-virtual-machine/blocks-and-tipsets.md",
        },
    }


def collect(run: Path, summary: dict[str, Any], rows: list[dict[str, Any]],
            issues: list[str]) -> None:
    plan = Plan(run / "plan")
    summary["plannedMessages"] = plan.batch_count
    summary["plan"] = {"sha256": plan.plan_hash, "batchesSha256": plan.batches_hash,
                       "manifest": plan.manifest}
    setup = load_object(run / "setup.json")
    summary["setupAndBuildPins"] = setup
    if setup.get("sourcePlanSha256") != plan.manifest.get("sourcePlanSha256"):
        raise ValueError("setup original source plan digest differs from rebatch manifest")
    identity = load_object(run / "journal" / "manifest.json")
    summary["journalIdentity"] = identity
    expected = {"version": 1, "planSha256": plan.plan_hash,
                "batchesSha256": plan.batches_hash, "fwss": plan.fwss,
                "facetCodeHash": setup["facetCodeHash"], "chainId": 31337}
    if any(identity.get(key) != value for key, value in expected.items()):
        raise ValueError("journal identity differs from plan, setup facet, or local chain 31337")
    if address(setup["fwss"]) != plan.fwss:
        raise ValueError("setup FWSS differs from plan")
    signer = address(identity["signer"])
    journal = Journal(run / "journal", plan,
                      argparse.Namespace(expected_chain_id=identity["chainId"]), signer)
    journal.identity = identity
    journal_indexes: set[int] = set()
    for path in sorted(journal.directory.glob("batch-*.json")):
        suffix = path.stem.removeprefix("batch-")
        if not suffix.isdecimal():
            issues.append(f"unexpected journal filename: {path.name}")
            continue
        index = int(suffix)
        if path != journal.path(index) or not 0 <= index < plan.batch_count:
            issues.append(f"unexpected journal batch file: {path.name}")
            continue
        journal_indexes.add(index)
    summary["journalBatchFiles"] = len(journal_indexes)
    if journal_indexes and max(journal_indexes) + 1 != len(journal_indexes):
        issues.append("journal indexes are not a contiguous prefix from zero")

    timing: dict[int, dict[str, Any]] = {}
    timing_path = run / "messages.jsonl"
    if timing_path.exists():
        with timing_path.open() as stream:
            for line_number, line in enumerate(stream, 1):
                try:
                    entry = json.loads(line)
                    index = nonnegative(entry["index"])
                    if index >= plan.batch_count or index in timing:
                        raise ValueError("duplicate or out-of-range timing index")
                    seconds = entry["elapsedSeconds"]
                    if isinstance(seconds, bool) or not isinstance(seconds, (int, float)) or not math.isfinite(seconds) or seconds < 0:
                        raise ValueError("invalid elapsedSeconds")
                    timing[index] = entry
                except (KeyError, TypeError, ValueError) as error:
                    issues.append(f"messages.jsonl line {line_number}: {error}")
    summary["timingSource"] = "messages.jsonl" if timing_path.exists() else None

    hashes: set[str] = set()
    nonces: set[int] = set()
    receipt_blocks: dict[int, str] = {}
    prefix_open = True
    planned_slots = planned_queues = 0
    planned_phases: Counter[str] = Counter()
    local_fee = fee_messages = 0
    confirmed_count = 0
    for batch in plan.batches():
        index = batch["index"]
        planned_slots += len(batch["slots"])
        planned_queues += len(batch["queues"])
        planned_phases[batch["phase"]] += 1
        try:
            saved = journal.load(batch)
            if saved is None:
                raise ValueError("missing journal batch")
            tx_hash = "0x" + hex_bytes(saved["transactionHash"], 32).hex()
            nonce = nonnegative(saved["nonce"])
            if tx_hash in hashes or nonce in nonces:
                raise ValueError("duplicate transaction hash or nonce")
            hashes.add(tx_hash)
            nonces.add(nonce)
            if saved["state"] != "confirmed":
                raise ValueError(f"journal state is {saved['state']}, not confirmed")
            if saved["phase"] != batch["phase"]:
                raise ValueError("journal phase differs from plan")
            receipt = saved["receipt"]
            if nonnegative(receipt["status"]) != 1:
                raise ValueError("receipt status is not success (1)")
            gas_used = nonnegative(saved["gasUsed"])
            if nonnegative(receipt["gasUsed"]) != gas_used:
                raise ValueError("receipt gasUsed differs from saved gasUsed")
            if receipt["transactionHash"].lower() != tx_hash:
                raise ValueError("receipt transaction hash differs from journal")
            if address(receipt["from"]) != signer or address(receipt["to"]) != plan.fwss:
                raise ValueError("receipt sender/destination differs from journal")
            hex_bytes(receipt["blockHash"], 32)
            block_number = nonnegative(receipt["blockNumber"])
            block_hash = receipt["blockHash"].lower()
            if block_number in receipt_blocks and receipt_blocks[block_number] != block_hash:
                raise ValueError("receipts disagree on the canonical hash of one block")
            receipt_blocks[block_number] = block_hash
            gas_limit = nonnegative(saved["gasLimit"])
            estimate = nonnegative(saved["gasEstimate"])
            if not gas_used or not estimate or gas_used > gas_limit:
                raise ValueError("invalid gas used/estimate/limit")
            signed_bytes = len(hex_bytes(saved["rawTransaction"]))
            if nonnegative(saved["signedBytes"]) != signed_bytes:
                raise ValueError("signedBytes differs from raw transaction length")
            if address(saved["facet"]) != address(setup["facet"]):
                raise ValueError("journal facet differs from setup")
            entry = {
                "index": index, "phase": batch["phase"], "operations": batch["operations"],
                "slotChecks": len(batch["slots"]), "gasEstimate": estimate,
                "gasLimit": gas_limit, "gasUsed": gas_used, "utilization": gas_used / gas_limit,
                "signedBytes": signed_bytes, "transactionHash": tx_hash,
                "receiptBlock": block_number, "elapsedSeconds": None,
            }
            if index in timing:
                measured = timing[index]
                for key in ("phase", "operations", "gasUsed", "gasLimit", "gasEstimate", "signedBytes"):
                    if measured.get(key) != entry[key]:
                        raise ValueError(f"messages.jsonl {key} differs from journal/plan")
                if measured.get("slots") != entry["slotChecks"]:
                    raise ValueError("messages.jsonl slots differs from plan")
                entry["elapsedSeconds"] = measured["elapsedSeconds"]
            fee = None
            if receipt.get("effectiveGasPrice") is not None:
                price = nonnegative(receipt["effectiveGasPrice"])
                fee = gas_used * price
                if "effectiveGasPrice" in saved and nonnegative(saved["effectiveGasPrice"]) != price:
                    raise ValueError("saved effectiveGasPrice differs from receipt")
                if "gasCost" in saved and nonnegative(saved["gasCost"]) != fee:
                    raise ValueError("saved gasCost differs from receipt-derived local fee")
            confirmed_count += 1
            if prefix_open:
                entry["cumulativeGasUsed"] = (rows[-1]["cumulativeGasUsed"] if rows else 0) + gas_used
                entry["cumulativeGasLimit"] = (rows[-1]["cumulativeGasLimit"] if rows else 0) + gas_limit
                for key in ("cumulativeGasUsed", "cumulativeGasLimit"):
                    if index in timing and timing[index].get(key) != entry[key]:
                        raise ValueError(f"messages.jsonl {key} differs from receipt-derived sum")
                rows.append(entry)
                if fee is not None:
                    local_fee += fee
                    fee_messages += 1
        except (KeyError, TypeError, ValueError, RuntimeError, OSError) as error:
            prefix_open = False
            issues.append(f"batch {index}: {error}")
    plan.assert_unchanged()
    summary["plannedPhaseMessages"] = dict(planned_phases)
    summary["confirmedMessagesObserved"] = confirmed_count
    summary["localFees"] = {
        "receiptGasUsedTimesEffectiveGasPriceAttoFIL": local_fee,
        "messagesWithEffectiveGasPrice": fee_messages,
        "completeForPrefix": fee_messages == len(rows),
        "label": "Local receipt-derived fees only; not a mainnet cost estimate or full Filecoin fee accounting",
    }
    execution_path = run / "execution.json"
    if not execution_path.exists():
        issues.append("missing execution.json canonical final verification")
        return
    execution = load_object(execution_path)
    summary["execution"] = execution
    summary["canonicalFinalVerification"] = execution.get("verification")
    expected_execution = {
        "status": "verified", "planSha256": plan.plan_hash,
        "chainId": identity["chainId"], "fwss": plan.fwss,
        "facetCodeHash": identity["facetCodeHash"], "batchCount": plan.batch_count,
        "signer": signer, "receiptsVerified": plan.batch_count,
        "gasUsed": sum(row["gasUsed"] for row in rows),
    }
    if any(execution.get(key) != value for key, value in expected_execution.items()):
        issues.append("execution.json identity, receipt count, status, or gas total differs from complete journal")
    verification = execution.get("verification")
    if not isinstance(verification, dict):
        issues.append("execution.json has no canonical final verification object")
    else:
        try:
            if nonnegative(verification["slotChecks"]) != planned_slots or nonnegative(verification["queueChecks"]) != planned_queues:
                raise ValueError("final slot/queue checks differ from plan")
            final_block = nonnegative(verification["blockNumber"])
            hex_bytes(verification["blockHash"], 32)
            if rows and final_block < max(row["receiptBlock"] for row in rows):
                raise ValueError("final verification predates a cleanup receipt")
            if final_block in receipt_blocks and verification["blockHash"].lower() != receipt_blocks[final_block]:
                raise ValueError("final canonical verification hash differs from receipt block")
            if nonnegative(execution["confirmations"]) < 1:
                raise ValueError("final verification has no confirmations")
        except (KeyError, TypeError, ValueError) as error:
            issues.append(f"execution.json: {error}")
    summary["verificationEvidence"] = "Persisted executor canonical checks only; this offline report does not re-query the chain"


def report(run: Path, output: Path) -> dict[str, Any]:
    summary: dict[str, Any] = {
        "schemaVersion": 1, "status": "incomplete", "runDirectory": str(run),
        "plannedMessages": None, "canonicalFinalVerification": None,
        "percentileMethod": "Deterministic nearest rank: sorted[ceil(p * n / 100) - 1]",
    }
    rows: list[dict[str, Any]] = []
    issues: list[str] = []
    try:
        collect(run, summary, rows, issues)
    except (KeyError, TypeError, ValueError, RuntimeError, OSError) as error:
        issues.append(str(error))
    complete = not issues and summary["plannedMessages"] == len(rows)
    if not complete and not issues:
        issues.append("journal does not cover exactly plan batchCount")
    summary["status"] = "complete" if complete else "incomplete"
    summary["issues"] = issues
    summary["completedPrefix"] = {"count": len(rows), "firstIndex": 0 if rows else None,
                                  "lastIndex": len(rows) - 1 if rows else None,
                                  "nextIndex": len(rows) if not complete else None}
    summary["totalsScope"] = "full run" if complete else "confirmed contiguous prefix only"
    summary["totals"] = aggregate(rows)
    summary["summedGasLimits"] = summary["totals"]["gasLimit"]
    summary["countPerPhase"] = {phase: sum(row["phase"] == phase for row in rows) for phase in PHASES}
    summary["perPhase"] = {}
    for phase in PHASES:
        phase_rows = [row for row in rows if row["phase"] == phase]
        summary["perPhase"][phase] = {**aggregate(phase_rows), "deciles": deciles(phase_rows)}
    chunks = deciles(rows)
    summary["deciles"] = chunks
    summary["firstMiddleLastDecile"] = {
        "first": chunks[0], "middle": chunks[4], "last": chunks[9],
        "definition": "First=1st, middle=5th (40-50%), last=10th decile; floor(n*d/10) boundaries",
    }
    summary["localTiming"] = {
        "source": summary.get("timingSource"),
        "messagesWithElapsedSeconds": sum(row["elapsedSeconds"] is not None for row in rows),
        "sumMessageElapsedSeconds": sum(row["elapsedSeconds"] or 0 for row in rows),
        "executionElapsedSeconds": summary.get("execution", {}).get("elapsedSeconds"),
        "wallSecondsSinceSetup": summary.get("execution", {}).get("wallSecondsSinceSetup"),
        "label": "Local rehearsal runtime only; not a chain-duration estimate",
    }
    summary["chainTime"] = chain_time(summary["totals"]["gasUsed"], summary["totals"]["gasLimit"], len(rows), complete)
    output.mkdir(parents=True, exist_ok=True)
    with (output / "messages.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=COLUMNS, lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
    (output / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True, allow_nan=False) + "\n")
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--output", type=Path, help="default: RUN_DIR/report")
    parser.add_argument("--require-complete", action="store_true",
                        help="exit nonzero unless every planned message and final verification are complete")
    args = parser.parse_args()
    output = args.output or args.run_dir / "report"
    try:
        summary = report(args.run_dir, output)
        print(json.dumps({"status": summary["status"], "summary": str(output / "summary.json"),
                          "messages": str(output / "messages.csv"),
                          "completedMessages": summary["completedPrefix"]["count"],
                          "plannedMessages": summary["plannedMessages"],
                          "gasUsed": summary["totals"]["gasUsed"],
                          "summedGasLimits": summary["summedGasLimits"]}, sort_keys=True))
        return int(args.require_complete and summary["status"] != "complete")
    except (OSError, TypeError, ValueError) as error:
        print(f"cleanup_stats: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
