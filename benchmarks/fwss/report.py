#!/usr/bin/env python3
"""Compare any collected FWSS benchmark variants by workload key."""

from __future__ import annotations

import argparse
import csv
import json
import sys
from pathlib import Path
from typing import Any


def load_results(
    root: Path, variants: list[str]
) -> dict[str, dict[str, dict[str, Any]]]:
    loaded: dict[str, dict[str, dict[str, Any]]] = {}
    for variant in variants:
        cases: dict[str, dict[str, Any]] = {}
        case_dir = root / "variants" / variant / "cases"
        if case_dir.exists():
            for path in sorted(case_dir.glob("*.json")):
                result = json.loads(path.read_text())
                cases[result["comparisonKey"]] = result
        loaded[variant] = cases
    return loaded


def load_boundaries(
    root: Path, variants: list[str]
) -> dict[str, dict[str, dict[str, Any]]]:
    loaded: dict[str, dict[str, dict[str, Any]]] = {}
    for variant in variants:
        items: dict[str, dict[str, Any]] = {}
        boundary_dir = root / "variants" / variant / "boundaries"
        if boundary_dir.exists():
            for path in sorted(boundary_dir.glob("*.json")):
                result = json.loads(path.read_text())
                key = "/".join(
                    str(value)
                    for value in (
                        result["operation"],
                        result.get("metadata"),
                        result.get("startingPieceCount"),
                        result["ceiling"],
                    )
                )
                items[key] = result
        loaded[variant] = items
    return loaded


def boundary_cell(result: dict[str, Any] | None) -> str:
    if result is None:
        return "not-run"
    if result["status"] == "exact":
        return str(result["maximumSuccessfulBatch"])
    if result["status"] == "lower-bound":
        return f">={result['successfulThrough']}"
    return f"prerequisite-limited (>={result['successfulThrough']})"


def print_boundary_table(
    loaded: dict[str, dict[str, dict[str, Any]]], baseline: str, variants: list[str]
) -> None:
    ordered = [baseline, *[variant for variant in variants if variant != baseline]]
    keys = sorted({key for items in loaded.values() for key in items})
    if not keys:
        return
    print("\nBatch boundaries")
    table = [["Search", *ordered]]
    for key in keys:
        table.append(
            [key, *[boundary_cell(loaded[variant].get(key)) for variant in ordered]]
        )
    widths = [
        max(len(str(row[index])) for row in table) for index in range(len(table[0]))
    ]
    for row_index, row in enumerate(table):
        print(
            "  ".join(
                str(value).ljust(widths[index]) for index, value in enumerate(row)
            )
        )
        if row_index == 0:
            print("  ".join("-" * width for width in widths))


def gas(result: dict[str, Any] | None) -> int | None:
    if result is None or result.get("status") != "success":
        return None
    return int(result["traceSummary"]["gasUsed"])


def status(result: dict[str, Any] | None) -> str:
    return "not-run" if result is None else str(result["status"])


def rows(
    loaded: dict[str, dict[str, dict[str, Any]]], baseline: str, variants: list[str]
) -> list[dict[str, Any]]:
    keys = sorted({key for cases in loaded.values() for key in cases})
    output = []
    for key in keys:
        baseline_result = loaded[baseline].get(key)
        baseline_gas = gas(baseline_result)
        for variant in variants:
            result = loaded[variant].get(key)
            variant_gas = gas(result)
            difference = (
                variant_gas - baseline_gas
                if variant_gas is not None and baseline_gas is not None
                else None
            )
            output.append(
                {
                    "comparisonKey": key,
                    "baseline": baseline,
                    "variant": variant,
                    "baselineStatus": status(baseline_result),
                    "variantStatus": status(result),
                    "baselineGasUsed": baseline_gas,
                    "variantGasUsed": variant_gas,
                    "gasDifference": difference,
                    "percentChange": (
                        difference * 100.0 / baseline_gas
                        if difference is not None and baseline_gas
                        else None
                    ),
                    "baselineComponents": (
                        baseline_result.get("traceSummary", {}).get(
                            "componentSelfMilligas", {}
                        )
                        if baseline_result
                        else {}
                    ),
                    "variantComponents": (
                        result.get("traceSummary", {}).get("componentSelfMilligas", {})
                        if result
                        else {}
                    ),
                }
            )
    return output


