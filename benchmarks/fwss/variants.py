#!/usr/bin/env python3
"""Build provenance, deployment, and proxy upgrades for benchmark variants."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from chain import Chain, command

IMPLEMENTATION_SLOT = (
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
)
INITIALIZABLE_SLOT = (
    "0xf0c57e16840df040f15088dc2f81fe391c3923bec73e23a9662efc9c229c6a00"
)


def digest_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def digest_json(value: Any) -> str:
    return digest_bytes(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    )


def repository_capture(repository: Path) -> dict[str, Any]:
    status = command(["git", "status", "--porcelain"], cwd=repository)
    untracked_solidity = [
        line[3:]
        for line in status.splitlines()
        if line.startswith("?? ") and line.endswith(".sol")
    ]
    if untracked_solidity:
        raise RuntimeError(
            f"untracked Solidity files in {repository}: {', '.join(untracked_solidity)}"
        )
    patch = command(["git", "diff", "HEAD", "--binary"], cwd=repository)
    return {
        "repository": str(repository),
        "head": command(["git", "rev-parse", "HEAD"], cwd=repository),
        "describe": command(
            ["git", "describe", "--tags", "--always", "--dirty"], cwd=repository
        ),
        "status": status.splitlines(),
        "patch": patch,
        "patchSha256": digest_bytes(patch.encode()),
    }


def runtime_hash(chain: Chain, address: str) -> str:
    code = chain.cast("code", "--rpc-url", chain.rpc_url, address)
    return digest_bytes(bytes.fromhex(code.removeprefix("0x")))


def implementation(chain: Chain, proxy: str) -> str:
    word = chain.cast("storage", "--rpc-url", chain.rpc_url, proxy, IMPLEMENTATION_SLOT)
    return "0x" + word[-40:]


def initialized_version(chain: Chain, proxy: str) -> int:
    word = chain.cast("storage", "--rpc-url", chain.rpc_url, proxy, INITIALIZABLE_SLOT)
    return int(word, 16)


def link_bytecode(
    bytecode: str, references: dict[str, Any], addresses: dict[str, str]
) -> str:
    linked = bytecode.removeprefix("0x")
    for source, libraries in references.items():
        for library, locations in libraries.items():
            key = f"{source}:{library}"
            address = addresses.get(key)
            if address is None:
                raise RuntimeError(f"no configured address for linked library {key}")
            replacement = address.removeprefix("0x").lower()
            if len(replacement) != 40:
                raise RuntimeError(f"invalid library address for {key}: {address}")
            for location in locations:
                start = int(location["start"]) * 2
                length = int(location["length"]) * 2
                if length != 40:
                    raise RuntimeError(
                        f"unsupported link length for {key}: {length // 2}"
                    )
                linked = linked[:start] + replacement + linked[start + length :]
    try:
        bytes.fromhex(linked)
    except ValueError as error:
        raise RuntimeError("creation bytecode remains unlinked") from error
    return "0x" + linked


class BuildManager:
    def __init__(self, chain: Chain, suite: dict[str, Any], node_repo: Path) -> None:
        self.chain = chain
        self.suite = suite
        self.node_repo = node_repo
        self.records: dict[str, dict[str, Any]] = {}
        self.artifacts: dict[str, dict[str, Any]] = {}

    def prepare(self, names: set[str]) -> None:
        for name in sorted(names):
            spec = self.suite["builds"][name]
            if spec["kind"] == "local":
                self._prepare_local(name, spec)
            else:
                self._prepare_imported(name, spec)

    def _prepare_imported(self, name: str, spec: dict[str, Any]) -> None:
        component = spec["component"]
        proxy = self.suite["contracts"][component]
        adopted = implementation(self.chain, proxy)
        version = self.chain.call(proxy, "VERSION()(string)").strip().strip('"')
        record = {
            "name": name,
            "component": component,
            "kind": "imported",
            "version": version,
            "implementation": adopted,
            "runtimeBytecodeSha256": runtime_hash(self.chain, adopted),
        }
        record["fingerprint"] = digest_json(record)
        self.records[name] = record

    def _prepare_local(self, name: str, spec: dict[str, Any]) -> None:
        repository = Path(spec["repository"]).expanduser()
        if not repository.is_absolute():
            repository = self.node_repo / repository
        repository = repository.resolve()
        command(["forge", "build"], cwd=repository)
        artifact_path = repository / spec["artifact"]
        artifact = json.loads(artifact_path.read_text())
        source = repository_capture(repository)
        sources = [source]
        pdp_submodule = repository / "lib/pdp"
        if spec["component"] == "fwss" and pdp_submodule.exists():
            sources.append(repository_capture(pdp_submodule))
        bytecode = artifact["bytecode"]["object"]
        artifact_identity = {
            "creationBytecodeSha256": digest_bytes(bytecode.encode()),
            "compiler": artifact.get("metadata", {}).get("compiler"),
        }
        identity = {
            "name": name,
            "component": spec["component"],
            "kind": "local",
            "sources": [
                {key: value for key, value in capture.items() if key != "patch"}
                for capture in sources
            ],
            "artifact": str(artifact_path),
            "artifactIdentity": artifact_identity,
            "upgradeCall": spec.get("upgradeCall", "0x"),
        }
        identity["fingerprint"] = digest_json(identity)
        identity["patches"] = [
            {
                "repository": capture["repository"],
                "content": capture["patch"],
                "sha256": capture["patchSha256"],
            }
            for capture in sources
            if capture["patch"]
        ]
        self.records[name] = identity
        self.artifacts[name] = artifact

    def public_record(self, name: str) -> dict[str, Any]:
        return self.records[name]

    def configure_variant(self, variant_name: str) -> dict[str, Any]:
        variant = self.suite["variants"][variant_name]
        upgrades: dict[str, Any] = {}
        for component in ("pdp", "fwss"):
            build_name = variant[component]
            if self.suite["builds"][build_name]["kind"] == "local":
                upgrades[component] = self._deploy_and_upgrade(build_name)
        return upgrades

    def _deploy_and_upgrade(self, build_name: str) -> dict[str, Any]:
        spec = self.suite["builds"][build_name]
        component = spec["component"]
        proxy = self.suite["contracts"][component]
        artifact = self.artifacts[build_name]
        creation = artifact["bytecode"]["object"]
        if component == "pdp":
            constructor = self.chain.abi_encode(
                "f(uint64,uint256)",
                str(initialized_version(self.chain, proxy) + 1),
                str(self.chain.call_uint(proxy, "getChallengeFinality()(uint256)")),
            )
        else:
            contracts = self.suite["contracts"]
            creation = link_bytecode(
                creation,
                artifact["bytecode"].get("linkReferences", {}),
                {
                    "src/lib/Rails.sol:Rails": contracts["railsLibrary"],
                    "src/lib/SignatureVerificationLib.sol:SignatureVerificationLib": contracts[
                        "signatureLibrary"
                    ],
                },
            )
            constructor = self.chain.abi_encode(
                "f(address,address,address,address,address,address,uint64)",
                contracts["pdp"],
                contracts["payments"],
                contracts["usdfc"],
                contracts["filBeamBeneficiary"],
                contracts["registry"],
                contracts["sessionKeys"],
                str(initialized_version(self.chain, proxy) + 1),
            )
        creation_code = creation + constructor.removeprefix("0x")
        deployed, deployment_receipt = self.chain.deploy(creation_code)
        after_epoch = self.chain.block_number() + 2
        announcement = self.chain.send(
            to=proxy,
            signature="announcePlannedUpgrade((address,uint96))",
            arguments=[f"({deployed},{after_epoch})"],
        )
        self.chain.mine_to(after_epoch)
        upgrade_call = spec.get("upgradeCall", "0x")
        calldata = (
            upgrade_call
            if upgrade_call.startswith("0x")
            else self.chain.calldata(upgrade_call)
        )
        upgrade = self.chain.send(
            to=proxy,
            signature="upgradeToAndCall(address,bytes)",
            arguments=[deployed, calldata],
        )
        adopted = implementation(self.chain, proxy)
        if adopted.lower() != deployed.lower():
            raise RuntimeError(f"{component} adopted {adopted}, expected {deployed}")
        return {
            "build": build_name,
            "buildFingerprint": self.records[build_name]["fingerprint"],
            "implementation": deployed,
            "runtimeBytecodeSha256": runtime_hash(self.chain, deployed),
            "deploymentReceipt": deployment_receipt,
            "announcementReceipt": announcement,
            "upgradeReceipt": upgrade,
        }
