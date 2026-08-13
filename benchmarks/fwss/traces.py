#!/usr/bin/env python3
"""Filecoin gas-trace summaries and contract-call attribution."""

from __future__ import annotations

import base64
from collections import defaultdict
from typing import Any

from chain import quantity


def delegated_eth_address(address: str) -> str | None:
    if not address.startswith(("f410f", "t410f")):
        return None
    encoded = address[5:].upper()
    encoded += "=" * ((8 - len(encoded) % 8) % 8)
    try:
        payload = base64.b32decode(encoded)
    except ValueError:
        return None
    if len(payload) < 24:
        return None
    return "0x" + payload[:20].hex()


def summarize_trace(trace: dict[str, Any], labels: dict[str, str]) -> dict[str, Any]:
    charges_by_name: dict[str, dict[str, int]] = {}
    component_self: defaultdict[str, int] = defaultdict(int)
    flat_calls: list[dict[str, Any]] = []
    charged_milligas = 0

    normalized_labels = {address.lower(): label for address, label in labels.items()}
    actor_labels: dict[int, str] = {}

    def collect_actor_labels(call: dict[str, Any]) -> None:
        eth_address = delegated_eth_address(call.get("to", ""))
        invoked = call.get("invokedActorId")
        if eth_address is not None and invoked is not None:
            actor_labels[quantity(invoked)] = normalized_labels.get(
                eth_address.lower(), eth_address
            )
        for child in call.get("subcalls", []):
            collect_actor_labels(child)

    for top_level_call in trace.get("calls", []):
        collect_actor_labels(top_level_call)

    def sum_charges(charges: list[dict[str, Any]]) -> int:
        nonlocal charged_milligas
        total = 0
        for charge in charges:
            value = quantity(charge["totalMilligas"])
            total += value
            charged_milligas += value
            target = charges_by_name.setdefault(
                charge["name"],
                {
                    "count": 0,
                    "computeMilligas": 0,
                    "otherMilligas": 0,
                    "totalMilligas": 0,
                },
            )
            target["count"] += 1
            for field in ("computeMilligas", "otherMilligas", "totalMilligas"):
                target[field] += quantity(charge[field])
        return total

    overhead = sum_charges(trace.get("gasCharges", []))
    component_self["message-overhead"] += overhead

    def walk(call: dict[str, Any], depth: int, path: str) -> int:
        to = call.get("to", "")
        eth_address = delegated_eth_address(to)
        if eth_address is not None:
            component = normalized_labels.get(eth_address.lower(), eth_address)
        elif to.startswith(("f0", "t0")) and to[2:].isdigit():
            component = actor_labels.get(int(to[2:]), to)
        else:
            component = to or "unknown"
        self_milligas = sum_charges(call.get("gasCharges", []))
        component_self[str(component)] += self_milligas
        child_total = 0
        for index, child in enumerate(call.get("subcalls", [])):
            child_total += walk(child, depth + 1, f"{path}.{index}")
        inclusive = self_milligas + child_total
        flat_calls.append(
            {
                "path": path,
                "depth": depth,
                "to": call.get("to"),
                "ethereumAddress": eth_address,
                "component": component,
                "method": call.get("method"),
                "readOnly": call.get("readOnly"),
                "exitCode": call.get("exitCode"),
                "selfMilligas": self_milligas,
                "inclusiveMilligas": inclusive,
            }
        )
        return inclusive

    for index, call in enumerate(trace.get("calls", [])):
        walk(call, 0, str(index))

    gas_used = quantity(trace["gasUsed"])
    return {
        "gasUsed": gas_used,
        "chargedMilligas": charged_milligas,
        "roundingMilligas": gas_used * 1000 - charged_milligas,
        "chargesByName": dict(sorted(charges_by_name.items())),
        "componentSelfMilligas": dict(sorted(component_self.items())),
        "calls": sorted(
            flat_calls, key=lambda item: [int(part) for part in item["path"].split(".")]
        ),
    }
