# fvm-anvil

`fvm-anvil` is a local Ethereum JSON-RPC node that executes transactions with `ref-fvm` and Filecoin's builtin EAM, EVM, and EthAccount actors. It is intended for contract development and gas measurement where Filecoin execution semantics matter.

Ethereum blocks, receipts, and state queries are compatibility views over FVM actor state. Gas values reported through Ethereum RPC are FVM gas, not `revm` gas.

## Status

The core workflow is implemented: contract deployment, calls, transactions, receipts, logs, gas estimation, manual and automatic mining, and snapshots. The RPC surface is smaller than Anvil's; the current method list is maintained in [the design document](docs/design.md#implemented-rpc-compatibility).

Curio PDP is also supported: native Filecoin head/history/beacon APIs and WebSocket head
notifications work alongside Ethereum RPC. The writable Skiff workflow is verified with
a freshly deployed FWSS 1.5.0/PDPVerifier 3.5.0 stack and Docker PostgreSQL/ScyllaDB.

This project does not provide Filecoin consensus, Forge cheatcodes, general remote-chain forking, or opcode-level Geth traces.

## Build and run

The repository pins its Rust toolchain. Build and start the node with:

```bash
cargo build --release -p fvm-anvil
target/release/fvm-anvil
```

The default RPC endpoint is `http://127.0.0.1:8545`, with chain ID `31337`. Ten deterministic development accounts are funded with 10,000 FIL each. The account keys and mnemonic are development credentials and must not be used on a public network.

On first startup, the node downloads and verifies the pinned builtin-actors v18 bundle. A compatible local CAR can be selected with `--actor-bundle`.

```bash
cast chain-id --rpc-url http://127.0.0.1:8545
cast balance 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  --rpc-url http://127.0.0.1:8545
```

Run `target/release/fvm-anvil --help` for account, genesis, mining, actor-bundle, prepared-state, and server options.

## Curio PDP and fresh FWSS deployment

Follow the [step-by-step Curio/FWSS guide](tests/curio/README.md) to select the tested
source revisions, build Skiff, launch a local node, deploy the contract graph, and run
dataset creation, piece ingestion, and real proof submission.

Native `Filecoin.*` and Ethereum methods share `/rpc/v1`; `Filecoin.ChainNotify` uses
Go-compatible WebSocket channels. The guide includes direct RPC examples, Curio's
WebSocket-origin configuration, signer setup, result inspection, and clean repeat runs.

This workflow starts from fresh local state, not a downloaded mainnet snapshot. Its default
development payment token is native-FIL-backed Solady WETH, **not USDFC**; an existing
real 18-decimal token can be supplied. PoRep, PoSt, native mining/wallet APIs, and general
Lotus compatibility are not included.

The [Curio PDP compatibility record](docs/curio-pdp-compatibility.md) lists the supported
APIs, limits, tested revisions, and verification evidence. The separate
[FWSS benchmark](benchmarks/fwss/README.md) uses historical imported state and a different
deployment model.

## Filecoin diagnostics

Filecoin-specific data is kept outside standard Ethereum receipt fields:

```bash
cast rpc filecoin_getGasBreakdown TRANSACTION_HASH
cast rpc filecoin_getGasTrace TRANSACTION_HASH
cast rpc filecoin_getStateRoots
```

The gas breakdown exposes ref-FVM burns, refunds, penalties, and miner tips. The gas trace contains nested actor calls and ref-FVM charge points in milligas.

## Prepared Filecoin state

The `press` tool can extract selected actors from a ForestCAR snapshot, apply explicit EVM code or storage patches, and produce a smaller archive for local execution:

```bash
cargo build --release -p press
target/release/press build \
  --snapshot SOURCE.forest.car.zst \
  --recipe recipes/RECIPE.json \
  --output OUTPUT.press.car.zst

target/release/fvm-anvil --state OUTPUT.press.car.zst
```

Prepared-state format, validation, and limitations are documented in [docs/press.md](docs/press.md). Snapshot files are not included in the repository.

## Development

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The public acceptance test requires Foundry (`cast` and `forge`) and `jq`. Run a node in another terminal, then:

```bash
RPC_URL=http://127.0.0.1:8545 \
FILECOIN_ASSERTIONS=1 \
tests/acceptance/run.sh
```

See [AGENTS.md](AGENTS.md) for contribution constraints, [docs/design.md](docs/design.md) for architecture and behavior, and [docs/roadmap.md](docs/roadmap.md) for remaining work. Prepared-state examples are in [docs/press-recipes.md](docs/press-recipes.md).

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT License](LICENSE-MIT), at your option.
