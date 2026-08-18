#!/usr/bin/env python3
"""Measure the PR #536 per-dataset authorizer gas on prepared mainnet state.

Upgrades the mainnet FWSS proxy in place (ERC-1967 implementation slot write)
to the FilOzone/filecoin-services PR #536 build, attaches authorizers to a
working dataset, and measures every gated operation (addPieces,
schedulePieceRemovals via PDPVerifier.schedulePieceDeletions, terminateService)
under three variants from identical pre-state:

  baseline  no authorizer attached; normal payer EIP-712 signature path
  trivial   TrivialAuthorizer: returns true immediately (dispatch overhead)
  burner    BurnerAuthorizer: burns the forwarded AUTHORIZER_GAS_LIMIT
            (150M FVM gas) stipend down to a small reserve, then returns true
            (the pricing worst case)

Requires: fvm-anvil running on the prepared FWSS archive; the PR #536 branch
built in the filecoin-services clone (forge build); the authorizer scratch
project built (see --authorizer-out).
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

from eip712 import abi_encode, command, keccak, sign_digest
from run import (
    FWSS_PROXY,
    PAYER_KEY,
    PDP_PROXY,
    SP_KEY,
    ZERO_ADDRESS,
    FwssBenchmark,
)

ERC1967_IMPL_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
SCHEDULE_PIECE_REMOVALS_TYPE = "SchedulePieceRemovals(uint256 clientDataSetId,uint256[] pieceIds)"
TERMINATE_SERVICE_TYPE = "TerminateService(uint256 dataSetId)"

SWEEP_BATCH_SIZES = (1, 8, 26, 40)
REMOVAL_COUNT = 5  # mainnet 30d average pieces per removal tx
PREFILL_PIECES = 120
BURNER_RESERVE_GAS = 3_000_000
AUTHORIZER_GAS_LIMIT = 150_000_000


class AuthorizerBenchmark(FwssBenchmark):
    def __init__(self, args: argparse.Namespace) -> None:
        super().__init__(args)
        self.services_repo = Path(args.services_repo).resolve()
        self.authorizer_out = Path(args.authorizer_out).resolve()
        self.result["configuration"]["sweepBatchSizes"] = list(SWEEP_BATCH_SIZES)
        self.result["configuration"]["authorizerGasLimit"] = AUTHORIZER_GAS_LIMIT
        self.result["configuration"]["burnerReserveGas"] = BURNER_RESERVE_GAS
        self.result["operations"] = []
        del self.result["login"], self.result["createDataSet"], self.result["addPieces"]

    # ------------------------------------------------------------------ deploy

    def forge_create(self, workdir: Path, target: str, *, constructor_args: list[str] | None = None,
                     libraries: list[str] | None = None) -> str:
        args = [
            "forge", "create", target,
            "--rpc-url", self.rpc_url,
            "--private-key", PAYER_KEY,
            "--broadcast", "--json",
            "--gas-limit", "10000000000",
            "--gas-price", str(self.max_fee_per_gas),
            "--priority-gas-price", "1",
            "--root", str(workdir),
        ]
        for library in libraries or []:
            args.extend(["--libraries", library])
        if constructor_args:
            args.append("--constructor-args")
            args.extend(constructor_args)
        out = command(args)
        deployed = json.loads(out[out.index("{"):])["deployedTo"]
        log(f"deployed {target.split(':')[-1]} at {deployed}")
        return deployed

    def upgrade_fwss(self) -> None:
        beneficiary = self.call(FWSS_PROXY, "filBeamBeneficiaryAddress()(address)").split()[0]
        registry = self.call(FWSS_PROXY, "serviceProviderRegistry()(address)").split()[0]
        skr = self.call(FWSS_PROXY, "sessionKeyRegistry()(address)").split()[0]
        usdfc = self.call(FWSS_PROXY, "usdfcTokenAddress()(address)").split()[0]
        repo = self.services_repo / "service_contracts"
        rails = self.forge_create(repo, "src/lib/Rails.sol:Rails")
        sig_lib = self.forge_create(repo, "src/lib/SignatureVerificationLib.sol:SignatureVerificationLib")
        libraries = [
            f"src/lib/Rails.sol:Rails:{rails}",
            f"src/lib/SignatureVerificationLib.sol:SignatureVerificationLib:{sig_lib}",
        ]
        impl = self.forge_create(
            repo,
            "src/FilecoinWarmStorageService.sol:FilecoinWarmStorageService",
            constructor_args=[PDP_PROXY, self.call(FWSS_PROXY, "paymentsContractAddress()(address)").split()[0],
                              usdfc, beneficiary, registry, skr, "9"],
            libraries=libraries,
        )
        slot_value = "0x" + impl[2:].rjust(64, "0").lower()
        self.rpc("anvil_setStorageAt", FWSS_PROXY, ERC1967_IMPL_SLOT, slot_value)
        stored = self.rpc("eth_getStorageAt", FWSS_PROXY, ERC1967_IMPL_SLOT, "latest")
        if stored.lower() != slot_value:
            raise RuntimeError(f"implementation slot write failed: {stored}")
        self.result["configuration"]["pr536Implementation"] = impl
        log(f"FWSS proxy implementation swapped to PR #536 build ({impl})")

    def deploy_authorizers(self) -> None:
        self.trivial = self.forge_create(self.authorizer_out, "src/Authorizers.sol:TrivialAuthorizer")
        self.burner = self.forge_create(
            self.authorizer_out, "src/Authorizers.sol:BurnerAuthorizer",
            constructor_args=[str(BURNER_RESERVE_GAS)],
        )
        self.result["configuration"]["trivialAuthorizer"] = self.trivial
        self.result["configuration"]["burnerAuthorizer"] = self.burner

    def set_authorizer(self, set_id: int, authorizer: str) -> None:
        self.send(PAYER_KEY, FWSS_PROXY, "setDataSetAuthorizer(uint256,address)", [str(set_id), authorizer])

    # ------------------------------------------------------------------ digests

    def schedule_removals_digest(self, client_data_set_id: int, piece_ids: list[int]) -> str:
        typehash = keccak(SCHEDULE_PIECE_REMOVALS_TYPE)
        packed = "0x" + "".join(f"{pid:064x}" for pid in piece_ids)
        struct_hash = keccak(abi_encode(
            "f(bytes32,uint256,bytes32)", typehash, str(client_data_set_id), keccak(packed),
        ))
        return self.signer.digest(struct_hash)

    def terminate_digest(self, data_set_id: int) -> str:
        typehash = keccak(TERMINATE_SERVICE_TYPE)
        struct_hash = keccak(abi_encode("f(bytes32,uint256)", typehash, str(data_set_id)))
        return self.signer.digest(struct_hash)

    # ---------------------------------------------------------------------- ops

    def schedule_removals(self, set_id: int, client_data_set_id: int, piece_ids: list[int]) -> dict:
        digest = self.schedule_removals_digest(client_data_set_id, piece_ids)
        signature = sign_digest(PAYER_KEY, digest)
        extra_data = abi_encode("f(bytes)", signature)
        ids_arg = "[" + ",".join(str(pid) for pid in piece_ids) + "]"
        return self.send(
            SP_KEY, PDP_PROXY,
            "schedulePieceDeletions(uint256,uint256[],bytes)",
            [str(set_id), ids_arg, extra_data],
        )

    def terminate(self, set_id: int) -> dict:
        digest = self.terminate_digest(set_id)
        signature = sign_digest(PAYER_KEY, digest)
        extra_data = abi_encode("f(bytes)", signature)
        return self.send(SP_KEY, FWSS_PROXY, "terminateService(uint256,bytes)", [str(set_id), extra_data])

    # ---------------------------------------------------------------- recording

    def record_op(self, op: str, variant: str, batch: int | None, receipt: dict) -> dict:
        entry = {
            "op": op,
            "variant": variant,
            "batchSize": batch,
            "transactionHash": receipt["transactionHash"],
            "gasUsed": receipt["gasUsed"],
            "breakdown": self.breakdown(receipt["transactionHash"]),
        }
        self.result["operations"].append(entry)
        log(f"{op} variant={variant} N={batch}: {receipt['gasUsed']:,} gas")
        return entry

    # ---------------------------------------------------------------------- run

    def measure_variant(self, variant: str, authorizer: str | None,
                        set_id: int, client_data_set_id: int, piece_ids: list[int]) -> None:
        outer = self.snapshot()
        if authorizer is not None:
            self.set_authorizer(set_id, authorizer)
        for batch in SWEEP_BATCH_SIZES:
            inner = self.snapshot()
            receipt = self.add_pieces(set_id, client_data_set_id, batch, "payer")
            self.record_op("addPieces", variant, batch, receipt)
            self.revert(inner)
        inner = self.snapshot()
        receipt = self.schedule_removals(set_id, client_data_set_id, piece_ids)
        self.record_op("schedulePieceRemovals", variant, len(piece_ids), receipt)
        self.revert(inner)
        inner = self.snapshot()
        receipt = self.terminate(set_id)
        self.record_op("terminateService", variant, None, receipt)
        self.revert(inner)
        self.revert(outer)

    def run(self) -> None:
        self.setup()
        self.upgrade_fwss()
        self.deploy_authorizers()

        log("creating working dataset")
        set_id, client_data_set_id, _ = self.create_data_set("payer")
        log(f"pre-filling dataset with {PREFILL_PIECES} pieces")
        added = 0
        while added < PREFILL_PIECES:
            batch = min(40, PREFILL_PIECES - added)
            self.add_pieces(set_id, client_data_set_id, batch, "payer")
            added += batch
        piece_ids = list(range(REMOVAL_COUNT))
        self.result["workingDataSet"] = {"dataSetId": set_id, "clientDataSetId": client_data_set_id,
                                          "prefilledPieces": added}

        self.measure_variant("baseline", None, set_id, client_data_set_id, piece_ids)
        self.measure_variant("trivial", self.trivial, set_id, client_data_set_id, piece_ids)
        self.measure_variant("burner", self.burner, set_id, client_data_set_id, piece_ids)

        # Sanity: the burner must actually consume (nearly) the full stipend.
        for op in ("addPieces", "schedulePieceRemovals", "terminateService"):
            base = min(e["gasUsed"] for e in self.result["operations"]
                       if e["op"] == op and e["variant"] == "baseline")
            burn = min(e["gasUsed"] for e in self.result["operations"]
                       if e["op"] == op and e["variant"] == "burner")
            delta = burn - base
            if delta < 100_000_000:
                raise RuntimeError(f"{op}: burner delta only {delta:,} gas; stipend not consumed")

        self.write()
        log(f"results written to {self.output}")


def log(message: str) -> None:
    print(f"authorizer-benchmark: {message}", file=sys.stderr, flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18548")
    parser.add_argument("--prepared-archive", default="")
    parser.add_argument("--services-repo", required=True,
                        help="filecoin-services clone with the PR #536 branch built")
    parser.add_argument("--authorizer-out", required=True,
                        help="forge project containing src/Authorizers.sol, built")
    parser.add_argument("--output", type=Path,
                        default=Path(__file__).resolve().parent / "results" / "fwss-authorizer-gas.json")
    args = parser.parse_args()
    AuthorizerBenchmark(args).run()


if __name__ == "__main__":
    main()
