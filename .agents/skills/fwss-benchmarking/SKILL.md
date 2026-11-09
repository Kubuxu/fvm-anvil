---
name: fwss-benchmarking
description: Benchmark Filecoin Warm Storage Service (FWSS) and PDPVerifier operations with fvm-anvil. Use this skill whenever the user asks to benchmark FWSS or PDP changes, compare deployed and local FWSS/PDP versions, measure callback gas, find batch-size limits, analyze stored results under benchmarks/fwss, or produce an FWSS gas report—even when they only name an operation such as addPieces, removals, or proving.
compatibility: Requires this repository's FWSS benchmark harness, a compatible prepared FWSS archive, a release fvm-anvil binary, cast, forge, and uv.
---

# Benchmark FWSS with fvm-anvil

Use the existing `benchmarks/fwss` harness. Keep the experiment narrow: configure the requested implementations, run only missing cases, preserve completed results, and report the operations that matter to the requested change.

## Boundaries

This skill benchmarks an existing prepared FWSS state. If no suitable archive exists, use the `fwss-snapshot-extraction` skill first.

Do not redesign the harness for each experiment. A normal PDP or FWSS implementation variant belongs in `benchmarks/fwss/suite.json`; change Python only when the contract protocol or fixture flow actually changed.

Read before running:

- `benchmarks/fwss/README.md` for commands;
- `benchmarks/fwss/suite.json` for the active archive, builds, variants, and cases;
- relevant sections of `benchmarks/fwss/methodology.md` when fixture chronology, result identity, or boundary semantics matter.

## Establish the experiment

Record:

- prepared archive and deployed PDP/FWSS versions;
- baseline and candidate variants;
- exact source commits, branches, or PRs;
- operations and workload profiles requested;
- whether fixtures must predate or postdate upgrades;
- fixed batch sizes and any boundary-search ceiling;
- existing completed cases that already answer part of the request.

Prefer repository-local ignored checkouts:

- `extern/pdp`;
- `extern/filecoin-services`.

Do not assume their current branch is the requested candidate. Resolve and report the exact commits. Build the selected Solidity projects before measuring.

## Reuse results first

Results under `benchmarks/fwss/results/` are independent, reusable measurements. Before starting the node, list configured cases and inspect stored results. Run only cases missing for the requested comparison.

Never rerun transactions to:

- change report wording or labels;
- fit another model;
- sort a table differently;
- add an operation to an offline report;
- regenerate JSON, CSV, or HTML from existing measurements.

Use `--force` only when the user explicitly needs an existing matching measurement replaced or an execution-affecting input changed without producing a new identity.

## Configure builds and variants

A build selects one PDP or FWSS implementation. A variant combines builds and fixture chronology.

Use:

- `fixtureCreation: "before-upgrades"` for imported/legacy state exercised by candidate code;
- `fixtureCreation: "after-upgrades"` for state created by the selected implementations.

Adding an ordinary candidate should require only build and variant entries in `suite.json`. Preserve stable variant names when updating their selected source revisions unless the user requests a new comparison target.

For local FWSS builds, deploy and link every direct Forge library reference from the same build output. Never link a new FWSS implementation against imported Rails or SignatureVerification libraries. Verify the adopted proxy implementation and runtime bytecode after the real upgrade path completes.

Treat library deployment as validity setup, not a headline benchmark result.

## Start from a clean node

Build the current binary when needed:

```console
cargo build --release -p fvm-anvil
```

Start the node with the archive and port configured for the benchmark:

```console
target/release/fvm-anvil \
  --state snapshots/fwss-mainnet-v1.3.0.press.car.zst \
  --port 18546
```

Restart the node before each separate `run` or `max-batch` invocation. The runner relies on a known imported root and in-process Anvil snapshots.

Validate the configured proxy addresses and deployed version strings before collecting measurements.

## Run the smallest useful case set

List comparison keys:

