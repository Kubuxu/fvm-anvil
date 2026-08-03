# fvm-anvil design decisions and findings

Status: living design record  
Last updated: 2026-08-03

## Purpose

`fvm-anvil` is a local development node for Filecoin's EVM actor. It should expose a familiar Anvil-compatible Ethereum JSON-RPC interface while executing contracts with `ref-fvm` and the builtin EAM, EVM, and EthAccount actors.

The primary reason for the tool is gas fidelity. Filecoin gas accounts for Wasm execution, IPLD access, storage, syscalls, retained memory, and other FVM costs. Running the same bytecode in `revm` does not answer how much gas it consumes on Filecoin.

The tool must make repeated contract deployment, execution, testing through RPC, and benchmarking fast without mining infrastructure or real block-time delays.

## Agreed scope

Initial product scope:

- Ethereum JSON-RPC for `cast`, Alloy clients, contract deployment, calls, transactions, receipts, logs, and benchmarks.
- Execution by `ref-fvm`, not `revm`.
- Official builtin actor bundle by default, with an alternate bundle selectable during initialization.
- Minimal Filecoin actor state needed for EAM/EVM execution and gas accounting.
- Anvil-compatible transaction pooling, block production controls, time controls, snapshots, and reverts.
- Filecoin-specific gas breakdown and trace endpoints in a separate namespace.

Explicitly out of scope:

- A Forge test runner.
- Foundry cheatcode execution.
- Full Filecoin consensus, mining, cron, null rounds, actor migrations, proofs, or tipset competition.
- General native actor behavior beyond the actors mechanically required by FVM execution.

Chain forking is desired but parked. Its design is intentionally unresolved; no conclusion about the eventual implementation or upstream interface is recorded here.

## Versions and default environment

The checked-out sources currently provide:

- `ref-fvm` 4.8.2.
- Builtin actors 18.0.0.
- Forest configuration for network version 28 with actor bundle v18.

Proposed defaults:

- Network version 28.
- Official mainnet actor bundle v18, pinned by URL and expected manifest CID.
- Local chain ID 31337, configurable at initialization and through the compatible runtime method.
- Filecoin block gas limit of 10,000,000,000.

An alternate `--actor-bundle <path>` is a genesis-time choice. Startup must load the manifest, verify all required actor entries, and reject an incompatible bundle. Near-term direct EVM state mutation assumes the v18 EVM actor state schema; alternate bundles must be schema-compatible unless a versioned state adapter is provided.

## Architecture

The node is a new runtime built directly on `ref-fvm`. Anvil is the RPC and behavioral reference, while Forest is the Filecoin Ethereum-RPC semantics reference.

A dedicated worker thread with a 64 MiB stack owns all mutable execution state:

- `EnginePool` and compiled actor cache.
- IPLD blockstore.
- Current FVM state root.
- Epoch, timestamp, base fee, and deterministic externs.
- Synthetic blocks and historical state roots.
- Transactions, receipts, logs, and gas traces.
- Transaction pool.
- Mining mode.
- Snapshots and one-shot next-block overrides.

Async RPC handlers communicate with this worker through semantic commands. RPC parsing must not leak into the execution layer.

Each produced block constructs a `DefaultMachine` from the current root and block context, executes selected messages sequentially through `DefaultExecutor`, records each `ApplyRet`, flushes once, and stores the resulting root with the synthetic block metadata. `EnginePool` is reused across machines.

Calls and gas estimation start from a selected historical root, execute without publishing the resulting root, and retain only their return value or diagnostic information. Content-addressed blocks written by discarded simulations are harmless because their roots are not adopted.

## Code and dependency boundaries

Proposed workspace:

```text
crates/
  fvm-anvil/          CLI and process startup
  fvm-anvil-rpc/      JSON-RPC parsing, aliases, handlers, responses
  fvm-anvil-node/     worker, pool, mining, history, snapshots
  fvm-anvil-eth/      pure Ethereum-to-Filecoin bridge
```

Dependency rules:

- `fvm-anvil-rpc` depends on the node handle, Alloy RPC types, `anvil-server`, and `anvil-rpc`; it does not depend on FVM internals.
- `fvm-anvil-eth` depends on Alloy plus `fvm_shared` and IPLD encoding; it is deterministic and testable without Wasmtime.
- `fvm-anvil-node` is the only owner of mutable chain state and the `ref-fvm` runtime.
- No production crate depends on Forest, `anvil-core`, or the main `anvil` crate.

