# Curio PDP compatibility

Status: verified compatibility record  
Last updated: 2026-11-09

This reference records native wire contracts, scope limits, tested revisions, and dated
verification evidence. Use the [Curio/FWSS guide](../tests/curio/README.md) for executable
setup, deployment, direct RPC examples, and repeat runs. Current runtime ownership and
Ethereum behavior are defined in [design.md](design.md), not in a separate implementation plan.

## Supported scope

Curio's PDP-only Skiff binary uses Ethereum transactions against a fresh local FWSS stack,
with native Filecoin head/history/beacon reads for scheduling and proving. The verified
workflow includes provider/payment setup, dataset creation, piece ingestion, and successful
real `provePossession`. PostgreSQL stores relational state; ScyllaDB provides the CQL proof cache.

Native miners, PoRep/sealing, PoSt, consensus, cron, native wallet/message submission, and
general Lotus FullNode compatibility are not supported. Curio signs PDP transactions locally;
registration of an idle native task does not imply support for its APIs. Ethereum
`eth_subscribe` is not implemented.

## Verified source versions

| Component | Revision | Runtime version |
| --- | --- | --- |
| Curio | `f5a416f633d82d293e278e7b1bb4ca135e0b6e16` | Skiff 2k build |
| Filecoin Services | `305a44723c1f0e612e294f51f5fccfc5a4812a62` | FWSS 1.5.0; registry 1.2.0 |
| Pinned PDP submodule | `cc3f5eaffee7df80471b671a4e35a42b000685b8` | PDPVerifier 3.5.0 |

The deployer builds the selected checkout and records its revisions; it does not fetch a
moving remote release. Compatibility with arbitrary future versions is not established.
FWSS 1.5.0 uses an ERC-8167 dispatcher/facet graph; its view address is the FWSS proxy.
This is distinct from the historical monolithic FWSS benchmark deployment.

The default local payment asset is official Solady WETH backed by native FIL, **not USDFC
or a stablecoin**. An existing real 18-decimal token can be selected explicitly. Deployment,
funding, and indexing configuration belong to the [runnable guide](../tests/curio/README.md).

## Implemented API boundary

Ethereum and supported native `Filecoin.*` methods share HTTP/WebSocket `/rpc/v1`.
`ChainNotify` requires WebSocket and Go `go-jsonrpc` channel framing, not Ethereum
subscription framing. Successful registration sends one initial `current`; subsequent
committed changes publish `apply`, and snapshot rollback publishes `revert` changes
followed by the restored head. Cancellation and reconnect are supported.

| Native method | Positional parameters | Result |
| --- | --- | --- |
| `Filecoin.StateNetworkName` | `[]` | `local-fvm-anvil`. |
| `Filecoin.StateNetworkVersion` | `[tipsetKey]` | Configured network version after validating the selected tipset. |
| `Filecoin.ChainHead` | `[]` | Canonical synthetic Lotus-decodable tipset. |
| `Filecoin.ChainGetTipSet` | `[tipsetKey]` | Canonical local tipset selected by key. |
| `Filecoin.ChainGetTipSetByHeight` | `[height, tipsetKey]` | Local history relative to the selected anchor. |
| `Filecoin.StateGetRandomnessDigestFromBeacon` | `[epoch, tipsetKey]` | Raw 32-byte beacon digest, base64-encoded in JSON. |
| `Filecoin.ChainNotify` | `[]` | Go channel of head-change batches; WebSocket only. |

An empty key is `[]` (`null` is also accepted for the key); populated keys use Lotus CID
objects: `[{"/":"CID"}]`. Heights/epochs are JSON integers, not Ethereum hex quantities.
Unknown keys, negative epochs, epochs above the selected head, and unavailable pre-import
history fail explicitly. The node does not fabricate imported chain history.

Headers are synthetic projections, not consensus-valid miner blocks. Header CIDs hash
canonical Lotus CBOR; the key CID hashes the CBOR byte string of concatenated header CID
bytes. The FVM chain extern uses the same key CID. Beacon RPC and ref-FVM share the same
deterministic raw digest source, verified through the real FIP-0095 precompile. Other
Lotus randomness and native object-store methods are not implied by this boundary.

## Ethereum and operational constraints

- `net_version`, `eth_chainId`, and transaction validation agree; Curio uses `net_version`
  as its signing chain ID. Pending replacement follows the [documented fee and reservation
  policy](design.md#implemented-rpc-compatibility).
- The pinned Go Ethereum client decodes full empty/nonempty blocks, receipts, and event logs.
  Curio derives dataset/piece IDs from those events and needs an additional epoch after
  transaction inclusion. Proof sends are checked through actual receipts, not an assumed
  `message_waits_eth` row.
- Empty interval blocks advance confidence and proof schedules. A realistic genesis timestamp
  is required by the recipe; generic Anvil defaults remain unchanged.
- The [database/indexing profile](../tests/curio/README.md#database-networking-and-indexing-behavior)
  disables advertisements but retains proof caching. In particular, the `withIPFSIndexing`
  metadata key must be absent; a present key with value `"false"` opts in.
- Node state is in memory. A restarted node needs fresh contracts and clean dedicated Curio
  databases; an old deployment manifest does not restore state.

## Exercised verification

The pinned implementation run on 2026-11-09 passed a release build, strict Clippy for the
affected crates, 44 node/RPC tests with an explicit official v18 bundle (including ignored
runtime/transport tests), public Ethereum acceptance, and a real pending-replacement scenario.
The pinned Go clients exercised native head decoding, full Ethereum bodies/receipts/logs,
and `current/apply/revert/cancel/reconnect`.

Fresh deployment completed 26 successful FVM receipts with the versions listed above.
Writable Skiff created a dataset, uploaded/added a 64 MiB piece, and submitted successful
proof `0x8275b7a3f3e269fe9123807be3ad357e6012566ecb943ffe643e1f1231bd838d`.
The run retained 32 ScyllaDB proof-cache layer rows and no advertisement intents/tasks/records.

These are historical results, not assertions about every future build. The proof hash is a
local-run identifier, not a public-network transaction. Run artifacts are local and ignored;
the [guide](../tests/curio/README.md#5-run-writable-pdp-acceptance) explains how to produce and
inspect new results without depending on the original session's files or processes.