```console
benchmarks/fwss/run.py list --variant VARIANT
```

Run selected fixed cases with repeatable patterns:

```console
benchmarks/fwss/run.py run \
  --variant VARIANT \
  --case 'addPieces/*'
```

Completed matching cases are skipped automatically.

Choose workloads from the change under test. Common priorities are:

1. `addPieces`;
2. complete piece-removal flow;
3. `provePossession`;
4. other lifecycle operations affected by the change.

Do not benchmark a callback merely for nominal coverage when FWSS deliberately rejects it. Do not expand the matrix with dimensions that cannot illuminate the requested change.

Every compared batch size must start from equivalent fixture state. Use the harness snapshots; do not let one measured case mutate the starting state of the next.

## Search batch limits

Use the harness command rather than ad hoc loops:

```console
benchmarks/fwss/run.py max-batch \
  --variant VARIANT \
  --operation addPieces \
  --metadata none \
  --starting-piece-count 0
```

Search separately for each operation, variant, metadata profile, and starting-state profile that matters.

Report boundary results precisely:

- adjacent observed success and failure: exact maximum;
- every probe through the ceiling succeeds: `successful through N`, not an exact maximum;
- fixture preparation fails first: prerequisite-limited, with the observed lower bound and blocking prerequisite;
- success above a failure: non-monotonic, not a single maximum.

Large cases that fail on the baseline can still demonstrate candidate capacity. Do not require a successful baseline measurement for every candidate batch size.

## Classify outcomes

Expected measurement outcomes include:

- success;
- contract revert;
- out of gas;
- admission failure;
- setup unreachable;
- not applicable.

A failed case is useful data. Preserve its transaction envelope, receipt when present, and Filecoin trace. A runner/configuration error—wrong contract, invalid signature, failed funding, missing trace, or broken snapshot restore—invalidates the measurement and should stop the run.

Use `filecoin_getGasTrace` to diagnose execution. Do not rely on `filecoin_gasBreakdown` for gas attribution. Distinguish contract reverts from Filecoin message-size limits, event-size limits, gas limits, and fixture prerequisites.

## Compare equivalent work

For `addPieces`, separate fixed and per-piece cost with a stated fit:

```text
G(n) = F + Pn
```

Report the points and range used, metadata profile, starting piece count, fixed cost `F`, marginal cost `P`, and fit error. Do not extrapolate beyond measured behavior without labeling it as a model prediction.

When removal processing differs between PDP versions, compare the complete public flow:

```text
deployed  = schedulePieceDeletions + nextProvingPeriod
candidate = schedulePieceDeletions + processPieceDeletions + nextProvingPeriod
```

Also show individual transactions when they explain the total. Do not compare only one side of a changed multi-transaction workflow.

If the baseline fails and the candidate succeeds, report the capacity difference and failure class; do not invent a percentage gas improvement.

## Report from stored measurements

Use the existing reporter:

```console
benchmarks/fwss/report.py \
  --baseline deployed \
  --variant CANDIDATE
```

Use its JSON or CSV output for further sorting, fitting, plotting, or HTML generation. Do not manually reconstruct data already available through the reporter.

Lead with decision-relevant findings:

1. primary operation gas and fixed/per-unit decomposition;
2. practical batch capacity;
3. complete equivalent-workload comparison;
4. meaningful regressions;
5. proving or secondary operations affected by the change;
6. concise provenance, method, and limitations.

Keep harness-wide outcome counts, setup debugging, addresses, and expected library deployment out of executive findings unless they change the interpretation.

## Required final evidence

Return:

- archive and deployed version baseline;
- candidate source commits and build fingerprints;
- exact variants and cases executed or reused;
- result paths;
- gas measurements and outcome classifications;
- exact or qualified batch boundaries;
- trace-backed explanation for important failures or deltas;
- report command and output path;
- operations intentionally omitted and why.

State clearly which results were newly measured and which were reused. Report only observed measurements; label fitted or inferred values.