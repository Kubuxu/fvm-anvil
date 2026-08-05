# fvm-anvil implementation plan

Status: active  
Last updated: 2026-08-05

This document records current component ownership and remaining implementation work. Design
rationale and compatibility constraints live in [`docs/design.md`](design.md). Offline state
preparation and prepared-state import are documented in [`docs/press.md`](press.md).

## Current status

The executable transaction path, HTTP RPC transport, synthetic canonical chain, instant/manual/
interval/mixed startup mining, snapshots, Filecoin gas diagnostics, deterministic development
accounts, and prepared-state import are implemented.

Remaining work is broader Anvil development-control compatibility and writable-overlay persistence.
The authoritative implemented RPC list is in
[`docs/design.md`](design.md#implemented-rpc-compatibility); unsupported methods return JSON-RPC
`method not found`.


## Architectural decision

The workspace has six crates:

```text
press                     offline snapshot selection, patching, and prepared archive output
  └── fvm-anvil-state     ForestCAR and prepared-state formats

fvm-anvil                 CLI, configuration, process lifetime, and graceful shutdown
  ├── fvm-anvil-rpc       JSON-RPC wire parsing, handlers, response conversion, and transport
  │     └── fvm-anvil-node
  └── fvm-anvil-node      sole owner of mutable execution and synthetic chain state
        ├── fvm-anvil-eth pure Ethereum-to-Filecoin protocol bridge
        └── fvm-anvil-state
```

Protocol types belong to the lowest crate that owns their invariants. The state crate exists for the
demonstrated on-disk format boundary shared by `press` and the node; it does not own live execution.

Production crates must not depend on Forest, `anvil-core`, the main `anvil` crate, `revm`, or the
ref-FVM integration harness. Forest, Foundry, ref-FVM integration tests, and builtin actor sources
under `extern/` are read-only behavioral references.

Actor Wasm and code CIDs come from the selected CAR bundle. Production code does not depend on
individual builtin actor crates; it adapts only the minimal serialized v18 state representations
needed for genesis, with attribution and explicit schema tests.

## Component ownership

### `fvm-anvil-eth`

This deterministic crate owns:

- Legacy/EIP-155 and EIP-1559 envelope decoding.
- Chain-ID and signature validation and Ethereum sender recovery.
- Ethereum-address to delegated f4-address conversion.
- EAM creation versus EVM invocation selection.
- Delegated signature and exact Filecoin `SignedMessage` construction.
- Exact serialized signed-message length used for FVM inclusion gas.
- EAM creation return and FVM EVM-event conversion.
- Typed bridge errors.

It must not access a blockstore, state root, machine, pool, clock, RPC request, or mutable node
state.

### `fvm-anvil-node`

A worker-local `NodeRuntime` is the only owner of:

- One shared IPLD blockstore.
- One reusable `EnginePool` and its compiled actor cache.
- The adopted FVM state root.
- Actor bundle and manifest artifacts.
- Synthetic blocks and historical roots.
- Transaction, receipt, log, and gas indexes.
- The pending transaction pool.
- Mining mode.
- Snapshots.

`NodeRuntime` is constructed inside a dedicated 64 MiB OS thread. That thread runs a bounded
blocking loop over semantic commands. Async callers receive a cloneable `NodeHandle`; the runtime,
blockstore, machine, executor, state tree, and externs never cross the worker boundary. Startup
interval mining is scheduled by the binary and submits semantic mine commands through `NodeHandle`.


The in-memory implementation uses `Rc<MemoryBlockstore>`. Cloning `MemoryBlockstore` itself would
copy its internal map and fork state; only the `Rc` is cloned into ephemeral machines.

Each block or simulation constructs a fresh `DefaultMachine` and `DefaultExecutor`. The runtime
reuses the engine pool, not an executor. `ThreadedExecutor` is not used because the node worker
already supplies the required stack and is the single execution owner.

The internal execution seam is conceptually:

```text
execute_at(root, block_context, prepared_messages, mode) -> candidate execution batch
commit_block(candidate batch) -> adopted root plus canonical indexes
```

Only canonical block commit may publish a root, advance the head, update canonical indexes, or
remove included pool entries. Calls and gas estimation execute without publishing their candidate
root. Fatal machine/executor errors leave the adopted root and indexes unchanged; normal actor
failure exit codes produce failed transaction receipts and are committed.

### `fvm-anvil-rpc`

This crate owns:

- A local serde request enum and exact method aliases.
- Empty, omitted, positional, and sequence parameter parsing.
- Quantity and fixed-byte validation.
- Node-domain command construction and result conversion.
- JSON-RPC error mapping.
- HTTP transport using generic Foundry `anvil-server`/`anvil-rpc` plumbing.
- Standard Ethereum responses and the separate `filecoin_` namespace.

It never decodes a raw Ethereum transaction, derives canonical receipts, owns a pool, accesses FVM
state, or sends JSON values to the node worker.

The matching Foundry transport crates are pinned to one exact Git revision and used with
`anvil-server` default features disabled until IPC is implemented. No production manifest uses a
path under ignored `extern/`.

### `fvm-anvil`

The binary crate owns CLI definitions, configuration precedence, development-account generation,
startup ordering, listener binding, tracing, the startup banner, and graceful shutdown. It contains
no RPC method logic, transaction conversion, actor initialization, or chain transition logic.

## Public node seam

`NodeHandle` exposes typed asynchronous methods while its channel command enum remains private.
Public node DTOs may use stable Alloy primitives such as `Address`, `B256`, `Bytes`, and `U256`, but
not Alloy RPC response types or serde attributes.

Current semantic operations are:

- Chain ID, accounts, block, account, transaction, receipt, log, fee, and gas queries.
- Raw transaction submission.
- Calls and gas estimation against a selected root.
- Forced and automine-dependent block production.
- Snapshot creation and revert.
- Automine control.
- Filecoin gas breakdown, call trace, and state-root queries.

`NodeError` retains stable semantic categories; only the RPC crate maps them to JSON-RPC errors.

## Canonical state and snapshots

The canonical chain store is the only source of blocks, historical roots, transaction positions,
receipts, logs, and gas diagnostics. RPC and bridge crates keep no parallel caches.

Snapshots retain the adopted root, canonical blocks, transaction and receipt indexes, gas indexes,
epoch, and timestamp. They do not copy the blockstore, engine pool, bundle, worker, mining mode, or
transaction pool. Revert consumes the selected snapshot and all newer snapshots, removes the
canonical suffix and its indexes, and retains pool entries that are still valid against restored
state. Transactions mined in the removed suffix are not automatically reinserted.

## Current roadmap

### Broader development controls

Implement time controls, fee/FIFO transaction ordering, pool inspection and removal, and balance and
nonce controls. Each method needs exact Foundry wire-format comparison, a public JSON-RPC
compatibility case, and a real endpoint smoke test.

Runtime mining controls must have one timer owner. A call that changes automine or interval behavior
must take ownership away from the startup scheduler; snapshot/revert must not restore process-control
state.

### Versioned direct EVM state controls

Implement a private builtin-actors v18 state adapter, then `anvil_setCode`,
`anvil_setStorageAt`, ERC-20 balance probing, and allowance probing in that dependency order.
Direct mutations must publish a new current FVM root, participate in snapshots, preserve unaffected
actor fields, and be verified through public EVM behavior. Every state-schema test names the actor
bundle and network version explicitly.

Local `--actor-bundle` inputs remain a genesis-time development choice. The user is responsible for
selecting a bundle compatible with the v18 state adapter.

### Prepared-state persistence

Persist the writable overlay and adopted root without copying the immutable prepared ForestCAR.
Restart must recover the same current root and canonical projection or fail clearly; partial
publication must not be accepted.

### Deferred work

- WebSocket subscriptions and optional IPC.
- Broader Anvil extensions where FEVM can provide honest semantics.
- General remote-chain forking.
- New imported-actor synthesis and workload-specific state witnesses.

## Completion evidence

For runtime and RPC changes, exercise the real binary and changed public endpoint before running
focused tests. `tests/acceptance/run.sh` remains the end-to-end compatibility gate for deployment,
transaction, event, receipt, block, gas, and snapshot behavior. Run workspace formatting, strict
Clippy, and tests only after the endpoint smoke test succeeds.
