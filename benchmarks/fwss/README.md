# FWSS gas benchmark

This benchmark compares PDPVerifier and Filecoin Warm Storage Service implementations against imported mainnet state. It measures lifecycle transactions, records complete Filecoin gas traces, resumes completed cases, and searches for practical batch-size limits. See [`methodology.md`](methodology.md) for the workload and result model.

Run commands from the repository root.

## Scope: imported-state benchmarks, not fresh Curio deployment

The configured baseline is the historical FWSS 1.3.0/PDPVerifier 3.4.0 archive.
The current local FWSS build entry expects a monolithic
`out/FilecoinWarmStorageService.sol/FilecoinWarmStorageService.json` artifact and a
single-implementation proxy upgrade. It does **not** deploy FWSS 1.5.0's ERC-8167
dispatcher/facet graph; cloning the latest Filecoin Services checkout does not make
those local variants compatible.

For the verified fresh FWSS 1.5.0/PDPVerifier 3.5.0 deployment and writable Curio
workflow, use [`tests/curio/README.md`](../../tests/curio/README.md). That workflow
validates real PDP proving but is not a replacement for benchmark measurements.

## Prerequisites

- `cast` and `forge` from Foundry.
- `uv` for the runner's Python dependency.
- A release build of `fvm-anvil`.
- `snapshots/fwss-mainnet-v1.3.0.press.car.zst`.

Build the node:

```console
$ cargo build --release -p fvm-anvil
```

The local variants use independent, ignored checkouts under `extern/`; they do not depend on sibling repositories outside this checkout. The following commands acquire sources only. Select revisions compatible with the artifact paths and upgrade protocol in `suite.json` before using a local variant:

```console
$ git clone --recurse-submodules https://github.com/FilOzone/pdp.git extern/pdp
$ git clone --recurse-submodules https://github.com/FilOzone/filecoin-services.git extern/filecoin-services
```

Build each selected Solidity revision once with `forge build` to confirm it compiles and produces the configured artifact. The runner rebuilds it before deploying local variants. Do not use the latest dispatcher/facet FWSS checkout with the existing monolithic build entry.

## Prepare the historical archive

The deployed baseline is reproducible once a valid historical archive is supplied.
The exact original ForestCAR source path and its present availability are not
established here. Obtain a mainnet ForestCAR for epoch **6,251,280**; do not
substitute a current/latest snapshot or the provenance of a PDP-only archive.
The recipe selects actors but does not select the source epoch or recreate
missing historical state.

Set `SOURCE_FOREST_CAR` to that supplied file, then inspect and extract it:

```console
$ cargo build --release -p press -p fvm-anvil
$ target/release/press inspect "$SOURCE_FOREST_CAR"
$ target/release/press build \
    --snapshot "$SOURCE_FOREST_CAR" \
    --recipe recipes/fwss-mainnet-v1.3.0.json \
    --output snapshots/fwss-mainnet-v1.3.0.press.car.zst
$ target/release/press inspect snapshots/fwss-mainnet-v1.3.0.press.car.zst
```

Confirm the source epoch is 6,251,280 and inspect the prepared roots and selected
actor inventory. After starting the node, validate the expected proxy code and
implementation targets, and confirm FWSS `VERSION()` is `1.3.0` and PDPVerifier
`VERSION()` is `3.4.0`; see the address table in
[`methodology.md`](methodology.md#imported-state-and-addresses).
The runner also checks the imported root and these version strings.

The `deployed` variant uses that historical state without candidate source
checkouts. Local variants are user-selected compatible experiments, not pinned
historical candidate revisions: select and record exact source revisions that
match the configured artifacts, constructors, and upgrade protocol before use.

## Start the node

```console
$ target/release/fvm-anvil \
    --state snapshots/fwss-mainnet-v1.3.0.press.car.zst \
    --port 18546
```

The runner mutates the in-process chain and verifies that each invocation starts at the imported state. Restart the node before each separate `run` or `max-batch` invocation.

## Run cases

List available comparison keys without starting the node:

```console
$ benchmarks/fwss/run.py list --variant deployed
```

Run every fixed case for the deployed implementation:

```console
$ benchmarks/fwss/run.py run --variant deployed
```

Select cases with repeatable shell-style patterns:

```console
$ benchmarks/fwss/run.py run \
    --variant deployed \
    --case 'createDataSet/*' \
    --case 'addPieces/batchSize-32/*'
```

Omitting `--variant` runs every variant configured in `suite.json`. Completed cases with the same measurement identity are skipped. Add `--force` to replace selected results.

Reuse requires the archive and running node build to remain unchanged. On first
use the manifest records the archive SHA-256 and imported roots; on reuse the
runner checks archive **size** and reuses the recorded digest rather than hashing
the archive again. It checks the node's current root and baseline proxy versions,
and fingerprints the repository revision/dirty description, not the live node
binary. These checks do not completely authenticate the archive or running build.
If either changed, use a separate results directory configured in `suite.json`
and a fresh node rather than relying on automatic skipping.

Search for a batch boundary:

```console
$ benchmarks/fwss/run.py max-batch \
    --variant deployed \
    --operation addPieces \
    --metadata one \
    --starting-piece-count 0
```

The supported boundary operations are `addPieces`, `schedulePieceDeletions`, `processPieceDeletions`, `nextProvingPeriod`, and `cleanupPieces`. `processPieceDeletions` is not applicable to PDP builds that still process removals inside `nextProvingPeriod`. Use `--ceiling` to override the configured search ceiling.

## Report results

Results default to `benchmarks/fwss/results/mainnet-v1.3.0` and are ignored by Git.

```console
$ benchmarks/fwss/report.py \
    --baseline deployed \
    --variant local-pdp \
    --variant local-new
```

Filter with `--operation` or select `--format json` or `--format csv` for machine-readable output. Reporting does not require a running node.

## Legacy metadata cleanup

The [PR #658 cleanup runbook](metadata-cleanup.md) covers frozen-state inventory,
historical coverage bounds, globally storage-ordered packed operations, and
sequential execution with a durable transaction journal. This is a separate
maintenance workflow, not the lifecycle benchmark runner.
