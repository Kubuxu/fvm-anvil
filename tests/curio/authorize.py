#!/usr/bin/env python3
"""Register a local PDP provider, fund FilecoinPay, and sign FWSS requests."""

from __future__ import annotations

import argparse
import base64
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

import urllib.parse

from deploy_fwss import rpc

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
        error = result.stderr.strip()
        if "--private-key" in arguments:
            error = error.replace(str(arguments[arguments.index("--private-key") + 1]), "[redacted]")
        raise RuntimeError(error)
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


def storage_terms_id(value: str) -> str:
    if not re.fullmatch(r"0x[0-9a-fA-F]{64}", value):
        raise argparse.ArgumentTypeError("storage terms ID must be a 0x-prefixed bytes32")
    return value.lower()


class Authorization:
    def __init__(self, args: argparse.Namespace, manifest: dict) -> None:
        self.args = args
        self.manifest = manifest
        self.contracts = manifest["contracts"]
        self.signer = cast("wallet", "address", "--private-key", args.private_key)
        actual_chain = int(cast("chain-id", "--rpc-url", args.rpc_url))
        endpoint = urllib.parse.urlsplit(args.rpc_url)
        try:
            loopback = ipaddress.ip_address(endpoint.hostname or "").is_loopback
        except ValueError:
            loopback = endpoint.hostname == "localhost"
        if endpoint.scheme not in ("http", "https") or not loopback or endpoint.username or endpoint.password:
            raise ValueError("Authorization requires a loopback HTTP RPC URL")
        version = rpc(args.rpc_url, "web3_clientVersion", [])
        if not isinstance(version, str) or "fvm-anvil" not in version.lower():
            raise ValueError("Authorization requires a local fvm-anvil node")
        self.local_evidence = {"clientVersion": version, "stateRoots": self.state_roots()}
        if actual_chain != int(manifest["chainId"]):
            raise ValueError("Deployment manifest chain ID differs from the live node")
        self.chain_id = actual_chain

    def call(self, address: str, signature: str, *arguments: str) -> str:
        return cast("call", "--rpc-url", self.args.rpc_url, address, signature, *arguments)

    def uint(self, address: str, signature: str, *arguments: str) -> int:
        return int(self.call(address, signature, *arguments).split()[0])


    def state_roots(self) -> dict:
        roots = rpc(self.args.rpc_url, "filecoin_getStateRoots", [])
        if not isinstance(roots, dict) or not roots.get("current") or "prepared" not in roots:
            raise ValueError("fvm-anvil did not return prepared/current state roots")
        return roots

    def words(self, address: str, signature: str, *arguments: str) -> list[str]:
        """Decode all-static ABI results without depending on cast's display formatting."""
        data = self.call(address, signature, *arguments)
        if not re.fullmatch(r"0x(?:[0-9a-fA-F]{64})+", data):
            raise ValueError(f"Invalid static ABI result for {signature}")
        return [data[offset:offset + 64] for offset in range(2, len(data), 64)]

    def terms(self, terms_id: str) -> dict:
        words = self.words(self.contracts["fwss"], "getStorageTerms(bytes32)", terms_id)
        if len(words) != 5:
            raise ValueError("Unexpected getStorageTerms result")
        token = "0x" + words[0][-40:]
        decimals = int(words[1], 16)
        if decimals > 18:
            raise ValueError("Storage terms precision exceeds the supported 18 decimals")
        if int(token, 16) != 0 and self.uint(token, "decimals()(uint8)") != decimals:
            raise ValueError("Registered storage terms decimals differ from the live token")
        return {"id": terms_id, "token": token, "tokenDecimals": decimals,
                "salt": "0x" + words[2], "pricePerTiBPerMonth": int(words[3], 16),
                "enabled": bool(int(words[4], 16))}

    def send(self, address: str, signature: str, *arguments: str, value: int = 0) -> dict:
        print(f"Submitting {signature}", file=sys.stderr)
        gas_price = int(rpc(self.args.rpc_url, "eth_gasPrice", []), 16)
        tx_hash = cast(
            "send", "--async", "--rpc-url", self.args.rpc_url,
            "--private-key", self.args.private_key,
            "--gas-limit", "10000000000", "--gas-price", str(gas_price), "--priority-gas-price", "1",
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
        client_id = self.args.client_data_set_id
        terms = self.terms(self.args.storage_terms_id) if self.args.storage_terms_id is not None else None
        if terms is not None and not terms["enabled"]:
            raise ValueError("Selected storage terms are disabled")
        token = terms["token"] if terms is not None else self.contracts["token"]
        decimals = terms["tokenDecimals"] if terms is not None else 18
        amount = self.args.deposit if self.args.deposit is not None else 10 * 10**decimals
        if amount <= 0 or amount >= 2**256:
            raise ValueError("Deposit must be a positive uint256 in the token's smallest units")
        native = int(token, 16) == 0
        if self.args.fund_token_balance is not None and (native or not 0 <= self.args.fund_token_balance < 2**256):
            raise ValueError("--fund-token-balance requires an ERC-20 token and a uint256 balance")
        fwss = self.contracts["fwss"]
        if self.args.approve_provider and self.call(fwss, "owner()(address)").lower() != self.signer.lower():
            raise ValueError("--approve-provider requires the FWSS owner signing key")
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
        approval = None
        if self.args.approve_provider:
            already_approved = self.call(fwss, "isProviderApproved(uint256)(bool)", str(provider_id)) == "true"
            if not already_approved:
                receipts.append(self.send(fwss, "addApprovedProvider(uint256)", str(provider_id)))
            if self.call(fwss, "isProviderApproved(uint256)(bool)", str(provider_id)) != "true":
                raise RuntimeError("FWSS provider approval was not stored")
            # Candidate 371e8b0a requires registry membership, not an endorsement-set entry.
            approval = {"providerId": provider_id, "alreadyApproved": already_approved, "approved": True}
        funding = None
        if self.args.fund_token_balance is not None:
            code = rpc(self.args.rpc_url, "eth_getCode", [token, "latest"])
            if code in ("0x", "0x0"):
                raise ValueError("Selected payment token has no runtime code")
            before = self.uint(token, "balanceOf(address)(uint256)", self.signer)
            roots_before = self.state_roots()
            rpc(self.args.rpc_url, "anvil_setERC20Balance", [self.signer, token, hex(self.args.fund_token_balance)])
            balance = self.uint(token, "balanceOf(address)(uint256)", self.signer)
            if balance != self.args.fund_token_balance:
                raise RuntimeError("Local ERC-20 funding did not set the requested balance")
            if rpc(self.args.rpc_url, "eth_getCode", [token, "latest"]) != code:
                raise RuntimeError("Local ERC-20 funding changed the token runtime code")
            funding = {"method": "anvil_setERC20Balance", "token": token, "owner": self.signer,
                       "balanceBefore": before, "balanceAfter": balance,
                       "runtimeCodeHash": cast("keccak", code), "runtimeCodePreserved": True,
                       "stateRootsBefore": roots_before, "stateRootsAfter": self.state_roots()}
        balance = (int(rpc(self.args.rpc_url, "eth_getBalance", [self.signer, "latest"]), 16)
                   if native else self.uint(token, "balanceOf(address)(uint256)", self.signer))
        if balance < amount:
            raise ValueError(f"Payment-token balance {balance} is below requested deposit {amount}")
        if not native:
            receipts.append(self.send(token, "approve(address,uint256)", self.contracts["payments"], str(amount)))
        funds_before = int(self.words(self.contracts["payments"], "accounts(address,address)", token, self.signer)[0], 16)
        deposit_receipt = self.send(self.contracts["payments"], "deposit(address,address,uint256)",
                                    token, self.signer, str(amount), value=amount if native else 0)
        receipts.append(deposit_receipt)
        funds_after = int(self.words(self.contracts["payments"], "accounts(address,address)", token, self.signer)[0], 16)
        if funds_after < funds_before + amount:
            raise RuntimeError("FilecoinPay deposit did not credit the requested amount")
        receipts.append(self.send(
            self.contracts["payments"], "setOperatorApproval(address,address,bool,uint256,uint256,uint256)",
            token, self.contracts["fwss"], "true", str(amount), str(amount), "31536000",
        ))
        fields = [
            {"name": "clientDataSetId", "type": "uint256"},
            {"name": "payee", "type": "address"},
            {"name": "metadata", "type": "MetadataEntry[]"},
        ]
        message = {"clientDataSetId": client_id, "payee": payee, "metadata": []}
        primary_type = "CreateDataSet"
        encoding = "f(address,uint256,string[],string[],bytes)"
        tail = []
        if terms is not None:
            primary_type = "CreateDataSetWithStorageTerms"
            fields.append({"name": "storageTermsId", "type": "bytes32"})
            message["storageTermsId"] = terms["id"]
            encoding = "f(address,uint256,string[],string[],bytes,bytes32)"
            tail.append(terms["id"])
        signature = self.sign(primary_type, fields, message)
        extra = cast("abi-encode", encoding, self.signer, str(client_id), "[]", "[]", signature, *tail)
        return {"providerId": provider_id, "provider": provider, "payer": self.signer,
                "clientDataSetId": client_id, "nextNonce": client_id + 1,
                "createExtraData": extra, "metadataKeys": [], "signatureType": primary_type,
                "storageTerms": terms, "paymentToken": token, "tokenDecimals": decimals,
                "deposit": {"amount": amount, "fundsBefore": funds_before, "fundsAfter": funds_after,
                            "transactionHash": deposit_receipt["transactionHash"]},
                "localNode": self.local_evidence, "localTokenFunding": funding,
                "providerApproval": approval, "setupReceipts": receipts,
                "setupTransactions": [r["transactionHash"] for r in receipts]}

    def verify_storage_terms(self, data_set_id: str, authorization: dict) -> dict:
        selected = authorization["storageTerms"]
        terms = self.terms(selected["id"])
        for key in ("token", "tokenDecimals", "salt", "pricePerTiBPerMonth"):
            if terms[key] != selected[key]:
                raise RuntimeError("Selected immutable storage terms changed since authorization")
        words = self.words(self.contracts["fwss"], "getDataSetV2(uint256)", data_set_id)
        if len(words) != 14:
            raise RuntimeError("Unexpected getDataSetV2 result")
        names = ("dataSetId", "pdpRailId", "cacheMissRailId", "cdnRailId", "payer", "payee",
                 "serviceProvider", "commissionBps", "clientDataSetId", "pdpEndEpoch", "providerId",
                 "pendingOneTimePayments", "lifecycleReserveBalance", "storageTermsId")
        info = {name: ("0x" + word[-40:] if name in ("payer", "payee", "serviceProvider") else
                       "0x" + word if name == "storageTermsId" else int(word, 16))
                for name, word in zip(names, words)}
        if info["dataSetId"] != int(data_set_id) or info["storageTermsId"] != selected["id"]:
            raise RuntimeError("getDataSetV2 does not store the selected storage terms")
        if info["payer"].lower() != self.signer.lower():
            raise RuntimeError("getDataSetV2 payer differs from the signer")
        rail_words = self.words(self.contracts["payments"], "getRail(uint256)", str(info["pdpRailId"]))
        if len(rail_words) != 12:
            raise RuntimeError("Unexpected FilecoinPay getRail result")
        rail_names = ("token", "from", "to", "operator", "validator", "paymentRate", "lockupPeriod",
                      "lockupFixed", "settledUpTo", "endEpoch", "commissionRateBps", "serviceFeeRecipient")
        rail = {name: ("0x" + word[-40:] if index < 5 or index == 11 else int(word, 16))
                for index, (name, word) in enumerate(zip(rail_names, rail_words))}
        if rail["token"].lower() != terms["token"].lower() or rail["from"].lower() != self.signer.lower():
            raise RuntimeError("FilecoinPay rail currency or payer differs from the signed terms")
        if rail["operator"].lower() != self.contracts["fwss"].lower() or rail["to"].lower() != info["payee"].lower():
            raise RuntimeError("FilecoinPay rail operator or payee differs from FWSS")
        leaves = self.uint(self.contracts["pdp"], "getDataSetLeafCount(uint256)(uint256)", data_set_id)
        raw_size = leaves * 32 * 127 // 128
        # StoragePricing at candidate 371e8b0a: floor raw-size monthly storage first,
        # add the 18-decimal catalogue's scaled dataset fee, then floor per epoch.
        prices = self.words(self.contracts["fwss"], "getCurrentPricingRates()")
        dataset_fee = int(prices[1], 16) // 10**(18 - terms["tokenDecimals"])
        monthly_storage = raw_size * terms["pricePerTiBPerMonth"] // (1 << 40)
        epochs_per_month = 2880 * 30
        expected_rate = (monthly_storage + dataset_fee) // epochs_per_month if leaves else 0
        if leaves == 0 or rail["paymentRate"] != expected_rate:
            raise RuntimeError(f"FilecoinPay rail rate {rail['paymentRate']} differs from actual leafcount terms rate {expected_rate}")
        return {"storageTerms": terms, "dataSet": info, "rail": rail, "leafCount": leaves,
                "rawSizeBytes": raw_size, "monthlyStorage": monthly_storage,
                "datasetFeePerMonth": dataset_fee, "epochsPerMonth": epochs_per_month,
                "expectedPaymentRate": expected_rate, "stateRoots": self.state_roots()}

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
    parser.add_argument("--deposit", type=int, help="Smallest token units; default 10 tokens (selected terms precision)")
    parser.add_argument("--storage-terms-id", type=storage_terms_id, help="Registered enabled terms; uses the six-field signed payload, including explicit ID zero")
    parser.add_argument("--fund-token-balance", type=int, help="Locally set the signer's ERC-20 balance in smallest units before the real deposit")
    parser.add_argument("--approve-provider", action="store_true", help="As FWSS owner, approve the provider if not already approved")
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
