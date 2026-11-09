# fvm-anvil roadmap

Status: remaining work  
Last updated: 2026-11-09

Current architecture, ownership, supported methods, and behavior are authoritative in
[`design.md`](design.md). This roadmap contains unresolved work, not a second implementation
inventory. Curio PDP support is complete for the [tested compatibility boundary](curio-pdp-compatibility.md);
its [runnable guide](../tests/curio/README.md) covers fresh FWSS deployment and proving.

## Broader development controls

Add time controls, fee/FIFO transaction ordering, and pool inspection/removal where FEVM can
provide the requested behavior. Each method needs exact Foundry wire-format comparison, a
public JSON-RPC compatibility case, and an actual endpoint smoke test.

Runtime mining controls must have one timer owner. A control change takes ownership away
from the startup scheduler; snapshot/revert must not restore process-control state.

## General ERC-20 layout discovery

Standard Solidity mapping layouts are already supported by the balance/allowance helpers.
Nonstandard, computed, packed, and out-of-range layouts remain unsupported. Broader discovery
needs storage-access tracing or equivalent contract-visible evidence before adopting a write.
Do not replace a failed lookup with an unvalidated state mutation.

## Prepared-state persistence

Persist the writable overlay and adopted root without copying or mutating the immutable
prepared ForestCAR. Restart must recover the same current root and canonical projections or
fail clearly; partial publication must not be accepted. Keep process controls distinct from
chain-visible snapshots.

See the [prepared-state reference](press.md) for the current nonpersistent import boundary.

## Deferred extensions and scope limits

- Ethereum subscriptions (`eth_subscribe`) and optional IPC.
- Broader Anvil extensions where FEVM can provide honest semantics.
- General remote-chain forking.
- New imported-actor synthesis and workload-specific state witnesses.

Native miners, PoRep/PoSt, consensus, and general Lotus API behavior are outside the current
PDP development scope, not prerequisites for using the completed Curio workflow.

## Completion guidance

For runtime/RPC changes, exercise the real binary and changed endpoint before relying on
focused tests. [`tests/acceptance/run.sh`](../tests/acceptance/run.sh) is the public Ethereum
workflow for deployment, transactions, gas, events, state controls, and snapshots. Run broader
formatting, strict Clippy, and test checks after the endpoint smoke succeeds.

For Curio/PDP changes, use the [writable workflow](../tests/curio/README.md). A ping or contract
view alone does not prove ingestion, raw beacon agreement, populated proof caching, successful
proof submission, or notification/reorg handling. Actor-state-dependent tests must name their
network version and actor bundle explicitly.
