# FWSS gas benchmark

This benchmark compares PDPVerifier and Filecoin Warm Storage Service implementations against imported mainnet state. It measures lifecycle transactions, records complete Filecoin gas traces, resumes completed cases, and searches for practical batch-size limits. See [`design.md`](design.md) for the workload and result model.

Run commands from the repository root.

## Prerequisites

- `cast` and `forge` from Foundry.
- `uv` for the runner's Python dependency.
- A release build of `fvm-anvil`.
- `snapshots/fwss-mainnet-v1.3.0.press.car.zst`.

Build the node:

```console
$ cargo build --release -p fvm-anvil
```

The `deployed` variant uses only contracts from the snapshot. Before running a local variant, update the corresponding `repository` paths in [`suite.json`](suite.json) and build the Solidity projects once with `forge build` to confirm they compile.

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

The supported boundary operations are `addPieces`, `schedulePieceDeletions`, `nextProvingPeriod`, and `cleanupPieces`. Use `--ceiling` to override the configured search ceiling.

## Report results

Results default to `benchmarks/fwss/results/mainnet-v1.3.0` and are ignored by Git.

```console
$ benchmarks/fwss/report.py \
    --baseline deployed \
    --variant local-pdp \
    --variant local-new
```

Filter with `--operation` or select `--format json` or `--format csv` for machine-readable output. Reporting does not require a running node.
