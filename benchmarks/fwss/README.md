# FWSS session-key gas benchmark

Measures FWSS `createDataSet` and `addPieces` FVM gas on fvm-anvil with prepared mainnet state, and isolates the session-key (SessionKeyRegistry) share of it. Built to answer one pricing question: how much of the 150M `AUTHORIZER_GAS_LIMIT` (FilOzone/filecoin-services#536) does the SKR path it replaces actually use. Re-run it at each fee recalibration or after an FWSS upgrade.

## What it measures

Every operation runs twice from identical pre-state (`evm_snapshot`/`evm_revert`): once payer-signed (no SKR interaction, asserted from traces) and once session-key-signed (exactly one SKR `authorizationExpiry` STATICCALL, asserted from traces). The variant delta is the total session-key verification overhead. `filecoin_getGasBreakdown` and `filecoin_getGasTrace` supply exact per-transaction and per-subcall gas. Sweep: N in {1, 2, 4, 8, 16, 26, 32, 40} pieces, on a fresh dataset and again after pre-filling ~2,000 pieces. `SessionKeyRegistry.login` is measured separately as the one-time client-paid cost.

Piece metadata is empty in every signed payload, so totals sit at the floor of the mainnet-observed range; the session-key delta is metadata-independent.

## Pins

Contract addresses and ABI shapes match the mainnet FWSS v1.3.0 deployment, FilOzone/filecoin-services commit `c2ff7f8ac574f62e5d9f9ace2cf2efbb64b0f3a4` (SessionKeyRegistry submodule `74fc4e94`). EIP-712 signatures use chain id 31337 because `EIP712Upgradeable` recomputes the domain separator on the fork. If the deployed FWSS version changes, re-pin the typehash strings and extraData layouts in `eip712.py`/`run.py` against the new revision before trusting results.

## Running

Requires the release binaries, `cast`, and a prepared archive containing the 13-actor FWSS graph (see `recipes/fwss-mainnet-v1.3.0.json`):

```console
cargo build --release -p press -p fvm-anvil
target/release/press build \
  --snapshot snapshots/<mainnet-forest-snapshot>.forest.car.zst \
  --recipe recipes/fwss-mainnet-v1.3.0.json \
  --output snapshots/<archive>.press.car.zst
target/release/fvm-anvil --state snapshots/<archive>.press.car.zst --port 18548
python3 benchmarks/fwss/run.py --rpc-url http://127.0.0.1:18548 --prepared-archive snapshots/<archive>.press.car.zst
python3 benchmarks/fwss/report.py
```

`run.py` writes `results/fwss-gas.json`; `report.py` renders `results/fwss-gas-report.html` and folds in `observed/mainnet-gas-30d.json` (foc-observer mainnet baseline) when present. Both directories are gitignored.
