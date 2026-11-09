#!/usr/bin/env python3
"""Deploy the checked-out FWSS stack without modifying extern sources.

The default payment asset is the official, collateral-backed Solady WETH (not
USDFC and not a mock). Supply --token-address for an existing 18-decimal USDFC.
Build artifacts and receipts are retained beside the output manifest.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import urllib.request


def command(args, cwd=None, env=None):
    result = subprocess.run([str(x) for x in args], cwd=cwd, env=env,
                            text=True, stdout=subprocess.PIPE, stderr=sys.stderr)
    if result.returncode:
        if result.stdout:
            print(result.stdout, file=sys.stderr)
        raise RuntimeError(f"command failed ({result.returncode}): {args[0]}")
    return result.stdout.strip()


def rpc(url, method, params):
    request = urllib.request.Request(url, json.dumps({"jsonrpc": "2.0", "id": 1,
        "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=300) as response:
        value = json.load(response)
    if "error" in value:
        raise RuntimeError(f"{method}: {value['error']}")
    return value["result"]


def finalize_deployment(args, source, output, build, revisions, chain_id, owner):
    """Validate confirmed broadcast state and publish its deployment manifest."""
    broadcast_path = build / "broadcast/DeployFWSS.s.sol" / str(chain_id) / "run-latest.json"
    broadcast = json.loads(broadcast_path.read_text())
    # Foundry records Solidity string returns as escaped string contents.
    contracts = json.loads(json.loads('"' + broadcast["returns"]["manifest"]["value"] + '"'))
    transactions = broadcast["transactions"]
    receipts = broadcast["receipts"]
    if not receipts or len(receipts) != len(transactions) or any(int(receipt["status"], 16) != 1 for receipt in receipts):
        raise RuntimeError(f"deployment has unsuccessful or missing receipts: {broadcast_path}")
    deployments = [{"name": tx.get("contractName"), "address": tx.get("contractAddress"),
                    "hash": tx.get("hash"), "type": tx.get("transactionType")} for tx in transactions
                   if tx.get("transactionType") in ("CREATE", "CREATE2")]
    for address in {item["address"] for item in deployments if item["address"]} | set(contracts.values()):
        if rpc(args.rpc_url, "eth_getCode", [address, "latest"]) in ("0x", "0x0"):
            raise RuntimeError(f"deployed contract has no runtime code: {address}")
    def call(address, signature):
        return command(["cast", "call", address, signature, "--rpc-url", args.rpc_url]).strip('"')
    versions = {name: call(contracts[key], "VERSION()(string)")
                for name, key in (("fwss", "fwss"), ("pdp", "pdp"), ("registry", "registry"))}
    for key in ("fwss", "pdp", "registry"):
        if call(contracts[key], "owner()(address)").lower() != owner.lower():
            raise RuntimeError(f"unexpected proxy owner: {key}")
    if int(call(contracts["token"], "decimals()(uint8)")) != 18:
        raise RuntimeError("FWSS requires an 18-decimal token")
    if call(contracts["fwss"], "viewContractAddress()(address)").lower() != contracts["view"].lower():
        raise RuntimeError("FWSS view address mismatch")
    libraries = {}
    for entry in broadcast.get("libraries", []):
        library_source, library_name, library_address = entry.rsplit(":", 2)
        libraries[f"{library_source}:{library_name}"] = library_address
    curio_env = {"CURIO_DEVNET_PDP_VERIFIER_ADDRESS": contracts["pdp"],
                 "CURIO_DEVNET_FWSS_ADDRESS": contracts["fwss"],
                 "CURIO_DEVNET_SERVICE_REGISTRY_ADDRESS": contracts["registry"]}
    manifest = {"schemaVersion": 1, "chainId": chain_id, "rpcUrl": args.rpc_url, "deployer": owner,
                "owner": owner, "sourceRevisions": revisions, "sourceDirectory": str(source),
                "buildDirectory": str(build), "artifactDirectory": str(build / "out"),
                "broadcastFile": str(broadcast_path), "contracts": contracts, "versions": versions,
                "libraries": libraries, "deployments": deployments, "receipts": receipts,
                "curioEnv": curio_env, "paymentTokenKind": "external" if args.token_address else "wrapped-native",
                "wrappedAmount": "0" if args.token_address else str(args.wrap_amount),
                "parameters": {"challengeFinality": args.challenge_finality, "maxProvingPeriod": args.max_proving_period,
                    "challengeWindowSize": args.challenge_window_size, "filBeamBeneficiary": owner, "filBeamController": owner}}
    temporary = output.with_name(output.name + ".tmp")
    temporary.write_text(json.dumps(manifest, indent=2) + "\n")
    temporary.replace(output)
    print(json.dumps(manifest))
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18545/rpc/v1")
    parser.add_argument("--private-key", default=os.environ.get("PRIVATE_KEY", os.environ.get("ETH_PRIVATE_KEY")))
    parser.add_argument("--source", type=Path, default=Path("extern/filecoin-services/service_contracts"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-directory", type=Path)
    parser.add_argument("--token-address", help="Existing real 18-decimal payment token; default deploys official WETH")
    parser.add_argument("--wrap-amount", type=int, default=100 * 10**18, help="Native wei deposited into the new WETH")
    parser.add_argument("--challenge-finality", type=int, default=5)
    parser.add_argument("--max-proving-period", type=int, default=120)
    parser.add_argument("--challenge-window-size", type=int, default=10)
    args = parser.parse_args()
    if not args.private_key:
        parser.error("--private-key or PRIVATE_KEY/ETH_PRIVATE_KEY is required")
    if not 0 < args.challenge_window_size < args.max_proving_period <= 2**64 - 1:
        parser.error("require 0 < challenge window < proving period <= uint64 max")
    if args.challenge_finality < 0 or args.max_proving_period < args.challenge_finality + args.challenge_window_size // 2:
        parser.error("proving period must accommodate finality plus half the challenge window")
    if args.wrap_amount <= 0:
        parser.error("--wrap-amount must be positive")
    source = args.source.resolve(strict=True)
    output = args.output.resolve()
    build = (args.build_directory or output.with_suffix(".build")).resolve()
    if build == source or source in build.parents or build in source.parents:
        parser.error("build directory must be outside the upstream source tree")
    if output.exists() or build.exists():
        parser.error("output and build directory must not exist (fresh deployment only)")
    revisions = {}
    for name, path in {"filecoinServices": source, "pdp": source / "lib/pdp",
                       "solady": source / "lib/solady", "erc8167": source / "lib/erc8167",
                       "payments": source / "lib/fws-payments", "sessionKeys": source / "lib/session-key-registry"}.items():
        revisions[name] = {"commit": command(["git", "-C", path, "rev-parse", "HEAD"]),
                           "describe": command(["git", "-C", path, "describe", "--always", "--dirty"])}
    chain_id = int(rpc(args.rpc_url, "eth_chainId", []), 16)
    env = os.environ.copy()
    env["ETH_PRIVATE_KEY"] = args.private_key
    owner = command(["cast", "wallet", "address", "--private-key", args.private_key])
    balance = int(rpc(args.rpc_url, "eth_getBalance", [owner, "latest"]), 16)
    if balance <= (0 if args.token_address else args.wrap_amount):
        raise RuntimeError("deployer needs native funds for deployment gas and the requested WETH deposit")
    output.parent.mkdir(parents=True, exist_ok=True)
    # A physical copy keeps upstream sources read-only, including assembler output,
    # Foundry broadcast files, and dependency-local compiler caches.
    shutil.copytree(source, build, ignore=shutil.ignore_patterns(".git", "out", "cache", "broadcast", "node_modules"))
    script = Path(__file__).resolve().parent / "contracts/DeployFWSS.s.sol"
    shutil.copy2(script, build / "script/DeployFWSS.s.sol")
    env.update({"DEPLOY_OWNER": owner, "DEPLOY_TOKEN": args.token_address or "0x" + "0" * 40,
                "DEPLOY_WRAP_AMOUNT": str(args.wrap_amount), "DEPLOY_FINALITY": str(args.challenge_finality),
                "DEPLOY_PERIOD": str(args.max_proving_period), "DEPLOY_WINDOW": str(args.challenge_window_size),
                "ETH_TIMEOUT": "300", "ETH_RPC_TIMEOUT": "300"})
    print(f"Building checked-out FWSS {revisions['filecoinServices']['commit']} in {build}", file=sys.stderr)
    print(command(["make", "erc8167"], cwd=build, env=env), file=sys.stderr)
    print(command(["forge", "build"], cwd=build, env=env), file=sys.stderr)
    command(["forge", "script", "script/DeployFWSS.s.sol:DeployFWSS", "--rpc-url", args.rpc_url,
             "--private-key", args.private_key, "--sender", owner, "--broadcast", "--skip-simulation"], cwd=build, env=env)
    finalize_deployment(args, source, output, build, revisions, chain_id, owner)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        print(f"deployment failed: {error}", file=sys.stderr)
        sys.exit(1)
