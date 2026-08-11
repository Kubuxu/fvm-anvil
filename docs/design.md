# fvm-anvil design and behavior

Status: living design record  
Last updated: 2026-08-05

Current implementation status and remaining work are tracked in
[`docs/implementation-plan.md`](implementation-plan.md).

## Purpose

`fvm-anvil` is a local development node for Filecoin's EVM actor. It should expose a familiar Anvil-compatible Ethereum JSON-RPC interface while executing contracts with `ref-fvm` and the builtin EAM, EVM, and EthAccount actors.

The primary reason for the tool is gas fidelity. Filecoin gas accounts for Wasm execution, IPLD access, storage, syscalls, retained memory, and other FVM costs. Running the same bytecode in `revm` does not answer how much gas it consumes on Filecoin.

The tool must make repeated contract deployment, execution, testing through RPC, and benchmarking fast without mining infrastructure or real block-time delays.

## Agreed scope

Product scope:

- Ethereum JSON-RPC for `cast`, Alloy clients, contract deployment, calls, transactions, receipts,
  logs, and gas measurement.
- Execution by `ref-fvm`, not `revm`.
- Official builtin actor bundle by default, with an alternate v18 bundle selectable at startup.
- Minimal Filecoin actor state needed for EAM/EVM execution and gas accounting.
- Anvil-compatible transaction pooling, block production, snapshots, reverts, and development
  controls where FEVM can implement them honestly.
- Filecoin-specific gas breakdown and trace endpoints in a separate namespace.

Explicitly out of scope:

- A Forge test runner.
- Foundry cheatcode execution.
- Full Filecoin consensus, mining, cron, null rounds, actor migrations, proofs, or tipset competition.
- General native actor behavior beyond the actors mechanically required by FVM execution.

General remote chain forking remains parked. Offline preparation and import of selected mainnet
actor state are implemented through [`press` and the prepared-state boundary](press.md); this does
not import consensus history or define remote RPC forking behavior.

## Versions and default environment

The checked-out sources currently provide:

- `ref-fvm` 4.8.2.
- Builtin actors 18.0.0.
- Forest configuration for network version 28 with actor bundle v18.

Defaults:

- Network version 28.
- Official mainnet actor bundle v18, pinned by URL and expected manifest CID. The CLI caches a
  verified download under `$XDG_CACHE_HOME/fvm-anvil` (or `$HOME/.cache/fvm-anvil`) using the
  expected root in the filename; an invalid cache entry is replaced from the pinned URL.
- Local chain ID 31337.
- Ten deterministic development accounts derived from
  `test test test test test test test test test test test junk` at
  `m/44'/60'/0'/0/<index>`, each funded with 10,000 FIL.
- Synthetic genesis block number and timestamp zero.
- Filecoin block gas limit 10,000,000,000 and initial base fee 100 attoFIL per gas.

`--state <PRESS_ARCHIVE>` replaces the empty genesis state and its number, timestamp, and base fee
with the prepared archive's recorded execution context. The local chain ID and block gas limit
remain node configuration. Generated development accounts are funded in a writable overlay without
mutating the prepared archive.

Chain ID, generated accounts, balances, mnemonic and derivation path, additional funded accounts,
block gas limit, and the selected empty or prepared initial state are immutable genesis-time
configuration. Explicit `--number`, `--timestamp`, and `--base-fee` values conflict with `--state`.

An alternate `--actor-bundle <path>` is a genesis-time choice. Startup trusts the CAR's declared
root, then loads its manifest and verifies the actor entries required for execution. The user is
responsible for supplying a bundle compatible with the v18 EVM actor state schema assumed by the
runtime.

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
- Snapshots.

Async RPC handlers communicate with this worker through semantic commands. RPC parsing must not leak into the execution layer.

Each produced block constructs a `DefaultMachine` from the current root and block context, executes selected messages sequentially through `DefaultExecutor`, records each `ApplyRet`, flushes once, and stores the resulting root with the synthetic block metadata. `EnginePool` is reused across machines.

Startup eagerly compiles only the EAM, EthAccount, EVM, and placeholder actors used by the Ethereum
transaction path. Other bundle actors compile lazily if a future feature invokes them; compiling
every v18 actor delays readiness without benefiting normal FEVM execution.
`ref-fvm`'s `testing` feature disables `DefaultExecutor`'s unconditional full-manifest preload. It
also grants actor ID 98 a test-only create-actor permission. Default genesis starts allocation at ID
100, and prepared-state construction and validation reject actor ID 98.


Calls and gas estimation start from a selected historical root, execute without publishing the resulting root, and retain only their return value or diagnostic information. Content-addressed blocks written by discarded simulations are harmless because their roots are not adopted.

## Code and dependency boundaries

The workspace has six crates:

```text
press                     offline snapshot selection, patching, and prepared archive output
  └── fvm-anvil-state     ForestCAR and prepared-state formats

fvm-anvil                 CLI, configuration, process lifetime, and graceful shutdown
  ├── fvm-anvil-rpc       JSON-RPC parsing, handlers, responses, and transport
  │     └── fvm-anvil-node
  └── fvm-anvil-node      mutable execution and synthetic chain state
        ├── fvm-anvil-eth Ethereum-to-Filecoin protocol bridge
        └── fvm-anvil-state
```

Dependency rules:

- `fvm-anvil-rpc` depends on the node handle, Alloy RPC types, `anvil-server`, and `anvil-rpc`; it
  does not depend on FVM internals.
- `fvm-anvil-eth` depends on Alloy plus `fvm_shared` and IPLD encoding; it is deterministic and
  testable without Wasmtime.
- `fvm-anvil-node` is the only owner of mutable chain state and the `ref-fvm` runtime.
- `fvm-anvil-state` owns the on-disk prepared-state and ForestCAR boundary plus the versioned
  builtin-actors state adapters shared by `press` and the node. It does not own a live root.
- No production crate depends on Forest, `anvil-core`, or the main `anvil` crate.

### Reuse directly

From Foundry:

- `extern/foundry/crates/anvil/server` for HTTP, batching, CORS, and generic RPC handler plumbing.
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

The empty-state genesis tree contains:

| ID | State | Reason |
| --- | --- | --- |
| f00 | System actor | Holds the builtin manifest used to resolve actor code. |
| f01 | Init actor | Resolves and registers actor and delegated addresses. |
| f02 | Balance-bearing fee recipient sink | `DefaultExecutor` transfers miner tips to actor ID 2. Full Reward actor behavior is out of scope. |
| f010 | EAM actor | Creates EVM and EthAccount actors. |
| f099 | Burnt-funds account actor | Receives base-fee and overestimation burns. |

Development EOAs are prefunded f4 addresses in EAM namespace 10 backed by placeholder actor code. On their first explicit message, ref-FVM promotes the placeholder to EthAccount code.

The f02 sink is mechanically necessary even with native actor behavior parked: ordinary EIP-1559 transactions may have a nonzero premium, and `DefaultExecutor` deposits that miner tip into actor ID 2. Calls to f02 and real Reward actor semantics remain unsupported.

Cron and all other native singleton actors are omitted from empty-state genesis.

## Ethereum transaction bridge

The bridge accepts the transaction formats supported by Filecoin's current Ethereum transaction
rules: legacy/EIP-155 and EIP-1559 with the applicable network-version restrictions.

For every raw transaction it:

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
- Receipt `effectiveGasPrice` follows EIP-1559:
  `min(maxFeePerGas, baseFeePerGas + maxPriorityFeePerGas)`. Exact Filecoin fee outputs remain
  available through `filecoin_getGasBreakdown`.
- Gas estimation executes against FVM and searches for a successful Filecoin gas limit.

FEVM contract context also follows Filecoin behavior:

- `GAS` returns FVM gas remaining.
- `GASLIMIT` returns 10,000,000,000.
- `BASEFEE` returns the Filecoin base fee.
- `BLOCKNUMBER` returns the Filecoin epoch.
- `TIMESTAMP` returns the synthetic tipset timestamp.
- `COINBASE` returns zero.
- `BLOCKHASH` is derived from a synthetic tipset CID.

Standard receipts cannot expose the full model. Filecoin-specific diagnostics use separate RPC
methods:

- `filecoin_getGasBreakdown` returns the exact `ApplyRet` fee, burn, refund, and penalty fields for
  a committed Ethereum transaction hash.
- `filecoin_getGasTrace` returns a compact nested actor-call trace for a committed Ethereum
  transaction hash. Each call owns its ordered ref-FVM gas charges and subcalls. Charge values are
  exact milligas (`computeMilligas`, `otherMilligas`, and `totalMilligas`); the top-level `gasUsed`
  remains whole FVM gas. Timing, parameters, return bytes, invocation logs, and IPLD events are
  intentionally omitted so the stable JSON is suitable for external aggregation scripts.
- `filecoin_getStateRoots` returns the prepared and current roots.

These three method names and response contracts are fixed. Trace capture is enabled only for
canonical transaction execution; calls and gas-estimation simulations do not pay its overhead.

Opcode-level Geth/Foundry traces are unavailable because the builtin EVM interpreter does not currently expose opcode tracing. FVM execution and gas-charge traces remain available.

## Implemented RPC compatibility

The current standard Ethereum surface is:

- `web3_clientVersion`, `net_version` (`eth_networkId`), `eth_chainId`, `eth_accounts`
  (`eth_requestAccounts`), `eth_syncing`, `eth_mining`, and `eth_blockNumber`;
- `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`, and `eth_getStorageAt`;
- `eth_call`, `eth_estimateGas`, and `eth_sendRawTransaction`;
- `eth_getTransactionByHash`, `eth_getTransactionReceipt`, `eth_getBlockByNumber`,
  `eth_getBlockByHash`, and `eth_getLogs`;
