#!/usr/bin/env python3
"""Render the FWSS session-key gas benchmark results as a standalone HTML report.

Reads the harness output (results/fwss-gas.json) and, when present, the
foc-observer mainnet baseline (observed/mainnet-gas-30d.json). Pure standard
library; the HTML is self-contained so it can be shared or pasted into Notion.
"""

from __future__ import annotations

import argparse
import html
import json
import statistics
from datetime import datetime, timezone
from pathlib import Path

AUTHORIZER_GAS_LIMIT = 150_000_000
# Pricing-doc basis (30d to 2026-07-14) and the observer-refreshed value are both shown.
DOC_GAS_PRICE_ATTO = 1_080_000
FEE_MODEL_BASE = 175_000_000
FEE_MODEL_PER_PIECE = 120_000_000


def usd(gas: float, atto_per_gas: float) -> str:
    value = gas * atto_per_gas * 1e-18
    return f"${value:,.6f}"


def fmt_gas(gas: float) -> str:
    if gas >= 1e9:
        return f"{gas / 1e9:,.2f}B"
    if gas >= 1e6:
        return f"{gas / 1e6:,.1f}M"
    return f"{gas:,.0f}"


def linear_fit(points: list[tuple[int, int]]) -> tuple[float, float]:
    """Least-squares (base, slope) for gas = base + slope * N."""
    n = len(points)
    sx = sum(p[0] for p in points)
    sy = sum(p[1] for p in points)
    sxx = sum(p[0] * p[0] for p in points)
    sxy = sum(p[0] * p[1] for p in points)
    slope = (n * sxy - sx * sy) / (n * sxx - sx * sx)
    base = (sy - slope * sx) / n
    return base, slope


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    base_dir = Path(__file__).resolve().parent
    parser.add_argument("--results", type=Path, default=base_dir / "results" / "fwss-gas.json")
    parser.add_argument("--observed", type=Path, default=base_dir / "observed" / "mainnet-gas-30d.json")
    parser.add_argument("--output", type=Path, default=base_dir / "results" / "fwss-gas-report.html")
    args = parser.parse_args()

    results = json.loads(args.results.read_text())
    observed = json.loads(args.observed.read_text()) if args.observed.exists() else None
    config = results["configuration"]

    observed_price = None
    if observed and observed.get("effectiveGasPrice"):
        observed_price = observed["effectiveGasPrice"].get("medianEffectiveGasPrice")

    def price_cells(gas: float) -> str:
        cells = f"<td>{usd(gas, DOC_GAS_PRICE_ATTO)}</td>"
        if observed_price:
            cells += f"<td>{usd(gas, observed_price)}</td>"
        return cells

    price_headers = "<th>USD @ 1.08M atto/gas (doc basis)</th>"
    if observed_price:
        price_headers += f"<th>USD @ {observed_price / 1e6:.2f}M atto/gas (observed median)</th>"

    # ---------------------------------------------------------------- login

    login_rows = []
    for label, key in (("login, 2 permissions", "twoPermissions"), ("login, 1 permission", "onePermission")):
        entry = results["login"][key]
        login_rows.append(
            f"<tr><td>{label}</td><td>{fmt_gas(entry['gasUsed'])}</td>{price_cells(entry['gasUsed'])}</tr>"
        )

    # --------------------------------------------------------- createDataSet

    create = {entry["variant"]: entry for entry in results["createDataSet"]}
    create_delta = create["session"]["gasUsed"] - create["payer"]["gasUsed"]
    create_skr = create["session"]["skrSubcallMilligas"] / 1000

    # ------------------------------------------------------------ addPieces

    by_key: dict[tuple[str, int], dict[str, dict]] = {}
    for entry in results["addPieces"]:
        by_key.setdefault((entry["datasetState"], entry["batchSize"]), {})[entry["variant"]] = entry

    add_rows = []
    deltas = []
    skr_subcalls = []
    for (state, batch), pair in sorted(by_key.items()):
        payer, session = pair["payer"], pair["session"]
        delta = session["gasUsed"] - payer["gasUsed"]
        skr_gas = session["skrSubcallMilligas"] / 1000
        deltas.append(delta)
        skr_subcalls.append(skr_gas)
        add_rows.append(
            f"<tr><td>{state}</td><td>{batch}</td>"
            f"<td>{fmt_gas(payer['gasUsed'])}</td><td>{fmt_gas(session['gasUsed'])}</td>"
            f"<td>{fmt_gas(delta)}</td><td>{fmt_gas(skr_gas)}</td>"
            f"<td>{delta / AUTHORIZER_GAS_LIMIT * 100:.3f}%</td></tr>"
        )

    median_delta = statistics.median(deltas)
    median_skr = statistics.median(skr_subcalls)
    max_delta = max(deltas + [create_delta])

    # ------------------------------------------------------------- G(N) fit

    fits = []
    for state in ("fresh", "populated"):
        points = [
            (batch, pair["payer"]["gasUsed"])
            for (entry_state, batch), pair in sorted(by_key.items())
            if entry_state == state
        ]
        if len(points) >= 2:
            base, slope = linear_fit(points)
            fits.append((state, base, slope))
    fit_rows = [
        f"<tr><td>{state}</td><td>{fmt_gas(base)}</td><td>{fmt_gas(slope)}</td>"
        f"<td>{fmt_gas(FEE_MODEL_BASE)}</td><td>{fmt_gas(FEE_MODEL_PER_PIECE)}</td></tr>"
        for state, base, slope in fits
    ]

    # ------------------------------------------------------------- observed

    observed_section = ""
    if observed:
        rows = "".join(
            f"<tr><td>{bucket['bucket']}</td><td>{bucket['txCount']}</td>"
            f"<td>{fmt_gas(bucket['medianGas']) if bucket['medianGas'] else '—'}</td>"
            f"<td>{fmt_gas(bucket['p10Gas']) if bucket.get('p10Gas') else '—'}</td>"
            f"<td>{fmt_gas(bucket['p90Gas']) if bucket.get('p90Gas') else '—'}</td></tr>"
            for bucket in observed["addPieces"]["buckets"]
        )
        skr_obs = observed.get("sessionKeyRegistry") or {}
        anvil_single = by_key[("fresh", 1)]["payer"]["gasUsed"]
        observed_single = observed["addPieces"]["buckets"][0]
        login_obs = skr_obs.get("medianGas")
        login_anvil = results["login"]["twoPermissions"]["gasUsed"]
        login_match = (
            f"The observed SKR login median ({fmt_gas(login_obs)}) matches the harness login "
            f"measurement ({fmt_gas(login_anvil)}) within "
            f"{abs(login_obs - login_anvil) / login_obs * 100:.0f}%."
            if login_obs
            else ""
        )
        observed_section = f"""
<h2>Mainnet observed baseline (30 days, foc-observer)</h2>
<table>
<thead><tr><th>Pieces per call</th><th>Txs</th><th>Median gas</th><th>p10</th><th>p90</th></tr></thead>
<tbody>{rows}</tbody>
</table>
<p>Observed effective gas price: median {observed_price / 1e6:.2f}M attoFIL/gas.
SessionKeyRegistry logins observed: {skr_obs.get('eventCount30d', 'n/a')} in 30 days,
median {fmt_gas(skr_obs['medianGas']) if skr_obs.get('medianGas') else 'n/a'} gas.</p>
<p class="small"><b>Reading the cross-check:</b> the harness signs pieces with <i>empty</i> metadata, so its
totals sit at the floor of the observed range (anvil single-piece {fmt_gas(anvil_single)} vs observed
median {fmt_gas(observed_single['medianGas'])}, p10 {fmt_gas(observed_single['p10Gas'])}). The observed
per-piece slope (~152M between the 1 and 33–40 buckets) exceeds the harness fit because production calls
carry per-piece metadata; the pricing model's 120M/piece sits between the two. The session-key delta is
unaffected by this: it is metadata-independent (one registry STATICCALL either way). {login_match}</p>
"""

    generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    document = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>FWSS session-key gas benchmark</title>
