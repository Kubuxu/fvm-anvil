#!/usr/bin/env python3
"""Upgrade an existing modular FWSS proxy on loopback fvm-anvil only.

The imported proxy, dispatcher, dependencies, and proving slots are preserved.
The only direct state patch is the explicitly recorded local owner takeover.
PRIVATE_KEY/ETH_PRIVATE_KEY may supply the signing key without a CLI argument.
Run with interval mining enabled; this helper never changes mining ownership.
"""
import argparse
from functools import cache
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import time
from urllib.parse import urlsplit

from deploy_fwss import command, rpc


CANDIDATE = "371e8b0a48ccf622288e0af0f02a4468d5c6a7be"
MAINNET_FWSS = "0x8408502033C418E1bbC97cE9ac48E5528F371A9f"
OWNABLE_SLOT = "0x9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300"
IMPLEMENTATION_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
ZERO_ADDRESS = "0x" + "00" * 20
ZERO_WORD = "0x" + "00" * 32
DEPENDENCIES = {
    "payments": "paymentsContractAddress()", "pdp": "pdpVerifierAddress()",
    "token": "usdfcTokenAddress()", "registry": "serviceProviderRegistry()",
    "sessionKeys": "sessionKeyRegistry()", "view": "viewContractAddress()",
    "filBeamBeneficiary": "filBeamBeneficiaryAddress()",
    "filBeamController": "filBeamControllerAddress()",
}


def export_source(repository, revision, destination, prefix=""):
    """Export committed inputs and recursively pinned submodules, never working-tree extras."""
    repository = repository.resolve()
    root = Path(command(["git", "-C", repository, "rev-parse", "--show-toplevel"])).resolve()
    if root != repository or command(["git", "-C", repository, "rev-parse", "HEAD"]) != revision:
        raise RuntimeError(f"source checkout does not match its pinned revision: {repository}")
    destination.mkdir(parents=True, exist_ok=True)
    tree = f"{revision}:{prefix}" if prefix else revision
    with subprocess.Popen(["git", "-C", str(repository), "archive", "--format=tar", tree],
                          stdout=subprocess.PIPE) as process:
        with tarfile.open(fileobj=process.stdout, mode="r|") as archive:
            archive.extractall(destination, filter="data")
        if process.wait() != 0:
            raise RuntimeError(f"failed to export pinned source: {repository}")
    entries = command(["git", "-C", repository, "ls-tree", "-rz", revision, "--", prefix or "."])
    for entry in entries.split("\0"):
        if not entry:
            continue
        metadata, path = entry.split("\t", 1)
        mode, _, commit = metadata.split()
        if mode == "160000":
            relative = Path(path).relative_to(prefix) if prefix else Path(path)
            export_source(repository / path, commit, destination / relative)


def local_url(url):
    parsed = urlsplit(url)
    if parsed.scheme not in ("http", "https") or parsed.username or parsed.password or parsed.fragment:
        raise ValueError("RPC URL must be HTTP(S), loopback, and contain no credentials or fragment")
    host = parsed.hostname or ""
    try:
        loopback = ipaddress.ip_address(host).is_loopback
    except ValueError:
        loopback = host.lower() == "localhost"
    if not loopback:
        raise ValueError("refusing non-loopback RPC URL")
    # Force validation of malformed ports before any subprocess or RPC.
    parsed.port


def address(value):
    if not re.fullmatch(r"0x[0-9a-fA-F]{40}", value):
        raise ValueError("expected a 20-byte hexadecimal address")
    return value.lower()


@cache
def selector(signature):
    return command(["cast", "sig", signature])


def word(value):
    return f"{int(value, 16) if isinstance(value, str) else value:064x}"


def call(url, target, signature, *words):
    return rpc(url, "eth_call", [{"to": target, "data": selector(signature) + "".join(words)}, "latest"])


def decode_address(value):
    if len(value) != 66 or int(value[2:26], 16):
        raise RuntimeError("invalid ABI address response")
    return address("0x" + value[-40:])


