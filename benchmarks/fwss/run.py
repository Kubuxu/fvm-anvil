#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["eth-account>=0.13,<0.14"]
# ///
"""Run resumable FWSS lifecycle gas benchmarks against imported state."""

from __future__ import annotations

import argparse
import copy
import fnmatch
import hashlib
import json
import os
import re
import sys
from pathlib import Path
from typing import Any

from chain import Chain, command
from fixtures import Fixtures
from operations import (
    SetupUnreachable,
    capped_batch_size,
    comparison_key,
    fixed_cases,
    measure_prepared,
    prepare_case,
)
from variants import BuildManager, digest_json, implementation

DEFAULT_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
RESULT_SCHEMA = 1


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def safe_name(value: str) -> str:
    readable = re.sub(r"[^A-Za-z0-9_.-]+", "__", value).strip("_")
    return f"{readable[:160]}__{hashlib.sha256(value.encode()).hexdigest()[:12]}"


class Runner:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root = Path(__file__).resolve().parents[2]
        suite_path = args.suite.resolve()
        self.suite = json.loads(suite_path.read_text())
        self.results = (self.root / self.suite["results"]).resolve()
        self.archive = (self.root / self.suite["archive"]).resolve()
        self.chain = Chain(args.rpc_url, args.private_key, int(self.suite["gasLimit"]))
        self.fixtures = Fixtures(self.chain, self.suite)
        runner_digest = hashlib.sha256()
        for name in (
            "run.py",
            "chain.py",
            "variants.py",
            "fixtures.py",
            "operations.py",
            "traces.py",
        ):
            runner_digest.update(name.encode())
            runner_digest.update(Path(__file__).with_name(name).read_bytes())
        self.runner_fingerprint = runner_digest.hexdigest()
        self.node_revision = command(["git", "rev-parse", "HEAD"], cwd=self.root)
        self.node_describe = command(
            ["git", "describe", "--tags", "--always", "--dirty"], cwd=self.root
        )
        self.tools = {
            "cast": command(["cast", "--version"]).splitlines()[0],
            "forge": command(["forge", "--version"]).splitlines()[0],
        }
        self.manifest = self._manifest()
        self.manager = BuildManager(self.chain, self.suite, self.root)
        self.base_snapshot: str | None = None

    def _manifest(self) -> dict[str, Any]:
        path = self.results / "manifest.json"
        archive_stat = self.archive.stat()
        if path.exists():
            manifest = json.loads(path.read_text())
            if manifest["archive"]["size"] != archive_stat.st_size:
                raise RuntimeError(
                    "prepared archive size differs from existing result manifest"
                )
            return manifest
        manifest = {
            "schemaVersion": RESULT_SCHEMA,
            "archive": {
                "path": str(self.archive),
                "size": archive_stat.st_size,
                "sha256": sha256_file(self.archive),
            },
            "importedBlock": self.chain.block_number(),
            "importedRoots": self.chain.rpc("filecoin_getStateRoots"),
            "contracts": self.suite["contracts"],
        }
        atomic_json(path, manifest)
        return manifest

    def prepare_builds(self, variants: list[str]) -> None:
        names = {
            self.suite["variants"][variant][component]
            for variant in variants
            for component in ("pdp", "fwss")
        }
        self.manager.prepare(names)
        for name in sorted(names):
            record = copy.deepcopy(self.manager.public_record(name))
            patches = record.pop("patches", [])
            fingerprint = record["fingerprint"]
            atomic_json(self.results / "builds" / f"{fingerprint}.json", record)
            for index, patch in enumerate(patches):
                path = self.results / "builds" / f"{fingerprint}-{index}.patch"
                path.parent.mkdir(parents=True, exist_ok=True)
                if not path.exists():
                    path.write_text(patch["content"])

    def initialize_chain(self) -> None:
        roots = self.chain.rpc("filecoin_getStateRoots")
        if roots.get("current") != self.manifest["importedRoots"].get("current"):
            raise RuntimeError(
                "node state differs from the imported benchmark archive; restart fvm-anvil"
            )
        for component, expected in (("pdp", "3.4.0"), ("fwss", "1.3.0")):
            proxy = self.suite["contracts"][component]
            version = self.chain.call(proxy, "VERSION()(string)").strip().strip('"')
            if version != expected:
                raise RuntimeError(
                    f"imported {component} version is {version}, expected {expected}"
                )
        setup = self.fixtures.patch_and_fund()
        self.base_snapshot = self.chain.snapshot()
        atomic_json(self.results / "setup.json", setup)

    def restore_base(self) -> None:
        if self.base_snapshot is None:
            raise RuntimeError("benchmark base snapshot is not initialized")
        self.chain.revert(self.base_snapshot)
        self.base_snapshot = self.chain.snapshot()

    def execution_plan(self, variant_name: str, case: dict[str, Any]) -> dict[str, Any]:
        variant = self.suite["variants"][variant_name]
        build_fingerprints = {
            component: self.manager.public_record(variant[component])["fingerprint"]
            for component in ("pdp", "fwss")
        }
        return {
            "resultSchema": RESULT_SCHEMA,
            "archiveSha256": self.manifest["archive"]["sha256"],
            "nodeRevision": self.node_revision,
            "nodeDescribe": self.node_describe,
            "tools": self.tools,
            "variant": variant,
            "buildFingerprints": build_fingerprints,
            "contracts": self.suite["contracts"],
            "funding": self.suite["funding"],
            "case": case,
            "transaction": {
                "gasLimit": self.chain.gas_limit,
                "maxFeePerGas": self.chain.max_fee_per_gas,
                "maxPriorityFeePerGas": self.chain.priority_fee_per_gas,
            },
            "runnerFingerprint": self.runner_fingerprint,
        }

    def result_path(self, variant_name: str, key: str) -> Path:
        return (
            self.results
            / "variants"
            / variant_name
            / "cases"
            / f"{safe_name(key)}.json"
        )

    def _write_variant(
        self, variant_name: str, upgrades: dict[str, Any] | None = None
    ) -> None:
        variant = self.suite["variants"][variant_name]
        value = {
            "name": variant_name,
            "configuration": variant,
            "builds": {
                component: {
                    "name": variant[component],
                    "fingerprint": self.manager.public_record(variant[component])[
                        "fingerprint"
                    ],
                }
                for component in ("pdp", "fwss")
            },
        }
        if upgrades is not None:
            value["lastUpgrade"] = upgrades
        atomic_json(self.results / "variants" / variant_name / "variant.json", value)

    def _labels(self, upgrades: dict[str, Any]) -> dict[str, str]:
        contracts = self.suite["contracts"]
        labels = {
            contracts["pdp"]: "PDP proxy",
            contracts["fwss"]: "FWSS proxy",
            contracts["payments"]: "FilecoinPay",
            contracts["usdfc"]: "USDFC",
            contracts["registry"]: "Provider registry",
            contracts["sessionKeys"]: "Session key registry",
            contracts["railsLibrary"]: "Rails library",
            contracts["signatureLibrary"]: "Signature library",
        }
        for component in ("pdp", "fwss"):
            labels[implementation(self.chain, contracts[component])] = (
                f"{component.upper()} implementation"
            )
        for component, upgrade in upgrades.items():
            labels[upgrade["implementation"]] = f"{component.upper()} implementation"
        return labels

    def run_one(
        self, variant_name: str, case: dict[str, Any], *, force: bool = False
    ) -> dict[str, Any]:
        key = comparison_key(case)
        plan = self.execution_plan(variant_name, case)
        measurement_id = digest_json(plan)
        path = self.result_path(variant_name, key)
        if path.exists() and not force:
            existing = json.loads(path.read_text())
            if existing.get("measurementId") == measurement_id:
                print(f"skip {variant_name}: {key}", file=sys.stderr)
                return existing

        variant = self.suite["variants"][variant_name]
        compact_empty_metadata = bool(variant.get("compactEmptyMetadata", False))
        self.restore_base()
        self.fixtures.begin_case(key)
        if (
            variant["fixtureCreation"] == "before-upgrades"
            and case["operation"] == "createDataSet"
        ):
            result = {
                "schemaVersion": RESULT_SCHEMA,
                "comparisonKey": key,
                "measurementId": measurement_id,
                "variant": variant_name,
                "case": case,
                "status": "not-applicable",
                "reason": "the measured creation cannot predate the upgrade",
            }
            atomic_json(path, result)
            return result

        try:
            if variant["fixtureCreation"] == "before-upgrades":
                prepared = prepare_case(
                    self.fixtures,
                    case,
                    compact_empty_metadata=compact_empty_metadata,
                )
                upgrades = self.manager.configure_variant(variant_name)
            else:
                upgrades = self.manager.configure_variant(variant_name)
                prepared = prepare_case(
                    self.fixtures,
                    case,
                    compact_empty_metadata=compact_empty_metadata,
                    compact_setup_metadata=compact_empty_metadata,
                )
        except SetupUnreachable as error:
            result = {
                "schemaVersion": RESULT_SCHEMA,
                "comparisonKey": key,
                "measurementId": measurement_id,
                "variant": variant_name,
                "case": case,
                "status": "setup-unreachable",
                "reason": str(error),
            }
            atomic_json(path, result)
            return result

        self._write_variant(variant_name, upgrades)
        labels = self._labels(upgrades)
        print(f"run {variant_name}: {key}", file=sys.stderr, flush=True)
        measured = measure_prepared(self.chain, prepared, case, labels)
        result = {
            "schemaVersion": RESULT_SCHEMA,
            "comparisonKey": key,
            "measurementId": measurement_id,
            "variant": variant_name,
            "case": case,
            "buildFingerprints": plan["buildFingerprints"],
            **measured,
        }
        atomic_json(path, result)
        return result

    def run_fixed(self, variants: list[str], patterns: list[str], force: bool) -> None:
        cases = fixed_cases(self.suite)
        if patterns:
            cases = [
                case
                for case in cases
                if any(
                    fnmatch.fnmatch(comparison_key(case), pattern)
                    for pattern in patterns
                )
            ]
        if not cases:
            raise RuntimeError("no benchmark cases matched")
        self.prepare_builds(variants)
        self.initialize_chain()
        for variant in variants:
            self._write_variant(variant)
            for case in cases:
                self.run_one(variant, case, force=force)

    def max_batch(
        self,
        variant: str,
        operation: str,
        metadata: str,
        starting_count: int,
        ceiling: int,
        force: bool,
    ) -> dict[str, Any]:
        ceiling = capped_batch_size(operation, ceiling)
        self.prepare_builds([variant])
        self.initialize_chain()

        def make_case(size: int) -> dict[str, Any]:
            if operation == "addPieces":
                parameters = {
                    "batchSize": size,
                    "metadata": metadata,
                    "startingPieceCount": starting_count,
                }
            elif operation == "schedulePieceDeletions":
                parameters = {"batchSize": size}
            elif operation == "nextProvingPeriod":
                parameters = {"removalCount": size, "metadata": metadata}
            elif operation == "cleanupPieces":
                parameters = {"batchSize": size}
            else:
                raise ValueError(f"unsupported maximum operation {operation}")
            return {"operation": operation, "parameters": parameters}

        probes: dict[int, dict[str, Any]] = {}

        def probe(size: int) -> dict[str, Any]:
            if size not in probes:
                probes[size] = self.run_one(variant, make_case(size), force=force)
            return probes[size]

        lower = 0
        upper: int | None = None
        size = 1
        prerequisite: dict[str, Any] | None = None
        while True:
            size = min(size, ceiling)
            result = probe(size)
            if result["status"] == "success":
                lower = size
            elif result["status"] == "setup-unreachable":
                prerequisite = {"size": size, "reason": result.get("reason")}
                break
            else:
                upper = size
                break
            if size == ceiling:
                break
            size = min(size * 2, ceiling)

        if upper is not None and lower > 0:
            while upper - lower > 1:
                midpoint = (lower + upper) // 2
                result = probe(midpoint)
                if result["status"] == "success":
                    lower = midpoint
                elif result["status"] == "setup-unreachable":
                    prerequisite = {"size": midpoint, "reason": result.get("reason")}
                    upper = None
                    break
                else:
                    upper = midpoint

        if prerequisite is not None:
            status = "prerequisite-limited"
        elif upper is None:
            status = "lower-bound"
        else:
            status = "exact"
        summary = {
            "schemaVersion": RESULT_SCHEMA,
            "variant": variant,
            "operation": operation,
            "metadata": metadata if operation == "addPieces" else None,
            "startingPieceCount": starting_count if operation == "addPieces" else None,
            "ceiling": ceiling,
            "status": status,
            "maximumSuccessfulBatch": lower if status == "exact" else None,
            "successfulThrough": lower,
            "firstFailingBatch": upper,
            "prerequisite": prerequisite,
            "probes": [
                {
                    "size": size,
                    "status": result["status"],
                    "comparisonKey": result["comparisonKey"],
                }
                for size, result in sorted(probes.items())
            ],
        }
        key = f"{operation}/{metadata}/{starting_count}/{ceiling}"
        atomic_json(
            self.results
            / "variants"
            / variant
            / "boundaries"
            / f"{safe_name(key)}.json",
            summary,
        )
        print(json.dumps(summary, indent=2))
        return summary