### Reuse directly

From Foundry:

- `extern/foundry/crates/anvil/server` for HTTP, WebSocket, optional IPC, batching, CORS, and generic RPC handler plumbing.
- `extern/foundry/crates/anvil/rpc` for JSON-RPC requests, responses, and errors.
- Alloy Ethereum transaction and RPC types.

From `ref-fvm`:

- `DefaultMachine`, `NetworkConfig`, and `MachineContext`.
- `EnginePool`.
- `DefaultExecutor`, `Executor`, `ApplyKind`, and `ApplyRet`.
- `DefaultFilecoinKernel` and `DefaultCallManager`.
- FVM blockstore, CAR, encoding, gas, and shared crates.

### Adapt with attribution and compatibility tests

From Foundry:

- RPC method names, aliases, parameter formats, and request deserialization tests from `anvil-core`.
- Mining, pool, timestamp, receipt, log, and snapshot behavior from Anvil integration tests.

From Forest:

- FIP-0091 legacy and EIP-1559 transaction conversion.
- Delegated signature encoding.
- Ethereum sender and f4 address mapping.
- EAM creation versus EVM invocation mapping.
- Exact Filecoin `SignedMessage` construction and serialized length.
- Receipt and contract-creation return conversion.
- EVM event conversion from FVM events.
- FVM gas-estimation search behavior.

From the ref-FVM integration harness:

- Actor CAR import.
- Manifest loading.
- Minimal system, init, EAM, burnt-funds, and delegated placeholder initialization.

The production runtime must not reuse the integration harness's custom kernel, `DummyExterns`, test defaults, or abbreviated message-length calculations.

### Implement locally

- FVM worker and command protocol.
- Synthetic block and historical state indexes.
- Transaction pool.
- Mining scheduler.
- Snapshot metadata.
- Deterministic randomness and synthetic tipset CIDs.
- FVM-specific RPC diagnostics.

## Minimal actor state

The initial state tree contains:

| ID | State | Reason |
| --- | --- | --- |
| f00 | System actor | Holds the builtin manifest used to resolve actor code. |
| f01 | Init actor | Resolves and registers actor and delegated addresses. |
| f02 | Balance-bearing fee recipient sink | `DefaultExecutor` transfers miner tips to actor ID 2. Full Reward actor behavior is out of scope. |
| f010 | EAM actor | Creates EVM and EthAccount actors. |
| f099 | Burnt-funds account actor | Receives base-fee and overestimation burns. |

Development EOAs are prefunded f4 addresses in EAM namespace 10 backed by placeholder actor code. On their first explicit message, ref-FVM promotes the placeholder to EthAccount code.

The f02 sink is mechanically necessary even with native actor behavior parked: ordinary EIP-1559 transactions may have a nonzero premium, and `DefaultExecutor` deposits that miner tip into actor ID 2. Calls to f02 and real Reward actor semantics remain unsupported.

Cron and all other native singleton actors are omitted initially.

## Ethereum transaction bridge

The bridge accepts the transaction formats supported by Filecoin's current Ethereum transaction rules, initially legacy/EIP-155 and EIP-1559 with the applicable network-version restrictions.

For every raw transaction it must:

1. Decode the original Ethereum envelope.
2. Validate chain ID and signature.
3. Recover the Ethereum sender.
4. Convert the sender to its EAM f4 address.
5. Map creation to EAM `CreateExternal` or a call to EVM `InvokeContract`.
6. Map Ethereum nonce to Filecoin sequence.
7. Map value, gas limit, fee cap, and premium.
8. CBOR-encode EVM calldata.
9. Construct the exact Filecoin delegated signature and `SignedMessage`.
10. Use the serialized Filecoin message length for inclusion gas.
11. Submit the unsigned message to `DefaultExecutor` with `ApplyKind::Explicit`.

FVM does not validate the original Ethereum envelope for this path; signature and chain-ID validation are the RPC boundary's responsibility.

## Gas semantics and diagnostics

Ethereum RPC gas quantities deliberately represent Filecoin gas:

