# FWSS gas benchmark

This benchmark compares PDPVerifier and Filecoin Warm Storage Service implementations against imported mainnet state. It measures lifecycle transactions, records complete Filecoin gas traces, resumes completed cases, and searches for practical batch-size limits. See [`design.md`](design.md) for the workload and result model.

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