def selectors(url, target):
    data = bytes.fromhex(call(url, target, "selectors()")[2:])
    if len(data) < 64 or int.from_bytes(data[:32], "big") != 32:
        raise RuntimeError("unsupported monolithic FWSS: no ERC-8167 selectors() ABI")
    count = int.from_bytes(data[32:64], "big")
    if not count or len(data) != 64 + count * 32:
        raise RuntimeError("unsupported modular FWSS: malformed or empty selector list")
    result = ["0x" + data[i:i + 4].hex() for i in range(64, len(data), 32)]
    if len(result) != len(set(result)):
        raise RuntimeError("duplicate exported selectors")
    return result


def routes(url, target):
    return {item: decode_address(call(url, target, "implementation(bytes4)", item[2:].ljust(64, "0")))
            for item in selectors(url, target)}


def storage(url, target, slot):
    return rpc(url, "eth_getStorageAt", [target, slot, "latest"]).lower()


def code(url, target):
    value = rpc(url, "eth_getCode", [target, "latest"]).lower()
    if value in ("0x", "0x0"):
        raise RuntimeError(f"contract has no runtime bytecode: {target}")
    return value


def guard(url, chain_id=None, prepared=None):
    local_url(url)
    version = rpc(url, "web3_clientVersion", [])
    if "fvm-anvil" not in version.lower():
        raise RuntimeError("refusing mutations: RPC is not fvm-anvil")
    roots = rpc(url, "filecoin_getStateRoots", [])
    if not isinstance(roots, dict) or not roots.get("current") or "prepared" not in roots:
        raise RuntimeError("fvm-anvil must expose prepared/current state roots")
    chain = int(rpc(url, "eth_chainId", []), 16)
    if chain_id is not None and (chain != chain_id or roots["prepared"] != prepared):
        raise RuntimeError("RPC chain or prepared state changed during upgrade")
    return version, roots, chain


