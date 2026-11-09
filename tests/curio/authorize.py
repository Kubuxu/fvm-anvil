#!/usr/bin/env python3
"""Register a local PDP provider, fund FilecoinPay, and sign FWSS requests."""

from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time


DOMAIN_FIELDS = [
    {"name": "name", "type": "string"},
    {"name": "version", "type": "string"},
    {"name": "chainId", "type": "uint256"},
    {"name": "verifyingContract", "type": "address"},
]
METADATA_FIELDS = [
    {"name": "key", "type": "string"},
    {"name": "value", "type": "string"},
]


def cast(*arguments: str) -> str:
    result = subprocess.run(
        ["cast", *map(str, arguments)], capture_output=True, text=True, check=False
    )
    if result.returncode:
        # Never include the command line: it may contain the private key.
        raise RuntimeError(result.stderr.strip())
    return result.stdout.strip()


def integer_bytes(value: int) -> str:
    return "0x" + value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big").hex()


def piece_bytes(value: str) -> str:
    if value.startswith("0x"):
        raw = bytes.fromhex(value[2:])
    elif value.startswith("b"):
        payload = value[1:].upper()
        raw = base64.b32decode(payload + "=" * (-len(payload) % 8))
    else:
        raise ValueError("Pass the base32 PieceCIDv2 used in the add-pieces request")
    # FRC-0069 CIDv1/raw with the fr32-sha2-256-trunc254-padded-binary-tree multihash.
    if not raw.startswith(bytes.fromhex("01559120")):
        raise ValueError("Signing requires PieceCIDv2 data, not the database's PieceCIDv1")
    return "0x" + raw.hex()