def selected_variants(suite: dict[str, Any], requested: list[str]) -> list[str]:
    variants = requested or list(suite["variants"])
    unknown = [variant for variant in variants if variant not in suite["variants"]]
    if unknown:
        raise RuntimeError(f"unknown variants: {', '.join(unknown)}")
    return variants


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--suite", type=Path, default=Path(__file__).with_name("suite.json")
    )
    parser.add_argument("--rpc-url", default="http://127.0.0.1:18546")
    parser.add_argument("--private-key", default=DEFAULT_KEY)
    subparsers = parser.add_subparsers(dest="command", required=True)

    run_parser = subparsers.add_parser("run")
    run_parser.add_argument("--variant", action="append", default=[])
    run_parser.add_argument("--case", action="append", default=[])
    run_parser.add_argument("--force", action="store_true")

    max_parser = subparsers.add_parser("max-batch")
    max_parser.add_argument("--variant", required=True)
    max_parser.add_argument(
        "--operation",
        required=True,
        choices=(
            "addPieces",
            "schedulePieceDeletions",
            "nextProvingPeriod",
            "cleanupPieces",
        ),
    )
    max_parser.add_argument("--metadata", choices=("none", "one"), default="one")
    max_parser.add_argument("--starting-piece-count", type=int, default=0)
    max_parser.add_argument("--ceiling", type=int)
    max_parser.add_argument("--force", action="store_true")

    list_parser = subparsers.add_parser("list")
    list_parser.add_argument("--variant", action="append", default=[])
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.command == "list":
        suite = json.loads(args.suite.resolve().read_text())
        variants = selected_variants(suite, args.variant)
        for variant in variants:
            for case in fixed_cases(suite):
                print(f"{variant}\t{comparison_key(case)}")
        return

    runner = Runner(args)
    variants = selected_variants(
        runner.suite, [args.variant] if args.command == "max-batch" else args.variant
    )
    if args.command == "run":
        runner.run_fixed(variants, args.case, args.force)
        return
    ceiling = args.ceiling or int(runner.suite["cases"]["maximumCeiling"])
    if ceiling <= 0 or args.starting_piece_count < 0:
        raise RuntimeError(
            "batch ceiling and starting piece count must be non-negative"
        )
    runner.max_batch(
        args.variant,
        args.operation,
        args.metadata,
        args.starting_piece_count,
        ceiling,
        args.force,
    )


if __name__ == "__main__":
    main()