- Transaction gas is the Filecoin message gas limit.
- Receipt `gasUsed` is the FVM message receipt gas used.
- Cumulative gas is accumulated across messages in the synthetic block.
- Effective gas price is derived from Filecoin fee outputs.
- Gas estimation executes against FVM and searches for a successful Filecoin gas limit.

FEVM contract context also follows Filecoin behavior:

- `GAS` returns FVM gas remaining.
- `GASLIMIT` returns 10,000,000,000.
- `BASEFEE` returns the Filecoin base fee.
- `BLOCKNUMBER` returns the Filecoin epoch.
- `TIMESTAMP` returns the synthetic tipset timestamp.
- `COINBASE` returns zero.
- `BLOCKHASH` is derived from a synthetic tipset CID.

Standard receipts cannot expose the full model. Separate Filecoin endpoints should expose `ApplyRet` fields and `ExecutionEvent::GasCharge` entries, including compute gas, other gas, burns, refund, tip, penalty, failure information, and optional elapsed time. Candidate methods include:

- `filecoin_getGasBreakdown`
- `filecoin_getGasTrace`
- `filecoin_getMessageCid`
- `filecoin_getStateRoot`
- `filecoin_getActor`

The exact names remain to be fixed in the RPC schema.

Opcode-level Geth/Foundry traces are unavailable because the builtin EVM interpreter does not currently expose opcode tracing. FVM execution and gas-charge traces remain available.

## Anvil RPC compatibility

The canonical snapshot methods are:

- `anvil_snapshot`, alias `evm_snapshot`.
- `anvil_revert`, alias `evm_revert`.

A revert consumes its snapshot ID. It restores the FVM root, synthetic head, epoch, timestamp, base fee, transaction/receipt/log indexes, and pending one-shot block overrides. Blocks and indexes newer than the snapshot are removed.

The mining calls have distinct parameter and response contracts:

- `anvil_mine` mines an optional number of blocks with an optional per-call timestamp interval.
- `evm_mine` accepts its Ganache-compatible timestamp/options form, mines regardless of mode, and returns `"0x0"`.
- `anvil_mine_detailed`, alias `evm_mine_detailed`, returns detailed block data.

### Block production modes

Match Anvil's four modes:

| Mode | Startup configuration | Behavior |
| --- | --- | --- |
| Instant | default | Mine when ready transactions arrive. |
| Interval | `--block-time <seconds>` | Mine on the timer, including empty blocks. |
| Mixed | `--block-time <seconds> --mixed-mining` | Mine on transaction arrival and timer. |
| Manual | `--no-mining` | Keep transactions pending until explicitly mined. |

Also support `--max-transactions` and `--order fees|fifo`.

Runtime transitions follow Anvil:

- `anvil_setAutomine(true)`, alias `evm_setAutomine`, replaces any mode with instant mode.
- Setting automine false changes pure instant mode to manual and is a no-op in interval or mixed mode.
- `anvil_setIntervalMining(n > 0)`, alias `evm_setIntervalMining`, replaces the current mode with interval mode.
- Setting the interval to zero selects manual mode.
- Runtime methods do not restore mixed mode.
- `anvil_getAutomine` is true only in pure instant mode.
- `anvil_getIntervalMining` reports only pure interval mode.

## Near-term direct state mutation methods

The following methods are required soon after the first end-to-end execution slice, not deferred indefinitely:

- `anvil_setCode`
- `anvil_setStorageAt`
- `anvil_dealERC20`
- `anvil_setERC20Allowance`

They mutate the current development state without charging gas or mining a block, matching Anvil's development-control semantics. The mutations produce a new current FVM state root and participate in subsequent snapshots and blocks.

### `anvil_setCode`

An EVM actor state contains:

- Raw bytecode CID.
- Keccak-256 bytecode hash.
- Contract storage KAMT root.
- Transient state metadata.
- EVM creation nonce.
- Optional tombstone.

Implementation requirements:

1. Resolve the Ethereum address to an actor ID.
2. If necessary, create or convert the address into an EVM actor while preserving its balance where meaningful.
3. Store bytecode as an `IPLD_RAW` block addressed with Blake2b-256, matching the EVM actor.
4. Compute and store the Keccak-256 bytecode hash.
5. Preserve contract storage and EVM nonce for an existing live contract.
6. Clear any tombstone so the installed code is observable and executable.
7. Flush the actor state and state tree atomically.