- `eth_gasPrice`, `eth_maxPriorityFeePerGas`, and `eth_feeHistory`.

The implemented Anvil controls are:

- `anvil_snapshot` (`evm_snapshot`) and `anvil_revert` (`evm_revert`);
- `anvil_mine` and `evm_mine`;
- `anvil_setAutomine` (`evm_setAutomine`);
- `anvil_setBalance` (`hardhat_setBalance`, `tenderly_setBalance`) and `anvil_addBalance`
  (`hardhat_addBalance`, `tenderly_addBalance`);
- `anvil_setNonce` (`hardhat_setNonce`, `evm_setAccountNonce`);
- `anvil_setCode` (`hardhat_setCode`);
- `anvil_setStorageAt` (`hardhat_setStorageAt`).

Methods outside this surface return JSON-RPC `method not found`; they must not report success without
an FEVM implementation.

A successful revert consumes the requested snapshot and every newer snapshot. It restores the FVM
root, synthetic blocks, epoch, timestamp, transaction and receipt indexes, and Filecoin gas indexes.
It retains pending transactions that remain valid against the restored state. Mining mode and the
startup interval scheduler are process controls and are not part of a snapshot.

`anvil_mine` accepts an optional block count and timestamp interval. `evm_mine` accepts its
Ganache-compatible timestamp/options form, mines regardless of automine, and returns `"0x0"`.

Direct account controls publish a new current FVM root without mining a synthetic block. `latest`
and `pending` reads and subsequent execution use that root; explicit block numbers and hashes retain
their historical roots. Snapshots capture direct mutations. Balance lives in `ActorState`; non-EVM
nonce lives in `ActorState.sequence`; EVM nonce, bytecode, bytecode hash, and storage use the shared
builtin-actors v18 adapter in `fvm-anvil-state`. Setting code or storage on an address without an EVM
actor creates an EVM actor at that delegated address while preserving its balance and
Ethereum-visible nonce. `anvil_addBalance` saturates at `U256::MAX`, matching Foundry.

### Block production modes

The CLI supports four startup modes:

| Mode | Startup configuration | Behavior |
| --- | --- | --- |
| Instant | default | Mine when a valid transaction arrives. |
| Interval | `--block-time <seconds>` | Mine on the timer, including empty blocks. |
| Mixed | `--block-time <seconds> --mixed-mining` | Mine on transaction arrival and on the timer. |
| Manual | `--no-mining` | Keep transactions pending until explicitly mined. |

Calling `anvil_setAutomine` or `evm_setAutomine` transfers control away from the startup interval
scheduler. Enabling automine mines a pending block immediately and then mines on submission;
disabling it leaves subsequent transactions pending.

Startup diagnostics, the account banner, and private development keys are written to stderr so RPC
process stdout remains clean. `--quiet` and its `--silent` alias suppress those diagnostics.
`--config-out <file>` writes wallet and fee fields plus a `filecoin` object containing the network
version, actor bundle version and root, prepared and current state roots, and startup mining mode.
On Unix the file is created or reset to mode `0600` because it contains private keys.

FEVM fixes contract-visible `GASLIMIT` at 10 billion and `COINBASE` at zero. Controls that cannot
honestly change contract-visible behavior must remain unsupported.

## Remaining compatibility work

The current roadmap is maintained in [`docs/implementation-plan.md`](implementation-plan.md).
Priorities include time controls, pool ordering and inspection, ERC-20 balance and allowance
helpers, and writable-overlay persistence. Foundry implements the ERC-20 helpers by collecting a
call's storage access list and retrying the call with one candidate state override at a time;
fvm-anvil needs honest FEVM access-list or storage-access tracing plus simulation state overrides
before it can reproduce that behavior.

`anvil_dumpState`, `anvil_loadState`, and `anvil_reset` also remain unsupported. Foundry's dump is a
gzip-compressed serialization of its Ethereum database and block environment, not an FVM state
format; fvm-anvil persistence must retain the writable IPLD overlay and adopted FVM root instead of
claiming wire compatibility with that blob.

WebSocket subscriptions, IPC, broader Anvil extensions, and general remote-chain forking remain
deferred. State preparation with selected mainnet actors is provided by
[`press`](press.md), but it is not consensus-history import or remote RPC forking.

## Acceptance runner

`tests/acceptance/run.sh` exercises the public JSON-RPC boundary and can run unchanged against
upstream Anvil or fvm-anvil:

```text
RPC_URL=http://127.0.0.1:8545 tests/acceptance/run.sh
```

It exercises direct account balance, nonce, code, and storage controls, then deploys a
Paris-targeted Solidity fixture, reads and mutates contract state, validates its transaction,
receipt, event, and block, and exercises nested snapshot/revert behavior. Set
`FILECOIN_ASSERTIONS=1` when targeting fvm-anvil to include Filecoin gas and canonical-index checks.

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