def render_cell(
    result: dict[str, Any] | None, baseline_gas: int | None, baseline: bool
) -> str:
    result_status = status(result)
    result_gas = gas(result)
    if result_gas is None:
        return result_status
    if baseline or baseline_gas is None:
        return f"{result_gas:,}"
    difference = result_gas - baseline_gas
    percent = difference * 100.0 / baseline_gas if baseline_gas else 0.0
    return f"{result_gas:,} ({percent:+.1f}%)"


def print_table(
    loaded: dict[str, dict[str, dict[str, Any]]], baseline: str, variants: list[str]
) -> None:
    ordered = [baseline, *[variant for variant in variants if variant != baseline]]
    keys = sorted({key for cases in loaded.values() for key in cases})
    table = [["Case", *ordered]]
    for key in keys:
        baseline_result = loaded[baseline].get(key)
        baseline_gas = gas(baseline_result)
        table.append(
            [
                key,
                *[
                    render_cell(
                        loaded[variant].get(key), baseline_gas, variant == baseline
                    )
                    for variant in ordered
                ],
            ]
        )
    widths = [
        max(len(str(row[index])) for row in table) for index in range(len(table[0]))
    ]
    for row_index, row in enumerate(table):
        print(
            "  ".join(
                str(value).ljust(widths[index]) for index, value in enumerate(row)
            )
        )
        if row_index == 0:
            print("  ".join("-" * width for width in widths))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--results",
        type=Path,
        default=Path(__file__).with_name("results") / "mainnet-v1.3.0",
    )
    parser.add_argument("--baseline", default="deployed")
    parser.add_argument("--variant", action="append", default=[])
    parser.add_argument("--operation")
    parser.add_argument("--format", choices=("table", "json", "csv"), default="table")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    available = sorted(
        path.name for path in (args.results / "variants").iterdir() if path.is_dir()
    )
    variants = args.variant or available
    if args.baseline not in variants:
        variants.insert(0, args.baseline)
    loaded = load_results(args.results, variants)
    boundaries = load_boundaries(args.results, variants)
    if args.operation:
        loaded = {
            variant: {
                key: result
                for key, result in cases.items()
                if result.get("operation") == args.operation
                or key.startswith(args.operation + "/")
            }
            for variant, cases in loaded.items()
        }
        boundaries = {
            variant: {
                key: result
                for key, result in items.items()
                if result.get("operation") == args.operation
            }
            for variant, items in boundaries.items()
        }
    comparison_variants = [variant for variant in variants if variant != args.baseline]
    data = rows(loaded, args.baseline, comparison_variants)
    if args.format == "json":
        print(
            json.dumps(
                {"comparisons": data, "boundaries": boundaries},
                indent=2,
                sort_keys=True,
            )
        )
    elif args.format == "csv":
        fieldnames = [
            "comparisonKey",
            "baseline",
            "variant",
            "baselineStatus",
            "variantStatus",
            "baselineGasUsed",
            "variantGasUsed",
            "gasDifference",
            "percentChange",
            "baselineComponents",
            "variantComponents",
        ]
        writer = csv.DictWriter(sys.stdout, fieldnames=fieldnames)
        writer.writeheader()
        for row in data:
            row = dict(row)
            row["baselineComponents"] = json.dumps(
                row["baselineComponents"], sort_keys=True
            )
            row["variantComponents"] = json.dumps(
                row["variantComponents"], sort_keys=True
            )
            writer.writerow(row)
    else:
        print_table(loaded, args.baseline, comparison_variants)
        print_boundary_table(boundaries, args.baseline, comparison_variants)


if __name__ == "__main__":
    main()