Open compatibility detail: Anvil permits development-time code injection, while normal FEVM deployment enforces maximum code size and EIP-3541. We must decide whether the control method deliberately bypasses those deployment checks or preserves FEVM invariants. This decision needs an explicit test either way.

### `anvil_setStorageAt`

Builtin actor v18 stores contract storage as `KAMT<U256, U256>` with the EVM actor's exact KAMT configuration. Setting zero deletes the key; a nonzero value inserts or replaces it.

Implementation requirements:

1. Resolve or create an empty-code EVM actor at the target Ethereum address.
2. Load the actor's storage KAMT with the version-compatible configuration.
3. Convert the RPC slot and 32-byte value as big-endian `U256` values.
4. Delete zero values or set nonzero values.
5. Flush the KAMT, update the actor state's `contract_state` CID, and flush the state tree atomically.
6. Preserve bytecode, nonce, transient metadata, tombstone state, balance, and delegated address unless actor creation is required.

This code must be isolated behind a versioned EVM state adapter because an arbitrary alternate actor bundle may use a different state schema or KAMT configuration.

### `anvil_dealERC20`

The method sets an absolute `balanceOf(address)` value by discovering the token's balance storage slot and then using `anvil_setStorageAt`.

Current Anvil discovers candidate slots from an EVM access list and tests state overrides. FEVM does not currently expose logical EVM storage-read/access-list tracing, so the initial FVM implementation should use bounded storage-layout probing:

1. ABI-encode `balanceOf(target)`.
2. Try common Solidity and Vyper mapping layouts across a bounded range of base slots.
3. For each candidate, create a temporary state root with that slot set to the requested balance.
4. Execute `balanceOf` through `eth_call` against the temporary root.
5. Select the candidate only when the returned ABI value equals the requested balance.
6. Commit the discovered slot through `anvil_setStorageAt`.
7. Call `balanceOf` again against committed state and verify the result.
8. Cache the discovered layout keyed by token address and bytecode hash.

Return an explicit `no slot found` error for nonstandard accounting or when the bounded probe cannot prove a candidate.

### `anvil_setERC20Allowance`

This follows the same proof-by-simulation strategy using `allowance(owner, spender)`. Candidate keys include common nested mapping layouts such as:

```text
keccak256(spender, keccak256(owner, base_slot))
```

and the relevant alternative ordering used by supported compiler layouts.

The method must:

1. ABI-encode `allowance(owner, spender)`.
2. Probe bounded candidate nested-mapping layouts using temporary FVM roots.
3. Accept a candidate only when the call returns the requested amount.
4. Commit through `anvil_setStorageAt`.
5. Verify the committed result through `allowance`.
6. Cache the proven layout by token address and bytecode hash.

Both ERC-20 methods depend on correct, cheap root-based simulation and direct storage mutation; they should be implemented immediately after `anvil_setStorageAt`.

## Initial RPC surface

The first compatibility tier should include enough standard Ethereum RPC for `cast`, Alloy, and `forge create`:

- `web3_clientVersion`
- `net_version`
- `eth_chainId`
- `eth_accounts`
- `eth_syncing`
- `eth_blockNumber`
- `eth_getBlockByNumber`
- `eth_getBlockByHash`
- `eth_getBalance`
- `eth_getTransactionCount`
- `eth_getCode`
- `eth_getStorageAt`
- `eth_call`
- `eth_estimateGas`
- `eth_gasPrice`
- `eth_maxPriorityFeePerGas`
- `eth_feeHistory`
- `eth_sendRawTransaction`
- `eth_getTransactionByHash`
- `eth_getTransactionReceipt`
- `eth_getLogs`

Anvil controls in the early surface:

- Mining and time methods described above.
- `anvil_snapshot` and `anvil_revert` with aliases.
- `anvil_setBalance`, `anvil_addBalance`, and `anvil_setNonce`.
- `anvil_setChainId`.
- `anvil_setNextBlockBaseFeePerGas`.
- `anvil_setNextBlockPrevRandao`.
- `anvil_nodeInfo` and `anvil_metadata`.
- Pool inspection and removal methods.

FEVM fixes `GASLIMIT` at 10 billion and `COINBASE` at zero. `anvil_setBlockGasLimit` and `anvil_setCoinbase` cannot honestly affect contract-visible values and should return clear unsupported errors unless the actor behavior changes.

