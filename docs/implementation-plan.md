# fvm-anvil implementation plan

Status: active  
Last updated: 2026-08-04

This document is the current execution plan for the architecture recorded in `docs/design.md`.
It defines component ownership, dependency direction, implementation waves, delegation boundaries,
and the evidence required before each wave is complete. Design rationale and compatibility findings
remain in `docs/design.md`.

Offline mainnet-state preparation and the active `press` tool are tracked in
[`docs/press.md`](press.md). The shared `fvm-anvil-state` crate owns its prepared-state format and
ForestCAR storage; node import remains the next integration boundary.

## Current status

Waves 0-3 are implemented. Waves 4-5 remain planned scope.

Observed evidence: 14 bridge unit tests pass; deterministic node tests pass; three ignored
real-bundle NV28/v18 transaction, mining, simulation, and snapshot tests pass; nine RPC tests
pass; and five CLI tests pass. The actual
binary was exercised over HTTP: Forge deployed NarrowSlice, Cast called and mutated it, and the
resulting event, receipt, transaction, and block were projected correctly. The endpoint also
returned the Filecoin gas breakdown, and snapshot/revert restored state and canonical index
visibility. Workspace formatting, Clippy, and the workspace test suite remain final follow-up
validation and are not asserted complete here.


## Architectural decision

The workspace has four crates:

```text
fvm-anvil                 CLI, configuration, process lifetime, and graceful shutdown
  ├── fvm-anvil-rpc       JSON-RPC wire parsing, handlers, response conversion, and transport
  │     └── fvm-anvil-node
  └── fvm-anvil-node      sole owner of mutable execution and synthetic chain state
        └── fvm-anvil-eth pure Ethereum-to-Filecoin protocol bridge
```

There is no shared types crate. Types belong to the lowest crate that owns their invariants. A new
crate or trait is added only when a demonstrated dependency boundary requires it.

Production crates must not depend on Forest, `anvil-core`, the main `anvil` crate, `revm`, or the
ref-FVM integration harness. Forest, Foundry, ref-FVM integration tests, and builtin actor sources
under `extern/` are read-only behavioral references.

Actor Wasm and code CIDs come from the selected CAR bundle. Initial production code does not
depend on individual builtin actor crates; it adapts only the minimal serialized v18 state
representations needed for genesis, with attribution and explicit schema tests.

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
- Transaction, receipt, log, Filecoin CID, and gas indexes.
- The pending transaction pool.
- Mining mode and one-shot block overrides.
- Snapshots.

`NodeRuntime` is constructed inside a dedicated 64 MiB OS thread. That thread runs a bounded
blocking loop over semantic commands. Async callers receive a cloneable `NodeHandle`; the runtime,
blockstore, machine, executor, state tree, and externs never cross the worker boundary. Interval
mining and timer-driven execution remain Wave 4 work.


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

Only `commit_block` may publish a root, advance the head, update canonical indexes, remove included
pool entries, or consume block overrides. Calls and gas estimation use `execute_at` without
`commit_block`. Fatal machine/executor errors leave the adopted root and indexes unchanged; normal
actor failure exit codes produce failed transaction receipts and are committed.

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

Initial semantic operations are:

- Chain ID, accounts, block, account, transaction, receipt, log, and gas queries.
- Raw transaction submission.
- Calls and gas estimation against a selected root.
- Forced and mode-dependent mining.
- Snapshot creation and revert.
- Mining-mode and next-block controls.
- Filecoin message CID and gas diagnostics.

Newtypes distinguish Ethereum-visible gas, FVM gas, epochs, timestamps, message lengths, pool
arrival order, and snapshot IDs. `NodeError` retains stable semantic categories; only the RPC crate
maps them to JSON-RPC errors.

## Canonical state and snapshots

The canonical chain store is the only source of blocks, historical roots, transaction positions,
receipts, logs, and gas diagnostics. RPC and bridge crates keep no parallel caches.

Snapshots retain the adopted root, canonical frontier, epoch, timestamp, base fee, index frontiers,
and one-shot overrides. They do not copy the blockstore, engine pool, bundle, worker, mining mode,
or transaction pool. Revert consumes a successful snapshot ID, removes the canonical suffix and
its indexes, and revalidates still-pending pool entries against restored state. Transactions mined
in the removed suffix are not automatically reinserted, matching Anvil's separation between its
backend snapshot and pool.

## Implementation waves

### Wave 0: workspace and contracts