<style>
html {{ background: #ffffff; }}
body {{ font-family: -apple-system, "Segoe UI", sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; background: #ffffff; color: #1a1a1a; line-height: 1.5; }}
table {{ border-collapse: collapse; width: 100%; margin: 0.75rem 0 1.5rem; }}
th, td {{ border: 1px solid #d0d0d0; padding: 0.35rem 0.6rem; text-align: right; font-variant-numeric: tabular-nums; }}
th:first-child, td:first-child {{ text-align: left; }}
thead {{ background: #f3f1ec; }}
.tip {{ position: relative; text-decoration: underline dotted #999; text-underline-offset: 2px; cursor: help; }}
.tip::after {{
  content: attr(data-tip); display: none; position: absolute; bottom: 125%; left: 50%;
  transform: translateX(-50%); width: 17rem; background: #1a1a1a; color: #fff;
  padding: 0.5rem 0.65rem; border-radius: 6px; font-size: 0.75rem; font-weight: 400;
  text-align: left; line-height: 1.4; white-space: normal; z-index: 10;
  box-shadow: 0 2px 8px rgba(0,0,0,0.25);
}}
.tip:hover::after, .tip:focus::after {{ display: block; }}
.tip-end::after {{ left: auto; right: 0; transform: none; }}
h1 {{ font-size: 1.5rem; }} h2 {{ font-size: 1.15rem; margin-top: 2rem; }}
.headline {{ background: #f3f1ec; border: 1px solid #d0d0d0; padding: 1rem 1.25rem; border-radius: 6px; }}
code {{ background: #f3f1ec; padding: 0.1rem 0.3rem; border-radius: 3px; }}
.small {{ color: #555; font-size: 0.85rem; }}
</style>
</head>
<body>
<h1>FWSS session-key gas benchmark — SKR vs the 150M authorizer budget</h1>
<p class="small">Generated {generated}. fvm-anvil prepared mainnet state
({html.escape(str(config.get('preparedArchive')))}), FWSS v1.3.0 at
<code>{config['sourceCommit'][:12]}</code>, node revision <code>{config['nodeRevision'][:12]}</code>.
All gas figures are FVM gas.</p>

<div class="headline">
<p><b>Headline:</b> the session-key path adds a median of <b>{fmt_gas(median_delta)}</b> gas per operation
(SKR subcall alone: {fmt_gas(median_skr)}), against the ACL budget of <b>{fmt_gas(AUTHORIZER_GAS_LIMIT)}</b>
(<code>AUTHORIZER_GAS_LIMIT</code>, PR&nbsp;#536).</p>
<p>SKR share of the budget: <b>{median_delta / AUTHORIZER_GAS_LIMIT * 100:.2f}%</b> (median),
{max_delta / AUTHORIZER_GAS_LIMIT * 100:.2f}% (worst measured).
True incremental cost of the ACL, <code>150M − SKR_gas</code>:
<b>{fmt_gas(AUTHORIZER_GAS_LIMIT - median_delta)}</b> — i.e. the 150M budget is
{(AUTHORIZER_GAS_LIMIT - median_delta) / AUTHORIZER_GAS_LIMIT * 100:.1f}% new cost, not replacement.</p>
</div>

<h2>Session-key lifecycle: one-time client cost</h2>
<table>
<thead><tr><th>Operation</th><th>Gas</th>{price_headers}</tr></thead>
<tbody>{''.join(login_rows)}</tbody>
</table>
<p class="small">login is a standalone client-paid transaction; it is not part of any SP op fee.</p>

<h2>createDataSet: payer-signed vs session-key-signed (identical pre-state)</h2>
<table>
<thead><tr><th>Variant</th><th>Gas</th>{price_headers}</tr></thead>
<tbody>
<tr><td>payer-signed</td><td>{fmt_gas(create['payer']['gasUsed'])}</td>{price_cells(create['payer']['gasUsed'])}</tr>
<tr><td>session-key-signed</td><td>{fmt_gas(create['session']['gasUsed'])}</td>{price_cells(create['session']['gasUsed'])}</tr>
<tr><td>delta (session-key overhead)</td><td>{fmt_gas(create_delta)}</td>{price_cells(create_delta)}</tr>
</tbody>
</table>
<p class="small">SKR subcall within the session-signed create: {fmt_gas(create_skr)} gas.</p>

<h2>addPieces sweep: per-op session-key overhead</h2>
<table>
<thead><tr><th>Dataset</th><th>N pieces</th>
<th><span class="tip" tabindex="0" data-tip="Total FVM gas of the whole operation when the payer signs directly. No SessionKeyRegistry involved.">Payer gas</span></th>
<th><span class="tip" tabindex="0" data-tip="Total FVM gas of the same whole operation, from identical pre-state, signed by an authorized session key instead.">Session gas</span></th>
<th><span class="tip" tabindex="0" data-tip="Session − Payer: the full cost of the session-key path, i.e. what an IDataSetAuthorizer would replace.">Delta</span></th>
<th><span class="tip tip-end" tabindex="0" data-tip="Gas charged inside the SessionKeyRegistry call frame only (from filecoin_getGasTrace): one authorizationExpiry mapping read.">SKR subcall</span></th>
<th><span class="tip tip-end" tabindex="0" data-tip="Delta divided by the 150M AUTHORIZER_GAS_LIMIT from PR #536.">Delta as % of 150M</span></th></tr></thead>
<tbody>{''.join(add_rows)}</tbody>
</table>
<p class="small"><b>Column definitions (hover or click the dotted headers for the same):</b>
<i>Payer gas</i> and <i>Session gas</i> are the total gas of the <b>whole</b> createDataSet/addPieces
transaction — the two variants are identical except for who signed the client authorization, and both
run from identical pre-state via snapshot/revert. <i>SKR subcall</i> is <b>just</b> the access-validation
call inside it: the SessionKeyRegistry <code>authorizationExpiry</code> read that only the session-signed
variant performs (constant 2.73M across all runs). <i>Delta</i> = Session − Payer ≈ SKR subcall + ~0.4M of
caller-side overhead (making the cross-contract call: actor code load, syscall, ABI encode/decode, and the
expiry check). Individual deltas scatter ±~2M from state-read charging noise between the two executions
(&lt;1% of the operation); the constant SKR subcall is the hard number.</p>

<h2>Fitted gas model vs pricing-doc model</h2>
<table>
<thead><tr><th>Dataset</th><th>Fitted base</th><th>Fitted per-piece</th>
<th>Doc model base</th><th>Doc model per-piece</th></tr></thead>
<tbody>{''.join(fit_rows)}</tbody>
</table>
<p class="small">Doc model: G(N) = 175M + 120M·N from pricing-rationale.md. Fit is least-squares on the payer-signed sweep.</p>

{observed_section}

<h2>Method</h2>
<p>Each operation ran twice from identical pre-state (<code>evm_snapshot</code>/<code>evm_revert</code>):
once signed by the payer (no SessionKeyRegistry interaction, verified from traces) and once by an
authorized session key (exactly one SKR <code>authorizationExpiry</code> STATICCALL, verified from traces).
The delta is the total session-key verification overhead — the gas an
<code>IDataSetAuthorizer</code> would replace. <code>filecoin_getGasBreakdown</code> and
<code>filecoin_getGasTrace</code> on fvm-anvil supplied exact per-transaction and per-subcall gas.</p>
<p class="small">Non-goals: no PR #536 code was deployed and no authorizer was measured (decision D6);
the P256/passkey authorizer benchmark is feasible on this stack (NV28 ships P256VERIFY) and remains a follow-up.
Reproduction: <code>benchmarks/fwss/run.py --rpc-url ... </code> then <code>benchmarks/fwss/report.py</code>.</p>
</body>
</html>
"""
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(document)
    print(args.output)


if __name__ == "__main__":
    main()