## Implementation milestones

### Milestone 1: end-to-end execution slice

- Workspace and dependency boundaries.
- Official v18 bundle loading at NV28.
- Minimal actors including f02 and f099.
- One prefunded standard Anvil f4 account.
- EIP-1559 raw transaction conversion.
- Dedicated FVM worker and reusable `EnginePool`.
- Contract deployment through EAM.
- Contract call and state-changing transaction through EVM.
- Synthetic block, transaction, and receipt indexes.
- Filecoin gas breakdown endpoint.
- Instant and manual mining.
- `anvil_snapshot` and `anvil_revert`.

Acceptance is a black-box flow using `cast` or `forge create`: deploy a contract, call it, transact with it, inspect the receipt and FVM gas breakdown, snapshot, mutate, revert, and verify restored state.

The first executable acceptance gate is `tests/acceptance/run.sh`. It is deliberately
endpoint-agnostic and runs unchanged against upstream Anvil or fvm-anvil:

```text
RPC_URL=http://127.0.0.1:8545 tests/acceptance/run.sh
```

The runner uses only public Foundry commands. It compiles a Paris-targeted Solidity fixture,
deploys it with an EIP-1559 transaction, checks code and constructor state, estimates and executes
a state-changing call, validates its receipt, event, transaction, and block, then proves nested
`anvil_snapshot`/`anvil_revert` behavior including nonce, receipt-index, bytecode, and state
restoration. An outer snapshot makes the test leave the target node unchanged.

This portable test does not call Filecoin-specific endpoints because it must also pass against
upstream Anvil. FVM gas-breakdown assertions are an additional fvm-anvil-only acceptance gate.

### Milestone 2: Anvil control parity needed for development

- Interval and mixed mining.
- Full time controls.
- Fee/FIFO transaction ordering and txpool inspection.
- `anvil_setBalance`, `anvil_addBalance`, and `anvil_setNonce`.
- `anvil_setCode`.
- `anvil_setStorageAt`.
- `anvil_dealERC20`.
- `anvil_setERC20Allowance`.
- Broader logs, filters, and historical queries.

The state mutation methods are ordered deliberately: code/storage primitives first, followed by ERC-20 helpers built on temporary-root simulation and storage writes.

### Later milestones

- State dump/load using CAR plus node metadata.
- WebSocket subscriptions and optional IPC if not included earlier.
- Broader Anvil extensions where they have honest FEVM semantics.
- Chain forking after its design is agreed.

## Verification evidence from investigation

The existing ref-FVM FEVM gas test was run directly:

```text
cargo test -p fvm_integration_tests --test gasfuzz test_gasfuzz -- --nocapture
```

Result:

```text
1 passed; 0 failed
finished in 40.06s
```

The test deploys a recursive EVM contract through EAM, executes it through ref-FVM, collects FVM gas charge points, and checks out-of-gas boundaries. It proves the direct executor path; the first implementation slice must repeat the proof with the selected v18 bundle, NV28, and exact signed Filecoin message lengths.

## Primary references in `extern`

Foundry:

- `extern/foundry/crates/anvil/server`
- `extern/foundry/crates/anvil/rpc`
- `extern/foundry/crates/anvil/core/src/eth/mod.rs`
- `extern/foundry/crates/anvil/src/eth/api.rs`
- `extern/foundry/crates/anvil/src/eth/miner.rs`
- `extern/foundry/crates/anvil/src/eth/pool`
- `extern/foundry/crates/anvil/tests/it`

Forest:

- `extern/forest/src/eth`
- `extern/forest/src/rpc/methods/eth.rs`
- `extern/forest/src/interpreter/vm.rs`
- `extern/forest/src/networks/actors_bundle.rs`

ref-FVM and actors:

- `extern/ref-fvm/fvm/src/executor`
- `extern/ref-fvm/fvm/src/machine`
- `extern/ref-fvm/fvm/src/engine`
- `extern/ref-fvm/testing/integration/src`
- `extern/builtin-actors/actors/evm/src/state.rs`
- `extern/builtin-actors/actors/evm/src/interpreter/system.rs`

External Anvil documentation:

- <https://www.getfoundry.sh/anvil/rpc-methods>
- <https://www.getfoundry.sh/anvil/mining>
- <https://www.getfoundry.sh/anvil/state-management>
