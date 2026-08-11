#!/usr/bin/env python3
"""Compare PDPVerifier lifecycle operations before and after a local upgrade."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

from run import (
    CLEANUP_DEPOSIT,
    DEFAULT_KEY,
    PROXY,
    REQUIRED_BALANCE_ATTO,
    ZERO_ADDRESS,
    Benchmark,
    command,
)

ADD_BATCH_SIZES = (1, 8, 32)
CHALLENGES_PER_PROOF = 5
REMOVAL_BATCH_SIZES = (1, 32, 96)
CLEANUP_BATCH_SIZES = (1, 16, 64, 96)
MAX_CLEANUP_BATCH_SIZE = 96

# A height-5 binary tree containing 32 zero leaves. Every offset has the same
# leaf and sibling path, so one fixture proves any challenge drawn in any piece.
ZERO_LEAF = "0x" + "00" * 32
ZERO_TREE_ROOT = "1f7ac9595510e09ea41c460b176430bb322cd6fb412ec57cb17d989a4310372f"
ZERO_TREE_PROOF = (
    "0x" + "00" * 32,
    "0xf5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb0b",
    "0x3731bb99ac689f66eef5973e4a94da188f4ddcae580724fc6f3fd60dfd488333",
    "0x642a607ef886b004bf2c1978463ae1d4693ac0f410eb2d1b7a47fe205e5e750f",
    "0x57a2381a28652bf47f6bef7aca679be4aede5871ab5cf3eb2c08114488cb8526",
)
VALID_PIECE_CID = "0x01559120220005" + ZERO_TREE_ROOT


class OperationsBenchmark(Benchmark):
    def __init__(self, args: argparse.Namespace) -> None:
        # Benchmark supplies the RPC, deployment, tracing, and atomic output machinery.
        args.fill_batch_size = args.setup_batch_size
        args.target_piece_count = args.piece_count
        super().__init__(args)
        self.piece_count = args.piece_count
        self.setup_batch_size = args.setup_batch_size
        self.result = {
            "schemaVersion": 1,
            "createdAt": self.result["createdAt"],
            "configuration": {
                "rpcUrl": self.rpc_url,
                "preparedArchive": args.prepared_archive,
                "proxy": self.proxy,
                "sender": self.sender,
                "pieceCount": self.piece_count,
                "setupBatchSize": self.setup_batch_size,
                "addBatchSizes": list(ADD_BATCH_SIZES),
                "removalBatchSizes": list(REMOVAL_BATCH_SIZES),
                "challengesPerProof": CHALLENGES_PER_PROOF,
                "cleanupBatchSizes": list(CLEANUP_BATCH_SIZES),
                "pieceEncoding": {
                    "description": "CommPv2 padding=0, height=5, all-zero Merkle tree",
                    "cid": VALID_PIECE_CID,
                    "leaf": ZERO_LEAF,
                    "proof": list(ZERO_TREE_PROOF),
                },
                "nodeRevision": self.result["configuration"]["nodeRevision"],
                "nodeDescribe": self.result["configuration"]["nodeDescribe"],
                "pdpRevision": self.result["configuration"]["pdpRevision"],
                "pdpDescribe": self.result["configuration"]["pdpDescribe"],
            },
            "baseline": {},
            "upgrade": {},
            "variants": [],
            "comparison": [],
        }

    @staticmethod
    def pieces(count: int) -> str:
        return "[" + ",".join(f"({VALID_PIECE_CID})" for _ in range(count)) + "]"

    @staticmethod
    def proofs(count: int) -> str:
        siblings = "[" + ",".join(ZERO_TREE_PROOF) + "]"
        return "[" + ",".join(f"({ZERO_LEAF},{siblings})" for _ in range(count)) + "]"

    def call(self, signature: str, *args: str) -> str:
        return self.cast("call", "--rpc-url", self.rpc_url, self.proxy, signature, *args)

    def measured_send(
        self,
        variant: dict[str, Any],
        operation: str,
        case: str,
        signature: str,
        arguments: list[str],
        *,
        value: int | None = None,
        parameters: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        print(f"pdp-operations: {variant['name']} {operation} [{case}]", file=sys.stderr, flush=True)
        receipt = self.send(signature, arguments, value=value)
        trace = self.trace(receipt["transactionHash"])
        summary = self.summarize_trace(trace)
        receipt_gas = self.quantity(receipt["gasUsed"])
        if self.quantity(trace["exitCode"]) != 0:
            raise RuntimeError(f"gas trace reports failure for {receipt['transactionHash']}")
        if summary["gasUsed"] != receipt_gas:
            raise RuntimeError(
                f"trace gas {summary['gasUsed']} differs from receipt gas {receipt_gas}"
            )
        measurement = {
            "operation": operation,
            "case": case,
            "parameters": parameters or {},
            "transaction": receipt,
            "traceSummary": summary,
            "trace": trace,
        }
        variant["measurements"].append(measurement)
        self.write()
        return measurement

    def mine_to(self, target_epoch: int) -> None:
        while self.block_number() < target_epoch:
            self.rpc("evm_mine")

    def run_variant(self, name: str, implementation: str, storage: str) -> dict[str, Any]:
        set_id = self.call_uint("getNextDataSetId()(uint64)")
        variant: dict[str, Any] = {
            "name": name,
            "implementation": implementation,
            "storage": storage,
            "dataSetId": set_id,
            "measurements": [],
            "setupTransactions": [],
            "invariants": {},
        }
        self.result["variants"].append(variant)
        self.write()

        self.measured_send(
            variant,
            "createDataSet",
            "empty",
            "createDataSet(address,bytes)",
            [ZERO_ADDRESS, "0x"],
            value=CLEANUP_DEPOSIT,
        )
        if self.call_uint("getNextDataSetId()(uint64)") != set_id + 1:
            raise RuntimeError("createDataSet did not advance nextDataSetId by one")

        added = 0
        for batch_size in ADD_BATCH_SIZES:
            self.measured_send(
                variant,
                "addPieces",
                str(batch_size),
                "addPieces(uint256,address,(bytes)[],bytes)",
                [str(set_id), ZERO_ADDRESS, self.pieces(batch_size), "0x"],
                parameters={"batchSize": batch_size, "startingPieceCount": added},
            )
            added += batch_size

        while added < self.piece_count:
            batch_size = min(self.setup_batch_size, self.piece_count - added)
            receipt = self.send(
                "addPieces(uint256,address,(bytes)[],bytes)",
                [str(set_id), ZERO_ADDRESS, self.pieces(batch_size), "0x"],
            )
            variant["setupTransactions"].append(
                {
                    "operation": "addPieces",
                    "batchSize": batch_size,
                    "startingPieceCount": added,
                    "transaction": receipt,
                }
            )
            added += batch_size
            self.write()

        observed_count = self.call_uint("getNextPieceId(uint256)(uint256)", str(set_id))
        if observed_count != self.piece_count:
            raise RuntimeError(f"dataset has {observed_count} pieces, expected {self.piece_count}")

        self.measured_send(
            variant,
            "getPieceCid",
            "tail",
            "getPieceCid(uint256,uint256)",
            [str(set_id), str(self.piece_count - 1)],
            parameters={"pieceId": self.piece_count - 1},
        )
        self.measured_send(
            variant,
            "getActivePiecesByCursor",
            "page-32-full",
            "getActivePiecesByCursor(uint256,uint256,uint256)",
            [str(set_id), "0", "32"],
            parameters={"startPieceId": 0, "limit": 32},
        )
        self.measured_send(
            variant,
            "findPieceIdsByCid",
            "limit-32",
            "findPieceIdsByCid(uint256,(bytes),uint256,uint256)",
            [str(set_id), f"({VALID_PIECE_CID})", "0", "32"],
            parameters={"startPieceId": 0, "limit": 32},
        )

        removed_ids: list[int] = []
        next_piece_to_remove = 0
        for removal_count in REMOVAL_BATCH_SIZES:
            piece_ids = list(range(next_piece_to_remove, next_piece_to_remove + removal_count))
            case = str(removal_count)
            self.measured_send(
                variant,
                "schedulePieceDeletions",
                case,
                "schedulePieceDeletions(uint256,uint256[],bytes)",
                [str(set_id), json.dumps(piece_ids, separators=(",", ":")), "0x"],
                parameters={"pieceIds": piece_ids},
            )

            challenge_epoch = (
                self.block_number() + self.call_uint("getChallengeFinality()(uint256)") + 1
            )
            self.measured_send(
                variant,
                "nextProvingPeriod",
                f"remove-{case}",
                "nextProvingPeriod(uint256,uint256,bytes)",
                [str(set_id), str(challenge_epoch), "0x"],
                parameters={"challengeEpoch": challenge_epoch, "removedPieceCount": removal_count},
            )
            if (
                self.call_uint(
                    "getPieceLeafCount(uint256,uint256)(uint256)",
                    str(set_id),
                    str(next_piece_to_remove),
                )
                != 0
            ):
                raise RuntimeError(
                    f"nextProvingPeriod did not remove scheduled piece {next_piece_to_remove}"
                )

            self.mine_to(challenge_epoch)
            proof_fee = self.call_uint("calculateProofFee(uint256)(uint256)", str(set_id))
            self.measured_send(
                variant,
                "provePossession",
                f"after-remove-{case}",
                "provePossession(uint256,(bytes32,bytes32[])[])",
                [str(set_id), self.proofs(CHALLENGES_PER_PROOF)],
                value=proof_fee,
                parameters={
                    "proofCount": CHALLENGES_PER_PROOF,
                    "proofFee": proof_fee,
                    "precedingRemovalCount": removal_count,
                },
            )

            removed_ids.extend(piece_ids)
            next_piece_to_remove += removal_count

        self.measured_send(
            variant,
            "getActivePiecesByCursor",
            "page-32-with-holes",
            "getActivePiecesByCursor(uint256,uint256,uint256)",
            [str(set_id), "0", "32"],
            parameters={"startPieceId": 0, "limit": 32, "removedPieceCount": len(removed_ids)},
        )

        self.measured_send(
            variant,
            "deleteDataSet",
            f"{self.piece_count}-slots",
            "deleteDataSet(uint256,bytes)",
            [str(set_id), "0x"],
            parameters={"pieceSlots": self.piece_count},
        )

        remaining = self.piece_count
        for batch_size in CLEANUP_BATCH_SIZES:
            self.measured_send(
                variant,
                "cleanupPieces",
                str(batch_size),
                "cleanupPieces(uint256,uint256)",
                [str(set_id), str(batch_size)],
                parameters={"maxPieces": batch_size, "startingPieceSlots": remaining},
            )
            remaining -= batch_size

        while remaining > MAX_CLEANUP_BATCH_SIZE:
            batch_size = min(MAX_CLEANUP_BATCH_SIZE, remaining - MAX_CLEANUP_BATCH_SIZE)
            receipt = self.send(
                "cleanupPieces(uint256,uint256)",
                [str(set_id), str(batch_size)],
            )
            variant["setupTransactions"].append(
                {
                    "operation": "cleanupPieces",
                    "batchSize": batch_size,
                    "startingPieceSlots": remaining,
                    "transaction": receipt,
                }
            )
            remaining -= batch_size
            self.write()

        self.measured_send(
            variant,
            "cleanupPieces",
            f"{remaining}-final",
            "cleanupPieces(uint256,uint256)",
            [str(set_id), str(remaining)],
            parameters={"maxPieces": remaining, "startingPieceSlots": remaining, "finalizes": True},
        )

        if self.call("dataSetLive(uint256)(bool)", str(set_id)).strip() != "false":
            raise RuntimeError("dataset remained live after delete and cleanup")
        try:
            self.call_uint("getNextPieceId(uint256)(uint256)", str(set_id))
        except RuntimeError:
            pass
        else:
            raise RuntimeError("final cleanup left the dataset singleton state present")

        variant["invariants"] = {
            "createdPieceCount": self.piece_count,
            "removedPieceCount": len(removed_ids),
            "provingPeriodCount": len(REMOVAL_BATCH_SIZES),
            "challengesPerProof": CHALLENGES_PER_PROOF,
            "cleanupFinalized": True,
        }
        self.write()
        return variant

    def compare(self) -> None:
        baseline, upgraded = self.result["variants"]
        baseline_by_key = {
            (item["operation"], item["case"]): item for item in baseline["measurements"]
        }
        upgraded_by_key = {
            (item["operation"], item["case"]): item for item in upgraded["measurements"]
        }
        if baseline_by_key.keys() != upgraded_by_key.keys():
            raise RuntimeError("baseline and upgraded variants measured different operation cases")

        comparison = []
        for key, old in baseline_by_key.items():
            new = upgraded_by_key[key]
            old_gas = old["traceSummary"]["gasUsed"]
            new_gas = new["traceSummary"]["gasUsed"]
            difference = new_gas - old_gas
            comparison.append(
                {
                    "operation": key[0],
                    "case": key[1],
                    "baselineGasUsed": old_gas,
                    "upgradedGasUsed": new_gas,
                    "gasDifference": difference,
                    "percentChange": difference * 100.0 / old_gas,
                }
            )
        self.result["comparison"] = comparison
        self.write()

    def run(self) -> None:
        sender_balance = int(self.cast("balance", "--rpc-url", self.rpc_url, self.sender))
        if sender_balance < REQUIRED_BALANCE_ATTO:
            raise RuntimeError(
                f"benchmark sender has {sender_balance} attoFIL; restart fvm-anvil with "
                f"--balance {REQUIRED_BALANCE_ATTO // 10**18}"
            )

        baseline_implementation = self.implementation()
        self.result["baseline"] = {
            "blockNumber": self.block_number(),
            "stateRoots": self.rpc("filecoin_getStateRoots"),
            "implementation": baseline_implementation,
            "senderBalanceAtto": sender_balance,
            "nextDataSetId": self.call_uint("getNextDataSetId()(uint64)"),
            "challengeFinality": self.call_uint("getChallengeFinality()(uint256)"),
        }
        snapshot = self.rpc("evm_snapshot")
        self.write()

        self.run_variant("deployed-v3.4.0", baseline_implementation, "legacy")

        print("pdp-operations: restoring imported baseline", file=sys.stderr, flush=True)
        if self.rpc("evm_revert", snapshot) is not True:
            raise RuntimeError(f"failed to revert benchmark snapshot {snapshot}")
        upgrade = self.deploy_and_upgrade()
        self.result["upgrade"] = upgrade
        self.write()

        self.run_variant("local-storage-optimized", upgrade["implementation"], "compact")
        self.compare()
        print(json.dumps({"output": str(self.output), "comparison": self.result["comparison"]}))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18546")
    parser.add_argument("--proxy", default=PROXY)
    parser.add_argument("--private-key", default=DEFAULT_KEY)
    parser.add_argument(
        "--pdp-repo", type=Path, default=Path("/home/kubuxu/work/github.com/FilOzone/pdp")
    )
    parser.add_argument(
        "--prepared-archive",
        default="snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("benchmarks/pdp-verifier/results/pdp-operation-gas-traces.json"),
    )
    parser.add_argument("--piece-count", type=int, default=1024)
    parser.add_argument("--setup-batch-size", type=int, default=32)
    args = parser.parse_args()
    if args.piece_count <= sum(REMOVAL_BATCH_SIZES):
        parser.error("removal scenarios must leave at least one live piece")
    if args.setup_batch_size <= 0:
        parser.error("--setup-batch-size must be positive")
    if sum(CLEANUP_BATCH_SIZES) >= args.piece_count:
        parser.error("cleanup measurement batches must leave pieces for bounded finalization")
    return args


if __name__ == "__main__":
    OperationsBenchmark(parse_args()).run()