1. Create the four-crate Cargo workspace.
2. Centralize and pin compatible Rust, Alloy, ref-FVM, Foundry, and actor-bundle identities.
3. Establish the one-way dependency graph.
4. Define the initial node-domain DTOs, typed errors, and `NodeHandle` seam.
5. Verify metadata and reject forbidden production dependencies.

Gate: workspace metadata resolves, the dependency graph has no cycle or forbidden edge, and all
public boundary types have one owner.

### Wave 1: parallel foundations

#### Ethereum bridge

Implement EIP-1559 decoding, signature/chain-ID validation, sender recovery, f4 mapping,
EAM/EVM routing, delegated signature construction, exact `SignedMessage` construction, and exact
serialized length. Adapt fixed Forest vectors with attribution.

Gate: fixed raw transactions produce the expected sender, Ethereum hash, Filecoin message fields,
signature bytes, message CID, and serialized length.

#### Runtime and genesis nucleus

Implement the worker, verified actor CAR import, manifest validation, minimal f00/f01/f02/f010/f099
state, one prefunded placeholder EOA, deterministic externs, reusable engine pool, and ephemeral
machine/executor construction.

Gate: an explicit message runs on the real NV28/v18 runtime and exercises placeholder promotion and
fee handling.

#### RPC protocol foundation

Implement local request parsing, aliases, JSON-RPC envelope use, the generic HTTP router adapter,
initial read-only node dispatch, and typed error mapping.

Gate: focused tests cover empty, omitted, positional, alias, invalid-params, notification, and batch
forms without using Anvil's backend.

### Wave 2: executable transaction slice

1. Integrate raw transaction submission through the bridge.
2. Add state-dependent nonce, balance, and fee validation.
3. Add a concrete pending/ready pool and instant/manual selection.
4. Implement non-publishing calls and FVM gas estimation.
5. Implement atomic block commit and synthetic indexes.
6. Convert EAM returns and EVM events.
7. Implement the standard RPC methods required by `forge create` and `cast`.
8. Implement `filecoin_getGasBreakdown`.
9. Wire CLI configuration, startup, readiness, transport, and shutdown.

Gate: launch the actual binary, deploy through EAM, call and transact through EVM, inspect the
block/transaction/receipt/log and Filecoin gas data, and prove a historical simulation does not
publish a root.

### Wave 3: snapshots and Anvil controls

1. Implement snapshot/revert and canonical suffix pruning.
2. Revalidate the pool after revert.
3. Implement manual mining and automine controls with exact Anvil aliases and response shapes.
4. Adapt relevant Foundry integration cases through public JSON-RPC.
5. Run the combined portable public-JSON-RPC acceptance runner, including
   `filecoin_getGasBreakdown` and snapshot canonical-index pruning assertions.

Gate: the combined acceptance runner proves snapshot/revert restores nonce, bytecode, receipt
visibility, state, and block height, while also covering Filecoin gas breakdown and canonical-index
pruning.

### Wave 4: broader development controls

Delivered: deterministic genesis accounts and funding, genesis block/fee/gas configuration,
manual/interval/mixed startup mining, Anvil CLI aliases, server controls, startup output, and
`--config-out`. Remaining work covers time controls, fee/FIFO ordering, pool inspection/removal,
and balance/nonce controls. Each remaining behavior receives a public-boundary compatibility case
and real endpoint smoke test.

### Wave 5: versioned direct EVM state controls

Implement a private v18 actor-state adapter, then `anvil_setCode`, `anvil_setStorageAt`, ERC-20
balance probing, and allowance probing in that dependency order. Every state-schema test names the
actor bundle and network version explicitly. Local bundles are trusted as development inputs; the
user is responsible for selecting one compatible with the active state adapter.

## Delegation and supervision

Before each wave, the supervisor fixes shared interfaces, file ownership, and acceptance criteria in
the delegation context. Independent tasks are dispatched together; the central `NodeRuntime`, root
adoption, and canonical indexes have one implementation owner per wave. Agents do not add crates,
shared abstractions, dependency versions, or alternate DTOs without an explicit contract change.

Agents skip workspace-wide format, lint, and test commands. At each barrier the supervisor:

1. Verifies changed APIs and dependency direction.
2. Resolves mismatches at the owning boundary without shims.
3. Exercises the real runtime or endpoint first.
4. Runs focused package checks.
5. Requests a review of ownership leaks, root-publication bugs, panic paths, arithmetic, and forbidden
   dependencies.
6. Runs workspace formatting, Clippy, and tests after the milestone smoke test succeeds.

The supervisor retains ownership of root manifests, dependency versions, the public `NodeHandle`
contract, `NodeRuntime` invariants, cross-crate integration, and final acceptance evidence.
