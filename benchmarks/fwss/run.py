#!/usr/bin/env python3
"""Benchmark FWSS createDataSet/addPieces gas and the session-key (SKR) share.

Drives PDPVerifier -> FilecoinWarmStorageService v1.3.0 on an fvm-anvil node
loaded with prepared mainnet state. Every operation runs in a payer-signed and
a session-key-signed variant from identical pre-state (evm_snapshot/evm_revert)
so the variant delta is exactly the session-key verification overhead that the
PR #536 authorizer (AUTHORIZER_GAS_LIMIT = 150M gas) would replace.

Contract addresses and ABI shapes are pinned to the mainnet v1.3.0 deployment,
FilOzone/filecoin-services commit c2ff7f8ac574f62e5d9f9ace2cf2efbb64b0f3a4.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from eip712 import Eip712Signer, abi_encode, command, sign_digest

FWSS_PROXY = "0x8408502033C418E1bbC97cE9ac48E5528F371A9f"
PDP_PROXY = "0xBADd0B92C1c71d02E7d520f64c0876538fa2557F"
REGISTRY_PROXY = "0xf55dDbf63F1b55c3F1D4FA7e339a68AB7b64A5eB"
FILECOIN_PAY = "0x23b1e018F08BB982348b15a86ee926eEBf7F4DAa"
SESSION_KEY_REGISTRY = "0x74FD50525A958aF5d484601E252271f9625231aB"
USDFC = "0x80B98d3aa09ffff255c3ba4A241111Ff1262F045"
SIGNATURE_LIB = "0xBbd3b82a9396156a997111D42c5356718522ed2D"
ZERO_ADDRESS = "0x0000000000000000000000000000000000000000"

# Standard anvil test keys; balances are set explicitly so derivation is irrelevant.
PAYER_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
SP_KEY = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
SESSION_KEY = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"
EXTRA_SESSION_KEY = "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"

GAS_LIMIT = 10_000_000_000
SYBIL_FEE = 10**17
REGISTRATION_FEE = 5 * 10**18
FIL_BALANCE = 10_000 * 10**18
USDFC_BALANCE = 10**9 * 10**18
PAY_DEPOSIT = 10**8 * 10**18
ALLOWANCE = 10**27
MAX_LOCKUP_PERIOD = 10**8
SESSION_EXPIRY = 4_000_000_000

SWEEP_BATCH_SIZES = (1, 2, 4, 8, 16, 26, 32, 40)
PREFILL_TARGET = 2_000
PREFILL_BATCH = 40
PIECE_PREFIX = bytes.fromhex("01559120220005")


class FwssBenchmark:
    def __init__(self, args: argparse.Namespace) -> None:
        self.rpc_url = args.rpc_url
        self.output = args.output.resolve()
        self.piece_index = 1
        self.client_nonce = int(datetime.now(timezone.utc).timestamp())
        self.payer = self.cast("wallet", "address", "--private-key", PAYER_KEY)
        self.sp = self.cast("wallet", "address", "--private-key", SP_KEY)
        self.session = self.cast("wallet", "address", "--private-key", SESSION_KEY)
        self.extra_session = self.cast("wallet", "address", "--private-key", EXTRA_SESSION_KEY)
        self.chain_id = int(self.rpc("eth_chainId"), 16)
        self.max_fee_per_gas = int(self.rpc("eth_gasPrice"), 16)
        self.signer = Eip712Signer(FWSS_PROXY, self.chain_id)
        self.skr_actor: str | None = None
        node_repo = Path(__file__).resolve().parents[2]
        self.result: dict[str, Any] = {
            "schemaVersion": 1,
            "createdAt": datetime.now(timezone.utc).isoformat(),
            "configuration": {
                "rpcUrl": self.rpc_url,
                "preparedArchive": args.prepared_archive,
                "chainId": self.chain_id,
                "fwssProxy": FWSS_PROXY,
                "pdpProxy": PDP_PROXY,
                "sessionKeyRegistry": SESSION_KEY_REGISTRY,
                "payer": self.payer,
                "serviceProvider": self.sp,
                "sessionSigner": self.session,
                "sweepBatchSizes": list(SWEEP_BATCH_SIZES),
                "prefillTarget": PREFILL_TARGET,
                "authorizerGasLimit": 150_000_000,
                "sourceCommit": "c2ff7f8ac574f62e5d9f9ace2cf2efbb64b0f3a4",
                "nodeRevision": command(["git", "rev-parse", "HEAD"]),
                "nodeDescribe": command(["git", "describe", "--tags", "--always", "--dirty"]),
            },
            "login": {},
            "createDataSet": [],
            "addPieces": [],
            "prefill": {"batches": 0, "pieces": 0},
        }
        # The node repo root is the working directory for git metadata only.
        del node_repo

    # ------------------------------------------------------------------ tooling

    def cast(self, *args: str) -> str:
        return command(["cast", *args])

    def rpc(self, method: str, *params: Any) -> Any:
        rendered = [param if isinstance(param, str) else json.dumps(param) for param in params]
        return json.loads(self.cast("rpc", "--rpc-url", self.rpc_url, method, *rendered))

    def call(self, to: str, signature: str, *args: str) -> str:
        return self.cast("call", "--rpc-url", self.rpc_url, to, signature, *args)

    def call_uint(self, to: str, signature: str, *args: str) -> int:
        return int(self.call(to, signature, *args).split()[0], 0)

    def send(
        self,
        key: str,
        to: str,
        signature: str,
        arguments: list[str] | None = None,
        *,
        value: int | None = None,
    ) -> dict[str, Any]:
        args = [
            "send",
            "--rpc-url",
            self.rpc_url,
            "--private-key",
            key,
            "--gas-limit",
            str(GAS_LIMIT),
            "--gas-price",
            str(self.max_fee_per_gas),
            "--priority-gas-price",
            "1",
        ]
        if value is not None:
            args.extend(["--value", str(value)])
        args.extend([to, signature])
        args.extend(arguments or [])
        args.append("--json")
        receipt = json.loads(self.cast(*args))
        if receipt.get("status") != "0x1":
            raise RuntimeError(f"transaction failed: {json.dumps(receipt)}")
        return {
            "transactionHash": receipt["transactionHash"],
            "gasUsed": int(receipt["gasUsed"], 16),
        }

    def snapshot(self) -> str:
        return self.rpc("evm_snapshot")

    def revert(self, snapshot_id: str) -> None:
        if self.rpc("evm_revert", snapshot_id) is not True:
            raise RuntimeError(f"evm_revert({snapshot_id}) failed")

    # ------------------------------------------------------------- measurements

    def breakdown(self, tx_hash: str) -> dict[str, Any]:
        return self.rpc("filecoin_getGasBreakdown", tx_hash)

    def trace(self, tx_hash: str) -> dict[str, Any]:
        trace = self.rpc("filecoin_getGasTrace", tx_hash)
        if not isinstance(trace, dict):
            raise RuntimeError(f"missing gas trace for {tx_hash}")
        return trace

    @staticmethod
    def subtree_milligas(call: dict[str, Any]) -> int:
        total = sum(int(charge["totalMilligas"], 16) for charge in call.get("gasCharges", []))
        for subcall in call.get("subcalls", []):
            total += FwssBenchmark.subtree_milligas(subcall)
        return total

    def skr_frames(self, trace: dict[str, Any]) -> list[dict[str, Any]]:
        """Collect every call frame that targets the SessionKeyRegistry actor."""
        if self.skr_actor is None:
            raise RuntimeError("SKR actor address not resolved yet (run the login benchmark first)")
        frames: list[dict[str, Any]] = []

        def walk(call: dict[str, Any]) -> None:
            if call.get("to") == self.skr_actor:
                frames.append(
                    {
                        "readOnly": call.get("readOnly"),
                        "milligas": self.subtree_milligas(call),
                    }
                )
            for subcall in call.get("subcalls", []):
                walk(subcall)

        for call in trace.get("calls", []):
            walk(call)
        return frames

    def record(self, op: str, variant: str, batch: int | None, dataset_state: str, receipt: dict[str, Any]) -> dict[str, Any]:
        tx_hash = receipt["transactionHash"]
        trace = self.trace(tx_hash)
        frames = self.skr_frames(trace) if self.skr_actor else []
        entry = {
            "op": op,
            "variant": variant,
            "batchSize": batch,
            "datasetState": dataset_state,
            "transactionHash": tx_hash,
            "gasUsed": receipt["gasUsed"],
            "breakdown": self.breakdown(tx_hash),
            "skrFrames": frames,
            "skrSubcallMilligas": sum(frame["milligas"] for frame in frames),
        }
        if variant == "payer" and frames:
            raise RuntimeError(f"payer-signed {op} unexpectedly called the SessionKeyRegistry: {frames}")
        if variant == "session" and op != "login" and not frames:
            raise RuntimeError(f"session-signed {op} produced no SessionKeyRegistry frame")
        return entry

    # ------------------------------------------------------------------- pieces

    def piece_batch(self, count: int) -> tuple[str, list[str]]:
        cid_bytes = []
        for _ in range(count):
            root = self.piece_index.to_bytes(32, "big")
            cid_bytes.append("0x" + (PIECE_PREFIX + root).hex())
            self.piece_index += 1
        tuples = "[" + ",".join(f"({cid})" for cid in cid_bytes) + "]"
        return tuples, cid_bytes

    # -------------------------------------------------------------------- setup

    def setup(self) -> None:
        log("funding accounts")
        for account in (self.payer, self.sp, self.session):
            self.rpc("anvil_setBalance", account, hex(FIL_BALANCE))
        self.rpc("anvil_setERC20Balance", self.payer, USDFC, hex(USDFC_BALANCE))
        balance = self.call_uint(USDFC, "balanceOf(address)(uint256)", self.payer)
        if balance != USDFC_BALANCE:
            raise RuntimeError(f"USDFC balance {balance} != requested {USDFC_BALANCE}")

        log("approving and depositing USDFC into FilecoinPay")
        self.send(PAYER_KEY, USDFC, "approve(address,uint256)", [FILECOIN_PAY, str(2**255)])
        self.send(PAYER_KEY, FILECOIN_PAY, "deposit(address,address,uint256)", [USDFC, self.payer, str(PAY_DEPOSIT)])
        self.send(
            PAYER_KEY,
            FILECOIN_PAY,
            "setOperatorApproval(address,address,bool,uint256,uint256,uint256)",
            [USDFC, FWSS_PROXY, "true", str(ALLOWANCE), str(ALLOWANCE), str(MAX_LOCKUP_PERIOD)],
        )

        log("registering benchmark storage provider")
        provider_id = self.call_uint(REGISTRY_PROXY, "getProviderIdByAddress(address)(uint256)", self.sp)
        if provider_id == 0:
            # The registry's PDP product schema requires these seven capability keys.
            pdp_keys = [
                "serviceURL",
                "minPieceSizeInBytes",
                "maxPieceSizeInBytes",
                "storagePricePerTibPerDay",
                "minProvingPeriodInEpochs",
                "location",
                "paymentTokenAddress",
            ]
            pdp_values = [
                "https://bench.invalid",
                "127",
                "34359738368",
                "1000000000000000000",
                "30",
                "local",
                ZERO_ADDRESS,
            ]
            keys_arg = "[" + ",".join(f'"{key}"' for key in pdp_keys) + "]"
            values_arg = "[" + ",".join("0x" + value.encode().hex() for value in pdp_values) + "]"
            self.send(
                SP_KEY,
                REGISTRY_PROXY,
                "registerProvider(address,string,string,uint8,string[],bytes[])",
                [self.sp, "fvm-anvil-bench", "", "0", keys_arg, values_arg],
                value=REGISTRATION_FEE,
            )
            provider_id = self.call_uint(REGISTRY_PROXY, "getProviderIdByAddress(address)(uint256)", self.sp)
        if provider_id == 0:
            raise RuntimeError("provider registration did not take effect")
        self.result["configuration"]["providerId"] = provider_id

        log("verifying signature path against deployed SignatureVerificationLib")
        digest = self.signer.create_data_set_digest(0, self.sp)
        signature = sign_digest(PAYER_KEY, digest)
        recovered = self.call(SIGNATURE_LIB, "recoverSigner(bytes32,bytes)(address)", digest, signature)
        if recovered.lower() != self.payer.lower():
            raise RuntimeError(f"recoverSigner returned {recovered}, expected {self.payer}")

    # ---------------------------------------------------------------------- ops

    def login(self) -> None:
        permissions = "[" + ",".join([self.signer.type_hashes["createDataSet"], self.signer.type_hashes["addPieces"]]) + "]"
        log("benchmarking SessionKeyRegistry.login (2 permissions)")
        receipt = self.send(
            PAYER_KEY,
            SESSION_KEY_REGISTRY,
            "login(address,uint256,bytes32[],string)",
            [self.session, str(SESSION_EXPIRY), permissions, "fvm-anvil-bench"],
        )
        trace = self.trace(receipt["transactionHash"])
        calls = trace.get("calls", [])
        if not calls:
            raise RuntimeError("login trace has no call frames")
        self.skr_actor = calls[0]["to"]
        self.result["configuration"]["skrActor"] = self.skr_actor
        expiry = self.call_uint(
            SESSION_KEY_REGISTRY,
            "authorizationExpiry(address,address,bytes32)(uint256)",
            self.payer,
            self.session,
            self.signer.type_hashes["addPieces"],
        )
        if expiry != SESSION_EXPIRY:
            raise RuntimeError(f"login did not set authorizationExpiry (got {expiry})")
        self.result["login"]["twoPermissions"] = self.record("login", "client", None, "n/a", receipt)

        log("benchmarking SessionKeyRegistry.login (1 permission)")
        single = self.send(
            PAYER_KEY,
            SESSION_KEY_REGISTRY,
            "login(address,uint256,bytes32[],string)",
            [self.extra_session, str(SESSION_EXPIRY), "[" + self.signer.type_hashes["addPieces"] + "]", "fvm-anvil-bench"],
        )
        self.result["login"]["onePermission"] = self.record("login", "client", None, "n/a", single)

    def create_data_set(self, variant: str) -> tuple[int, int, dict[str, Any]]:
        client_data_set_id = self.client_nonce
        self.client_nonce += 1
        digest = self.signer.create_data_set_digest(client_data_set_id, self.sp)
        key = PAYER_KEY if variant == "payer" else SESSION_KEY
        signature = sign_digest(key, digest)
        extra_data = abi_encode(
            "f(address,uint256,string[],string[],bytes)",
            self.payer,
            str(client_data_set_id),
            "[]",
            "[]",
            signature,
        )
        set_id = self.call_uint(PDP_PROXY, "getNextDataSetId()(uint64)")
        receipt = self.send(SP_KEY, PDP_PROXY, "createDataSet(address,bytes)", [FWSS_PROXY, extra_data], value=SYBIL_FEE)
        return set_id, client_data_set_id, receipt

    def add_pieces(self, set_id: int, client_data_set_id: int, count: int, variant: str) -> dict[str, Any]:
        nonce = self.client_nonce
        self.client_nonce += 1
        tuples, cid_bytes = self.piece_batch(count)
        digest = self.signer.add_pieces_digest(client_data_set_id, nonce, cid_bytes)
        key = PAYER_KEY if variant == "payer" else SESSION_KEY
        signature = sign_digest(key, digest)
        empty_matrix = "[" + ",".join("[]" for _ in range(count)) + "]"
        extra_data = abi_encode(
            "f(uint256,string[][],string[][],bytes)",
            str(nonce),
            empty_matrix,
            empty_matrix,
            signature,
        )
        return self.send(
            SP_KEY,
            PDP_PROXY,
            "addPieces(uint256,address,(bytes)[],bytes)",
            [str(set_id), ZERO_ADDRESS, tuples, extra_data],
        )

    # ---------------------------------------------------------------------- run

    def paired_create(self) -> None:
        for variant in ("payer", "session"):
            log(f"createDataSet ({variant})")
            snapshot_id = self.snapshot()
            _, _, receipt = self.create_data_set(variant)
            self.result["createDataSet"].append(self.record("createDataSet", variant, None, "fresh", receipt))
            self.revert(snapshot_id)

    def paired_sweep(self, set_id: int, client_data_set_id: int, dataset_state: str) -> None:
        for batch in SWEEP_BATCH_SIZES:
            for variant in ("payer", "session"):
                log(f"addPieces N={batch} ({variant}, {dataset_state})")
                snapshot_id = self.snapshot()
                receipt = self.add_pieces(set_id, client_data_set_id, batch, variant)
                self.result["addPieces"].append(self.record("addPieces", variant, batch, dataset_state, receipt))
                self.revert(snapshot_id)

    def run(self) -> None:
        self.setup()
        self.login()
        self.paired_create()

        log("creating working dataset for sweeps")
        set_id, client_data_set_id, receipt = self.create_data_set("payer")
        self.result["workingDataSet"] = {
            "dataSetId": set_id,
            "clientDataSetId": client_data_set_id,
            "createTransaction": receipt["transactionHash"],
        }
        self.paired_sweep(set_id, client_data_set_id, "fresh")

        log(f"pre-filling dataset to {PREFILL_TARGET} pieces")
        added = 0
        while added < PREFILL_TARGET:
            batch = min(PREFILL_BATCH, PREFILL_TARGET - added)
            self.add_pieces(set_id, client_data_set_id, batch, "payer")
            added += batch
        self.result["prefill"] = {"batches": (PREFILL_TARGET + PREFILL_BATCH - 1) // PREFILL_BATCH, "pieces": added}
        self.paired_sweep(set_id, client_data_set_id, "populated")

        self.write()
        log(f"results written to {self.output}")

    def write(self) -> None:
        self.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.output.with_suffix(self.output.suffix + ".tmp")
        temporary.write_text(json.dumps(self.result, indent=2, sort_keys=True) + "\n")
        os.replace(temporary, self.output)


def log(message: str) -> None:
    print(f"fwss-benchmark: {message}", file=sys.stderr, flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18548")
    parser.add_argument("--prepared-archive", default="snapshots/fwss-mainnet-v1.3.0-h6273120.press.car.zst")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).resolve().parent / "results" / "fwss-gas.json",
    )
    args = parser.parse_args()
    FwssBenchmark(args).run()


if __name__ == "__main__":
    main()