def write_json(path, value):
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def secret_command(args, key, cwd=None, env=None):
    """Never let a signing command echo a private key in captured errors."""
    result = subprocess.run([str(item) for item in args], cwd=cwd, env=env,
                            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    def redact(text):
        return text.replace(key, "[REDACTED]").replace(key.removeprefix("0x"), "[REDACTED]")
    if result.stderr:
        print(redact(result.stderr), file=sys.stderr, end="")
    if result.returncode:
        if result.stdout:
            print(redact(result.stdout), file=sys.stderr)
        raise RuntimeError(f"signing command failed ({result.returncode}): {args[0]}")
    return redact(result.stdout).strip()


def receipt(url, tx_hash, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = rpc(url, "eth_getTransactionReceipt", [tx_hash])
        if value is not None:
            if int(value["status"], 16) != 1:
                raise RuntimeError(f"transaction reverted: {tx_hash}")
            return value
        time.sleep(0.25)
    raise RuntimeError(f"receipt timed out: {tx_hash}; retain interval mining and inspect retained build evidence")


def send(args, owner, signature, *values):
    result = secret_command(["cast", "send", args.proxy, signature, *values, "--rpc-url", args.rpc_url,
                             "--private-key", args.private_key, "--from", owner, "--async"], args.private_key)
    if not re.fullmatch(r"0x[0-9a-fA-F]{64}", result):
        raise RuntimeError("cast send did not return a transaction hash")
    return receipt(args.rpc_url, result, args.timeout)


def dependencies(url, proxy):
    return {name: decode_address(call(url, proxy, signature)) for name, signature in DEPENDENCIES.items()}


def dataset_evidence(url, proxy, pdp, requested):
    """Sample existing records, not time-dependent status/proving-period views."""
    next_id = int(call(url, pdp, "getNextDataSetId()"), 16)
    ids = requested or sorted(set(range(1, min(next_id, 9))) |
                              set(range(max(1, next_id - 8), next_id)))
    views = {}
    for data_set in ids:
        encoded = word(data_set)
        info = call(url, proxy, "getDataSet(uint256)", encoded)
        if len(info) < 66 or int(info[2:66], 16) == 0:
            if requested:
                raise RuntimeError(f"requested FWSS dataset has no payment rail: {data_set}")
            continue
        views[str(data_set)] = {
            signature: call(url, proxy, signature, encoded)
            for signature in ("getDataSet(uint256)", "getDataSetPayerAndRailId(uint256)",
                              "getAllDataSetMetadata(uint256)", "provingActivationEpoch(uint256)",
                              "provingDeadline(uint256)", "provenThisPeriod(uint256)")
        }
    return {"pdpNextDataSetId": next_id, "sampledIds": ids, "views": views}


def validate_runtimes(url, candidate):
    for target, expected in candidate["runtimes"].items():
        if code(url, target) != expected.lower():
            raise RuntimeError(f"candidate runtime differs from exact build deployment: {target}")


def decode_manifest(value):
    # Foundry string returns are escaped string contents, as in deploy_fwss.py.
    if isinstance(value, dict):
        return value
    try:
        return json.loads(value)
    except json.JSONDecodeError:
        return json.loads(json.loads('"' + value + '"'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True, help="StorageTerms candidate service_contracts checkout")
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18545/rpc/v1")
    parser.add_argument("--private-key", default=os.environ.get("PRIVATE_KEY", os.environ.get("ETH_PRIVATE_KEY")))
    parser.add_argument("--proxy", default=MAINNET_FWSS, help="Existing modular proxy; override only for local rehearsal")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-directory", type=Path)
    parser.add_argument("--dataset-id", type=int, action="append", help="Existing dataset to prove continuity (repeatable)")
    parser.add_argument("--timeout", type=int, default=7200, help="Receipt/Foundry timeout in seconds for imported proving periods")
    args = parser.parse_args()
    if not args.private_key or not re.fullmatch(r"(?:0x)?[0-9a-fA-F]{64}", args.private_key):
        parser.error("a valid --private-key or PRIVATE_KEY/ETH_PRIVATE_KEY is required")
    if args.timeout <= 0 or any(item <= 0 for item in (args.dataset_id or [])):
        parser.error("timeout and dataset IDs must be positive")
    args.proxy = address(args.proxy)
    client, original_roots, chain_id = guard(args.rpc_url)
    source = args.source.resolve(strict=True)
    output = args.output.resolve()
    build = (args.build_directory or output.with_suffix(".build")).resolve()
    if build == source or source in build.parents or build in source.parents:
        parser.error("build directory must be outside the upstream source tree")
    if output == source or source in output.parents or output == build or build in output.parents:
        parser.error("manifest must be outside source and build directories")
    extern = Path(__file__).resolve().parents[2] / "extern"
    if any(path == extern or extern in path.parents for path in (build, output)):
        parser.error("build and output must be outside extern")
    if output.exists() or build.exists():
        parser.error("output and build directory must not exist (retain each upgrade's evidence)")

    revisions = {}
    for name, path in {"filecoinServices": source, "pdp": source / "lib/pdp",
                       "solady": source / "lib/solady", "erc8167": source / "lib/erc8167",
                       "payments": source / "lib/fws-payments", "sessionKeys": source / "lib/session-key-registry"}.items():
        revisions[name] = {"commit": command(["git", "-C", path, "rev-parse", "HEAD"]),
                           "describe": command(["git", "-C", path, "describe", "--always", "--dirty"])}
    if revisions["filecoinServices"]["commit"] != CANDIDATE or revisions["filecoinServices"]["describe"].endswith("-dirty"):
        raise RuntimeError(f"source must be the clean StorageTerms candidate {CANDIDATE}")
    ownable_source = source / "src/lib/FWSSOwnable.sol"
    ownable_text = ownable_source.read_text()
    ownership_source = source / "src/modules/OwnershipModule.sol"
    ownership_text = ownership_source.read_text()
    if (OWNABLE_SLOT not in ownable_text.lower() or
            "StorageSlot.getAddressSlot(OWNABLE_STORAGE_LOCATION).value" not in ownable_text or
            "return _owner();" not in ownership_text):
        raise RuntimeError("candidate Ownable source does not match the explicit takeover slot/getter")

    try:
        old_routes = routes(args.rpc_url, args.proxy)
    except (RuntimeError, ValueError) as error:
        raise RuntimeError("unsupported monolithic/incomplete FWSS: require existing ERC-8167 modular routes") from error
    for signature in ("implementation(bytes4)", "selectors()", "announceMigration(address,uint96)", "migrate(address)"):
        if old_routes.get(selector(signature), ZERO_ADDRESS) == ZERO_ADDRESS:
            raise RuntimeError(f"unsupported modular FWSS: missing route {signature}")
    old_dispatcher_word = storage(args.rpc_url, args.proxy, IMPLEMENTATION_SLOT)
    dispatcher = decode_address(old_dispatcher_word)
    dispatcher_runtime = code(args.rpc_url, dispatcher)
    proxy_runtime = code(args.rpc_url, args.proxy)
    for delegate in set(old_routes.values()):
        if delegate in (ZERO_ADDRESS, dispatcher, args.proxy):
            raise RuntimeError("invalid existing modular route")
        code(args.rpc_url, delegate)
    original_owner = decode_address(call(args.rpc_url, args.proxy, "owner()"))
    original_owner_word = storage(args.rpc_url, args.proxy, OWNABLE_SLOT)
    if original_owner_word != "0x" + word(original_owner):
        raise RuntimeError("Ownable storage slot does not match the public owner() getter; refusing takeover")
    original_dependencies = dependencies(args.rpc_url, args.proxy)
    if original_dependencies["view"] != args.proxy:
        raise RuntimeError("unsupported modular FWSS: view getter must already name the existing proxy")
    for name in ("payments", "pdp", "token", "registry", "sessionKeys", "view"):
        code(args.rpc_url, original_dependencies[name])
    proving_slots = {hex(slot): storage(args.rpc_url, args.proxy, hex(slot)) for slot in (0, 1)}
    period = int(proving_slots["0x0"], 16) & (2**64 - 1)
    window = int(proving_slots["0x1"], 16)
    if not 0 < window < period:
        raise RuntimeError("existing FWSS proving parameters are invalid")
    continuity = dataset_evidence(args.rpc_url, args.proxy, original_dependencies["pdp"], args.dataset_id)
    previous_plan = call(args.rpc_url, args.proxy, "nextUpgrade()")
    try:
        previous_terms = call(args.rpc_url, args.proxy, "getStorageTerms(bytes32)", word(0))
    except RuntimeError:
        previous_terms = None  # Expected for the pre-StorageTerms modular source.

    env = os.environ.copy()
    env.update({"ETH_PRIVATE_KEY": args.private_key, "UPGRADE_FWSS": args.proxy,
                "ETH_TIMEOUT": str(args.timeout), "ETH_RPC_TIMEOUT": "300"})
    owner = address(secret_command(["cast", "wallet", "address", "--private-key", args.private_key], args.private_key))
    if int(rpc(args.rpc_url, "eth_getBalance", [owner, "latest"]), 16) == 0:
        raise RuntimeError("local signing account needs native funds for candidate deployment and migration gas")
    output.parent.mkdir(parents=True, exist_ok=True)
    repository = Path(command(["git", "-C", source, "rev-parse", "--show-toplevel"])).resolve()
    export_source(repository, CANDIDATE, build, source.relative_to(repository).as_posix())
    shutil.copy2(Path(__file__).resolve().parent / "contracts/UpgradeFWSS.s.sol", build / "script/UpgradeFWSS.s.sol")
    before = {"clientVersion": client, "stateRoots": original_roots, "proxy": args.proxy,
              "dispatcher": dispatcher, "dispatcherSlot": old_dispatcher_word,
              "owner": original_owner, "ownerSlot": original_owner_word, "dependencies": original_dependencies,
              "provingStorage": proving_slots, "routes": old_routes, "dataSets": continuity,
              "nextUpgrade": previous_plan, "legacyStorageTerms": previous_terms,
              "ownerSource": {"slot": OWNABLE_SLOT, "file": str(ownable_source),
                  "sha256": hashlib.sha256(ownable_text.encode()).hexdigest(),
                  "getterFile": str(ownership_source),
                  "getterSha256": hashlib.sha256(ownership_text.encode()).hexdigest()},
              "sourceRevisions": revisions}
    write_json(build / "upgrade-before.json", before)
    print(f"Building StorageTerms candidate {CANDIDATE} in {build}", file=sys.stderr)
    # Build only in the physical copy; assembler, link artifacts and caches cannot touch extern.
    print(command(["make", "erc8167"], cwd=build, env=env), file=sys.stderr)
    print(command(["forge", "build"], cwd=build, env=env), file=sys.stderr)
    guard(args.rpc_url, chain_id, original_roots["prepared"])
    secret_command(["forge", "script", "script/UpgradeFWSS.s.sol:UpgradeFWSS", "--rpc-url", args.rpc_url,
                    "--private-key", args.private_key, "--sender", owner, "--broadcast", "--skip-simulation"],
                   args.private_key, cwd=build, env=env)
    broadcast_path = build / "broadcast/UpgradeFWSS.s.sol" / str(chain_id) / "run-latest.json"
    broadcast = json.loads(broadcast_path.read_text())
    txs = broadcast["transactions"]
    deployment_receipts = broadcast["receipts"]
    if not txs or len(txs) != len(deployment_receipts):
        raise RuntimeError("candidate deployment is missing receipts")
    for item in deployment_receipts:
        confirmed = receipt(args.rpc_url, item["transactionHash"], args.timeout)
        if int(confirmed["status"], 16) != 1:
            raise RuntimeError("candidate deployment failed")
    candidate = decode_manifest(broadcast["returns"]["manifest"]["value"])
    candidate["routes"] = {key.lower(): address(value) for key, value in candidate["routes"].items()}
    write_json(build / "upgrade-candidate.json", candidate)
    validate_runtimes(args.rpc_url, candidate)
    if decode_address(call(args.rpc_url, candidate["migration"], "migration()")) != address(candidate["rawMigration"]):
        raise RuntimeError("StorageTerms wrapper does not wrap the candidate route migration")

    guard(args.rpc_url, chain_id, original_roots["prepared"])
    if (storage(args.rpc_url, args.proxy, OWNABLE_SLOT) != original_owner_word or
            routes(args.rpc_url, args.proxy) != old_routes or
            storage(args.rpc_url, args.proxy, IMPLEMENTATION_SLOT) != old_dispatcher_word):
        raise RuntimeError("proxy changed during candidate deployment; refusing owner takeover")
    new_owner_word = "0x" + word(owner)
    rpc(args.rpc_url, "anvil_setStorageAt", [args.proxy, OWNABLE_SLOT, new_owner_word])
    if (storage(args.rpc_url, args.proxy, OWNABLE_SLOT) != new_owner_word or
            decode_address(call(args.rpc_url, args.proxy, "owner()")) != owner):
        raise RuntimeError("local owner takeover failed")
    takeover = {"slot": OWNABLE_SLOT, "before": original_owner_word, "after": new_owner_word,
                "ownerBefore": original_owner, "ownerAfter": owner,
                "stateRootsAfter": rpc(args.rpc_url, "filecoin_getStateRoots", [])}
    write_json(build / "upgrade-owner-takeover.json", takeover)
    announced = send(args, owner, "announceMigration(address,uint96)", candidate["migration"], "1")
    write_json(build / "upgrade-announcement-receipt.json", announced)
    plan = call(args.rpc_url, args.proxy, "nextUpgrade()")
    if len(plan) != 130 or decode_address(plan[:66]) != address(candidate["migration"]):
        raise RuntimeError("migration announcement did not pin the StorageTerms wrapper")
    after_epoch = int(plan[66:], 16)
    current_epoch = int(rpc(args.rpc_url, "eth_blockNumber", []), 16)
    mined = max(0, after_epoch - current_epoch)
    if mined:
        guard(args.rpc_url, chain_id, original_roots["prepared"])
        rpc(args.rpc_url, "anvil_mine", [hex(mined)])
    guard(args.rpc_url, chain_id, original_roots["prepared"])
    migrated = send(args, owner, "migrate(address)", candidate["migration"])
    write_json(build / "upgrade-migration-receipt.json", migrated)

    expected_routes = candidate["routes"]
    if routes(args.rpc_url, args.proxy) != expected_routes:
        raise RuntimeError("full selector table does not match candidate routes")
    for removed in old_routes.keys() - expected_routes.keys():
        if decode_address(call(args.rpc_url, args.proxy, "implementation(bytes4)", removed[2:].ljust(64, "0"))) != ZERO_ADDRESS:
            raise RuntimeError(f"obsolete selector still routes: {removed}")
    validate_runtimes(args.rpc_url, candidate)
    if (storage(args.rpc_url, args.proxy, IMPLEMENTATION_SLOT) != old_dispatcher_word or
            code(args.rpc_url, dispatcher) != dispatcher_runtime or code(args.rpc_url, args.proxy) != proxy_runtime):
        raise RuntimeError("upgrade changed the imported proxy or dispatcher")
    if dependencies(args.rpc_url, args.proxy) != original_dependencies:
        raise RuntimeError("upgrade changed a dependency/beneficiary/controller/view getter")
    if decode_address(call(args.rpc_url, args.proxy, "owner()")) != owner:
        raise RuntimeError("upgrade changed the local owner")
    for slot, value in proving_slots.items():
        if storage(args.rpc_url, args.proxy, slot) != value:
            raise RuntimeError("upgrade changed stored proving parameters")
    if call(args.rpc_url, args.proxy, "nextUpgrade()") != "0x" + word(0) + word(0):
        raise RuntimeError("migration plan was not cleared")
    terms = call(args.rpc_url, args.proxy, "getStorageTerms(bytes32)", word(0))
    expected_terms = ("0x" + word(original_dependencies["token"]) + word(18) + word(0) +
                      word(2_500_000_000_000_000_000) + word(1))
    if terms.lower() != expected_terms:
        raise RuntimeError("legacy StorageTerms zero must be imported token / 18 decimals / zero salt / 2.5e18 / enabled")
    for data_set, views in continuity["views"].items():
        for signature, expected in views.items():
            if call(args.rpc_url, args.proxy, signature, word(int(data_set))) != expected:
                raise RuntimeError(f"dataset continuity changed: {data_set} {signature}")
    contracts = {key: original_dependencies[key] for key in ("pdp", "registry", "payments", "sessionKeys", "token", "view")}
    contracts.update({"fwss": args.proxy, "fwssDispatcher": dispatcher,
                      "fwssMigration": candidate["migration"], "fwssRawMigration": candidate["rawMigration"]})
    libraries = {}
    for entry in broadcast.get("libraries", []):
        library_source, library_name, library_address = entry.rsplit(":", 2)
        libraries[f"{library_source}:{library_name}"] = library_address
    manifest = {"schemaVersion": 1, "chainId": chain_id, "rpcUrl": args.rpc_url, "owner": owner,
                "sourceKind": "imported-prepared" if original_roots["prepared"] else "fresh-rehearsal",
                "sourceRevisions": revisions, "sourceDirectory": str(source), "buildDirectory": str(build),
                "artifactDirectory": str(build / "out"), "broadcastFile": str(broadcast_path),
                "contracts": contracts, "libraries": libraries,
                "parameters": {"maxProvingPeriod": period, "challengeWindowSize": window,
                    "filBeamBeneficiary": original_dependencies["filBeamBeneficiary"],
                    "filBeamController": original_dependencies["filBeamController"]},
                "curioEnv": {"CURIO_DEVNET_PDP_VERIFIER_ADDRESS": contracts["pdp"],
                    "CURIO_DEVNET_FWSS_ADDRESS": args.proxy,
                    "CURIO_DEVNET_SERVICE_REGISTRY_ADDRESS": contracts["registry"]},
                "upgrade": {"beforeFile": str(build / "upgrade-before.json"),
                    "candidateFile": str(build / "upgrade-candidate.json"), "originalStateRoots": original_roots,
                    "finalStateRoots": rpc(args.rpc_url, "filecoin_getStateRoots", []), "ownerTakeover": takeover,
                    "announcementReceipt": announced, "migrationReceipt": migrated,
                    "afterEpoch": after_epoch, "explicitlyMinedEpochs": mined,
                    "routes": expected_routes, "removedSelectors": sorted(old_routes.keys() - expected_routes.keys()),
                    "runtimeSha256": {target: hashlib.sha256(bytes.fromhex(runtime[2:])).hexdigest()
                        for target, runtime in candidate["runtimes"].items()},
                    "legacyStorageTerms": {"id": ZERO_WORD, "token": contracts["token"], "tokenDecimals": 18,
                        "salt": ZERO_WORD, "pricePerTiBPerMonth": "2500000000000000000", "enabled": True},
                    "continuityDataSetIds": list(continuity["views"]), "provingStorageUnchanged": proving_slots,
                    "dispatcherUnchanged": True, "planCleared": True},
                "receipts": deployment_receipts + [announced, migrated]}
    write_json(output, manifest)
    print(json.dumps(manifest))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        message = str(error)
        for key in (os.environ.get("PRIVATE_KEY"), os.environ.get("ETH_PRIVATE_KEY")):
            if key:
                message = message.replace(key, "[REDACTED]").replace(key.removeprefix("0x"), "[REDACTED]")
        print(f"upgrade failed: {message}", file=sys.stderr)
        sys.exit(1)
