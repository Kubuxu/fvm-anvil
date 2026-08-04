#!/usr/bin/env python3
"""Benchmark PDPVerifier addPieces against imported and locally upgraded code."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

PROXY = "0xBADd0B92C1c71d02E7d520f64c0876538fa2557F"
ZERO_ADDRESS = "0x0000000000000000000000000000000000000000"
IMPLEMENTATION_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
INITIALIZABLE_SLOT = "0xf0c57e16840df040f15088dc2f81fe391c3923bec73e23a9662efc9c229c6a00"
DEFAULT_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
TARGET_BATCH_SIZES = (1, 2, 4, 16, 32)
GAS_LIMIT = 10_000_000_000
CLEANUP_DEPOSIT = 100_000_000_000_000_000
REQUIRED_BALANCE_ATTO = 10_000 * 10**18
PIECE_PREFIX = bytes.fromhex("01559120220005")


def command(args: list[str], *, cwd: Path | None = None) -> str:
    completed = subprocess.run(
        args,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        rendered = " ".join(args[:4])
        raise RuntimeError(
            f"command failed ({completed.returncode}): {rendered}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed.stdout.strip()


class Benchmark:
    def __init__(self, args: argparse.Namespace) -> None:
        self.rpc_url = args.rpc_url
        self.private_key = args.private_key
        self.proxy = args.proxy
        self.pdp_repo = args.pdp_repo.resolve()
        self.output = args.output.resolve()
        self.fill_batch_size = args.fill_batch_size
        self.target_piece_count = args.target_piece_count
        self.piece_index = 1
        self.sender = self.cast("wallet", "address", "--private-key", self.private_key)
        self.max_fee_per_gas = int(self.rpc("eth_gasPrice"), 16)
        self.priority_fee_per_gas = 1
        node_repo = Path(__file__).resolve().parents[2]
        self.result: dict[str, Any] = {
            "schemaVersion": 1,
            "createdAt": datetime.now(timezone.utc).isoformat(),
            "configuration": {
                "rpcUrl": self.rpc_url,
                "preparedArchive": args.prepared_archive,
                "proxy": self.proxy,
                "sender": self.sender,
                "targetBatchSizes": list(TARGET_BATCH_SIZES),
                "targetPieceCount": self.target_piece_count,
                "fillBatchSize": self.fill_batch_size,
                "requiredSenderBalanceAtto": REQUIRED_BALANCE_ATTO,
                "maxFeePerGas": self.max_fee_per_gas,
                "maxPriorityFeePerGas": self.priority_fee_per_gas,
                "pieceEncoding": {
                    "description": "CommPv2 padding=0, height=5, sequential bytes32 roots",
                    "hexPrefix": "0x" + PIECE_PREFIX.hex(),
                },
                "nodeRevision": command(["git", "rev-parse", "HEAD"], cwd=node_repo),
                "nodeDescribe": command(
                    ["git", "describe", "--tags", "--always", "--dirty"], cwd=node_repo
                ),
                "pdpRevision": command(["git", "rev-parse", "HEAD"], cwd=self.pdp_repo),
                "pdpDescribe": command(["git", "describe", "--tags", "--always", "--dirty"], cwd=self.pdp_repo),
            },
            "baseline": {},
            "variants": [],
            "comparison": [],
        }

    def cast(self, *args: str) -> str:
        return command(["cast", *args])

    def rpc(self, method: str, *params: str) -> Any:
        output = self.cast("rpc", "--rpc-url", self.rpc_url, method, *params)
        return json.loads(output)

    def call_uint(self, signature: str, *args: str) -> int:
        output = self.cast("call", "--rpc-url", self.rpc_url, self.proxy, signature, *args)
        return int(output.split()[0])

    def block_number(self) -> int:
        return int(self.cast("block-number", "--rpc-url", self.rpc_url))

    def implementation(self) -> str:
        word = self.cast("storage", "--rpc-url", self.rpc_url, self.proxy, IMPLEMENTATION_SLOT)
        return "0x" + word[-40:]

    def send(
        self,
        signature: str | None = None,
        arguments: list[str] | None = None,
        *,
        to: str | None = None,
        value: int | None = None,
        creation_code: str | None = None,
    ) -> dict[str, Any]:
        args = [
            "send",
            "--rpc-url",
            self.rpc_url,
            "--private-key",
            self.private_key,
            "--gas-limit",
            str(GAS_LIMIT),
            "--gas-price",
            str(self.max_fee_per_gas),
            "--priority-gas-price",
            str(self.priority_fee_per_gas),
        ]
        if value is not None:
            args.extend(["--value", str(value)])
        if creation_code is not None:
            args.extend(["--create", creation_code])
        else:
            args.extend([to or self.proxy, signature or ""])
            args.extend(arguments or [])
        args.append("--json")
        receipt = json.loads(self.cast(*args))
        if receipt.get("status") != "0x1":
            raise RuntimeError(f"transaction failed: {json.dumps(receipt)}")
        return self.compact_receipt(receipt)

    @staticmethod
    def compact_receipt(receipt: dict[str, Any]) -> dict[str, Any]:
        fields = (
            "transactionHash",
            "contractAddress",
            "blockHash",
            "blockNumber",
            "status",
            "gasUsed",
            "cumulativeGasUsed",
        )
        return {field: receipt[field] for field in fields if receipt.get(field) is not None}

    @staticmethod
    def quantity(value: str | int) -> int:
        return int(value, 16) if isinstance(value, str) and value.startswith("0x") else int(value)

    def write(self) -> None:
        self.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.output.with_suffix(self.output.suffix + ".tmp")
        temporary.write_text(json.dumps(self.result, indent=2, sort_keys=True) + "\n")
        os.replace(temporary, self.output)

    def pieces(self, count: int) -> str:
        encoded = []
        for _ in range(count):
            root = self.piece_index.to_bytes(32, "big")
            encoded.append(f"(0x{(PIECE_PREFIX + root).hex()})")
            self.piece_index += 1
        return "[" + ",".join(encoded) + "]"

    def trace(self, transaction_hash: str) -> dict[str, Any]:
        trace = self.rpc("filecoin_getGasTrace", transaction_hash)
        if not isinstance(trace, dict):
            raise RuntimeError(f"missing gas trace for {transaction_hash}")
        return trace

    def summarize_trace(self, trace: dict[str, Any]) -> dict[str, Any]:
        totals: dict[str, dict[str, int]] = {}
        charged_milligas = 0

        def add_charge(charge: dict[str, Any]) -> None:
            nonlocal charged_milligas
            name = charge["name"]
            target = totals.setdefault(
                name,
                {"count": 0, "computeMilligas": 0, "otherMilligas": 0, "totalMilligas": 0},
            )
            target["count"] += 1
            for field in ("computeMilligas", "otherMilligas", "totalMilligas"):
                target[field] += self.quantity(charge[field])
            charged_milligas += self.quantity(charge["totalMilligas"])

        def walk_call(call: dict[str, Any]) -> None:
            for charge in call.get("gasCharges", []):
                add_charge(charge)
            for subcall in call.get("subcalls", []):
                walk_call(subcall)

        for charge in trace.get("gasCharges", []):
            add_charge(charge)
        for call in trace.get("calls", []):
            walk_call(call)
        return {
            "gasUsed": self.quantity(trace["gasUsed"]),
            "chargedMilligas": charged_milligas,
            "chargesByName": dict(sorted(totals.items())),
        }

    def add_pieces(self, set_id: int, count: int) -> dict[str, Any]:
        return self.send(
            "addPieces(uint256,address,(bytes)[],bytes)",
            [str(set_id), ZERO_ADDRESS, self.pieces(count), "0x"],
        )

    def create_data_set(self) -> tuple[int, dict[str, Any]]:
        set_id = self.call_uint("getNextDataSetId()(uint64)")
        receipt = self.send(
            "createDataSet(address,bytes)",
            [ZERO_ADDRESS, "0x"],
            value=CLEANUP_DEPOSIT,
        )
        next_set_id = self.call_uint("getNextDataSetId()(uint64)")
        if next_set_id != set_id + 1:
            raise RuntimeError(f"dataset counter advanced from {set_id} to {next_set_id}")
        return set_id, receipt

    def measured_batch(
        self, variant: dict[str, Any], phase: str, set_id: int, piece_count: int, batch_size: int
    ) -> int:
        print(
            f"pdp-benchmark: {variant['name']} {phase} addPieces({batch_size}) at {piece_count}",
            file=sys.stderr,
            flush=True,
        )
        receipt = self.add_pieces(set_id, batch_size)
        transaction_hash = receipt["transactionHash"]
        trace = self.trace(transaction_hash)
        summary = self.summarize_trace(trace)
        receipt_gas_used = self.quantity(receipt["gasUsed"])
        if self.quantity(trace["exitCode"]) != 0:
            raise RuntimeError(f"gas trace reports failure for {transaction_hash}")
        if summary["gasUsed"] != receipt_gas_used:
            raise RuntimeError(
                f"trace gas {summary['gasUsed']} differs from receipt gas {receipt_gas_used}"
            )
        variant["measurements"].append(
            {
                "phase": phase,
                "batchSize": batch_size,
                "startingPieceCount": piece_count,
                "transaction": receipt,
                "traceSummary": summary,
                "trace": trace,
            }
        )
        self.write()
        return piece_count + batch_size

    def run_variant(self, name: str, implementation: str, source: dict[str, Any]) -> dict[str, Any]:
        self.piece_index = 1
        set_id, create_receipt = self.create_data_set()
        variant: dict[str, Any] = {
            "name": name,
            "implementation": implementation,
            "source": source,
            "dataSetId": set_id,
            "createDataSetTransaction": create_receipt,
            "measurements": [],
            "fillTransactions": [],
        }
        self.result["variants"].append(variant)
        self.write()

        piece_count = 0
        for batch_size in TARGET_BATCH_SIZES:
            piece_count = self.measured_batch(variant, "empty", set_id, piece_count, batch_size)

        while piece_count < self.target_piece_count:
            batch_size = min(self.fill_batch_size, self.target_piece_count - piece_count)
            receipt = self.add_pieces(set_id, batch_size)
            piece_count += batch_size
            variant["fillTransactions"].append(
                {
                    "batchSize": batch_size,
                    "endingPieceCount": piece_count,
                    "transaction": receipt,
                }
            )
            if len(variant["fillTransactions"]) % 10 == 0 or piece_count == self.target_piece_count:
                print(
                    f"pdp-benchmark: {name} populated {piece_count}/{self.target_piece_count}",
                    file=sys.stderr,
                    flush=True,
                )
                self.write()

        for batch_size in TARGET_BATCH_SIZES:
            piece_count = self.measured_batch(variant, "populated", set_id, piece_count, batch_size)

        observed_piece_count = self.call_uint(
            "getNextPieceId(uint256)(uint256)", str(set_id)
        )
        if observed_piece_count != piece_count:
            raise RuntimeError(
                f"dataset {set_id} contains {observed_piece_count} pieces, expected {piece_count}"
            )
        variant["endingPieceCount"] = piece_count
        variant["observedPieceCount"] = observed_piece_count
        self.write()
        return variant

    def deploy_and_upgrade(self) -> dict[str, Any]:
        print("pdp-benchmark: building local PDPVerifier", file=sys.stderr, flush=True)
        command(["forge", "build"], cwd=self.pdp_repo)
        artifact_path = self.pdp_repo / "out/PDPVerifier.sol/PDPVerifier.json"
        artifact = json.loads(artifact_path.read_text())
        creation_bytecode = artifact["bytecode"]["object"]

        initialized_word = self.cast(
            "storage", "--rpc-url", self.rpc_url, self.proxy, INITIALIZABLE_SLOT
        )
        initializer_version = int(initialized_word, 16) + 1
        challenge_finality = self.call_uint("getChallengeFinality()(uint256)")
        constructor_args = initializer_version.to_bytes(32, "big") + challenge_finality.to_bytes(32, "big")
        creation_code = creation_bytecode + constructor_args.hex()

        print("pdp-benchmark: deploying local PDPVerifier", file=sys.stderr, flush=True)
        deployment = self.send(creation_code=creation_code)
        implementation = deployment.get("contractAddress")
        if not implementation:
            raise RuntimeError(f"deployment receipt has no contract address: {deployment}")

        after_epoch = self.block_number() + 2
        announcement = self.send(
            "announcePlannedUpgrade((address,uint96))",
            [f"({implementation},{after_epoch})"],
        )
        self.rpc("evm_mine")
        migrate_data = self.cast("calldata", "migrate()")
        upgrade = self.send(
            "upgradeToAndCall(address,bytes)",
            [implementation, migrate_data],
        )
        adopted = self.implementation()
        if adopted.lower() != implementation.lower():
            raise RuntimeError(f"proxy adopted {adopted}, expected {implementation}")
        return {
            "implementation": implementation,
            "initializerVersion": initializer_version,
            "challengeFinality": challenge_finality,
            "deploymentTransaction": deployment,
            "announcementTransaction": announcement,
            "upgradeTransaction": upgrade,
        }

    def compare(self) -> None:
        deployed, upgraded = self.result["variants"]
        upgraded_by_key = {
            (item["phase"], item["batchSize"]): item for item in upgraded["measurements"]
        }
        comparison = []
        for old in deployed["measurements"]:
            key = (old["phase"], old["batchSize"])
            new = upgraded_by_key[key]
            old_gas = old["traceSummary"]["gasUsed"]
            new_gas = new["traceSummary"]["gasUsed"]
            difference = new_gas - old_gas
            comparison.append(
                {
                    "phase": key[0],
                    "batchSize": key[1],
                    "startingPieceCount": old["startingPieceCount"],
                    "deployedGasUsed": old_gas,
                    "upgradedGasUsed": new_gas,
                    "gasDifference": difference,
                    "percentChange": difference * 100.0 / old_gas,
                }
            )
        self.result["comparison"] = comparison
        self.write()

    def run(self) -> None:
        sender_balance = int(
            self.cast("balance", "--rpc-url", self.rpc_url, self.sender)
        )
        if sender_balance < REQUIRED_BALANCE_ATTO:
            raise RuntimeError(
                f"benchmark sender has {sender_balance} attoFIL; restart fvm-anvil with "
                f"--balance {REQUIRED_BALANCE_ATTO // 10**18}"
            )
        roots = self.rpc("filecoin_getStateRoots")
        baseline_implementation = self.implementation()
        self.result["baseline"] = {
            "blockNumber": self.block_number(),
            "stateRoots": roots,
            "implementation": baseline_implementation,
            "senderBalanceAtto": sender_balance,
            "nextDataSetId": self.call_uint("getNextDataSetId()(uint64)"),
            "challengeFinality": self.call_uint("getChallengeFinality()(uint256)"),
        }
        snapshot = self.rpc("evm_snapshot")
        self.write()

        self.run_variant(
            "deployed-v3.4.0",
            baseline_implementation,
            {
                "release": "v3.4.0",
                "deployment": "Filecoin mainnet imported at epoch 6251280",
            },
        )

        print("pdp-benchmark: restoring imported baseline", file=sys.stderr, flush=True)
        if self.rpc("evm_revert", snapshot) is not True:
            raise RuntimeError(f"failed to revert benchmark snapshot {snapshot}")
        upgrade = self.deploy_and_upgrade()
        self.result["upgrade"] = upgrade
        self.write()

        self.run_variant(
            "local-storage-optimized",
            upgrade["implementation"],
            {
                "repository": str(self.pdp_repo),
                "revision": self.result["configuration"]["pdpRevision"],
                "describe": self.result["configuration"]["pdpDescribe"],
            },
        )
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
        default=Path("benchmarks/pdp-verifier/results/pdp-add-pieces-gas-traces.json"),
    )
    parser.add_argument("--fill-batch-size", type=int, default=16)
    parser.add_argument("--target-piece-count", type=int, default=5000)
    args = parser.parse_args()
    if args.fill_batch_size <= 0:
        parser.error("--fill-batch-size must be positive")
    if args.target_piece_count < sum(TARGET_BATCH_SIZES):
        parser.error(f"--target-piece-count must be at least {sum(TARGET_BATCH_SIZES)}")
    return args


if __name__ == "__main__":
    Benchmark(parse_args()).run()