class Authorization:
    def __init__(self, args: argparse.Namespace, manifest: dict) -> None:
        self.args = args
        self.manifest = manifest
        self.contracts = manifest["contracts"]
        self.signer = cast("wallet", "address", "--private-key", args.private_key)
        actual_chain = int(cast("chain-id", "--rpc-url", args.rpc_url))
        if actual_chain != int(manifest["chainId"]):
            raise ValueError("Deployment manifest chain ID differs from the live node")
        self.chain_id = actual_chain

    def call(self, address: str, signature: str, *arguments: str) -> str:
        return cast("call", "--rpc-url", self.args.rpc_url, address, signature, *arguments)

    def uint(self, address: str, signature: str, *arguments: str) -> int:
        return int(self.call(address, signature, *arguments).split()[0])

    def send(self, address: str, signature: str, *arguments: str, value: int = 0) -> dict:
        print(f"Submitting {signature}", file=sys.stderr)
        tx_hash = cast(
            "send", "--async", "--rpc-url", self.args.rpc_url,
            "--private-key", self.args.private_key,
            "--gas-limit", "10000000000", "--gas-price", "200", "--priority-gas-price", "1",
            "--value", str(value), address, signature, *arguments,
        )
        deadline = time.monotonic() + self.args.timeout
        while time.monotonic() < deadline:
            receipt = json.loads(cast("rpc", "--rpc-url", self.args.rpc_url,
                                      "eth_getTransactionReceipt", tx_hash))
            if receipt is not None:
                if int(receipt["status"], 16) != 1:
                    raise RuntimeError(f"{signature} reverted: {json.dumps(receipt)}")
                return receipt
            time.sleep(0.2)
        raise TimeoutError(f"Receipt unavailable for {tx_hash}")

    def sign(self, primary_type: str, fields: list[dict], message: dict, *, extra_types: dict | None = None) -> str:
        data = {
            "types": {"EIP712Domain": DOMAIN_FIELDS, "MetadataEntry": METADATA_FIELDS,
                      primary_type: fields, **(extra_types or {})},
            "primaryType": primary_type,
            "domain": {"name": "FilecoinWarmStorageService", "version": "1",
                       "chainId": self.chain_id, "verifyingContract": self.contracts["fwss"]},
            "message": message,
        }
        return cast("wallet", "sign", "--private-key", self.args.private_key,
                    "--data", json.dumps(data, separators=(",", ":")))

    def create(self) -> dict:
        provider = self.args.provider_address or self.signer
        registry = self.contracts["registry"]
        token = self.contracts["token"]
        client_id = self.args.client_data_set_id
        if client_id is None:
            client_id = self.uint(self.contracts["pdp"], "getNextDataSetId()(uint64)")
            while self.uint(self.contracts["fwss"], "clientNonces(address,uint256)(uint256)", self.signer, str(client_id)):
                client_id += 1
        elif self.uint(self.contracts["fwss"], "clientNonces(address,uint256)(uint256)", self.signer, str(client_id)):
            raise ValueError(f"Client dataset nonce {client_id} was already consumed")
        provider_id = self.uint(registry, "getProviderIdByAddress(address)(uint256)", provider)
        receipts = []
        if provider_id == 0:
            if provider.lower() != self.signer.lower():
                raise ValueError("Register the provider with its own key, or use the payer as provider")
            capabilities = {
                "serviceURL": "0x" + self.args.service_url.encode().hex(),
                "minPieceSizeInBytes": integer_bytes(128),
                "maxPieceSizeInBytes": integer_bytes(32 << 30),
                "storagePricePerTibPerDay": integer_bytes(1),
                "minProvingPeriodInEpochs": integer_bytes(int(self.manifest.get("parameters", {}).get("maxProvingPeriod", 120))),
                "location": "0x" + b"local".hex(),
                "paymentTokenAddress": token,
            }
            receipts.append(self.send(
                registry, "registerProvider(address,string,string,uint8,string[],bytes[])",
                provider, "fvm-anvil PDP", "Local writable PDP acceptance", "0",
                json.dumps(list(capabilities)), "[" + ",".join(capabilities.values()) + "]",
                value=self.uint(registry, "REGISTRATION_FEE()(uint256)"),
            ))
            provider_id = self.uint(registry, "getProviderIdByAddress(address)(uint256)", provider)
        if provider_id == 0:
            raise RuntimeError("Provider registration did not produce a provider ID")
        payee = self.call(registry, "getProviderPayee(uint256)(address)", str(provider_id)).strip()
        amount = self.args.deposit
        balance = self.uint(token, "balanceOf(address)(uint256)", self.signer)
        if balance < amount:
            raise ValueError(f"Payment-token balance {balance} is below requested deposit {amount}")
        receipts.append(self.send(token, "approve(address,uint256)", self.contracts["payments"], str(amount)))
        receipts.append(self.send(self.contracts["payments"], "deposit(address,address,uint256)",
                                  token, self.signer, str(amount)))
        receipts.append(self.send(
            self.contracts["payments"], "setOperatorApproval(address,address,bool,uint256,uint256,uint256)",
            token, self.contracts["fwss"], "true", str(amount), str(amount), "31536000",
        ))
        signature = self.sign("CreateDataSet", [
            {"name": "clientDataSetId", "type": "uint256"},
            {"name": "payee", "type": "address"},
            {"name": "metadata", "type": "MetadataEntry[]"},
        ], {"clientDataSetId": client_id, "payee": payee, "metadata": []})
        extra = cast("abi-encode", "f(address,uint256,string[],string[],bytes)",
                     self.signer, str(client_id), "[]", "[]", signature)
        return {"providerId": provider_id, "provider": provider, "payer": self.signer,
                "clientDataSetId": client_id, "nextNonce": client_id + 1,
                "createExtraData": extra, "metadataKeys": [],
                "setupTransactions": [r["transactionHash"] for r in receipts]}

    def add(self, authorization: dict) -> dict:
        if authorization["payer"].lower() != self.signer.lower():
            raise ValueError("Authorization payer differs from signing key")
        if self.args.data_set_id is None or not self.args.piece_cids:
            raise ValueError("add requires --data-set-id and --piece-cids")
        nonce = authorization["nextNonce"]
        while self.uint(self.contracts["fwss"], "clientNonces(address,uint256)(uint256)", self.signer, str(nonce)):
            nonce += 1
        cids = [piece_bytes(cid) for cid in self.args.piece_cids]
        signature = self.sign("AddPieces", [
            {"name": "clientDataSetId", "type": "uint256"},
            {"name": "nonce", "type": "uint256"},
            {"name": "pieceData", "type": "Cid[]"},
            {"name": "pieceMetadata", "type": "PieceMetadata[]"},
        ], {"clientDataSetId": authorization["clientDataSetId"], "nonce": nonce,
            "pieceData": [{"data": cid} for cid in cids], "pieceMetadata": []},
            extra_types={"Cid": [{"name": "data", "type": "bytes"}],
                         "PieceMetadata": [{"name": "pieceIndex", "type": "uint256"},
                                           {"name": "metadata", "type": "MetadataEntry[]"}]})
        authorization.update({"dataSetId": self.args.data_set_id, "nextNonce": nonce + 1,
                              "pieceCidBytes": cids,
                              "addExtraData": cast("abi-encode", "f(uint256,string[][],string[][],bytes)",
                                                   str(nonce), "[]", "[]", signature)})
        return authorization


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18545/rpc/v1")
    parser.add_argument("--private-key", default=os.environ.get("PRIVATE_KEY"))
    parser.add_argument("--provider-address")
    parser.add_argument("--service-url", default="http://127.0.0.1:4702")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--operation", choices=("create", "add"), default="create")
    parser.add_argument("--deposit", type=int, default=10 * 10**18)
    parser.add_argument("--client-data-set-id", type=int)
    parser.add_argument("--data-set-id", type=int)
    parser.add_argument("--piece-cids", nargs="+")
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    if not args.private_key:
        parser.error("--private-key or PRIVATE_KEY is required")
    manifest = json.loads(args.manifest.read_text())
    helper = Authorization(args, manifest)
    result = helper.create() if args.operation == "create" else helper.add(json.loads(args.output.read_text()))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, TimeoutError) as error:
        print(f"Authorization failed: {error}", file=sys.stderr)
        sys.exit(1)
