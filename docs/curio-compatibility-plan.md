# Curio PDP compatibility: implementation record

Status: completed and verified through writable Curio PDP proving.  
Last updated: 2026-11-09

For setup, deployment, RPC examples, and repeat-run instructions, use
[`tests/curio/README.md`](../tests/curio/README.md). This document records the completed
scope and evidence; it is not a backlog of APIs still to implement.

## Supported scope

Curio's PDP-only Skiff binary runs against fvm-anvil and a freshly deployed FWSS stack.
PostgreSQL holds relational state; Docker-hosted ScyllaDB holds the CQL index store and
PDP proof cache. The verified workflow registers a provider, funds FilecoinPay, creates
a dataset, uploads/adds a piece, and submits a successful real `provePossession`.

Native miners, PoRep/sealing, Window PoSt, Winning PoSt, consensus, cron, native wallet
signing/submission, and general Lotus FullNode compatibility remain out of scope.
PDP Ethereum transactions are signed locally by Curio. Registration of an idle native
message task does not imply that its APIs are supported.

## Verified source versions

| Component | Revision | Runtime version |
| --- | --- | --- |
| Curio | `f5a416f633d82d293e278e7b1bb4ca135e0b6e16` | Skiff 2k build |
| Filecoin Services | `305a44723c1f0e612e294f51f5fccfc5a4812a62` | FWSS 1.5.0; registry 1.2.0 |
| Pinned PDP submodule | `cc3f5eaffee7df80471b671a4e35a42b000685b8` | PDPVerifier 3.5.0 |

The deployer builds the selected checkout and records its revisions; it does not fetch
or track a moving upstream branch. These are the tested revisions, not a promise of
compatibility with every future FWSS or Curio release.

FWSS 1.5.0 uses an ERC-8167 dispatcher, facets, selector delegates, and a proxy. Its
view address is the FWSS proxy itself. The deployment helper builds the complete graph
in a separate working copy, validates confirmed receipts and live getters, and publishes
a manifest containing contract addresses, source revisions, versions, artifacts, and
Curio environment settings. The default development payment asset is official Solady
WETH backed by native FIL, **not USDFC or a stablecoin**; an existing real 18-decimal
token can be supplied explicitly.

## Implemented API boundary

Both Ethereum and native `Filecoin.*` methods are available at `/rpc/v1` over HTTP
and WebSocket. `ChainNotify` requires WebSocket and Go `go-jsonrpc` channel framing;
it is not an Ethereum `eth_subscribe` stream.

| Native method | Positional parameters | Behavior |
| --- | --- | --- |
| `Filecoin.StateNetworkName` | `[]` | Returns `local-fvm-anvil`. |
| `Filecoin.StateNetworkVersion` | `[tipsetKey]` | Validates the selected tipset and returns network version 28. |
| `Filecoin.ChainHead` | `[]` | Returns the canonical synthetic Lotus-decodable tipset. |
| `Filecoin.ChainGetTipSet` | `[tipsetKey]` | Resolves a canonical local tipset. |
| `Filecoin.ChainGetTipSetByHeight` | `[height, tipsetKey]` | Resolves history relative to the selected anchor. |
| `Filecoin.StateGetRandomnessDigestFromBeacon` | `[epoch, tipsetKey]` | Returns the raw 32-byte beacon digest, base64-encoded in JSON. |
| `Filecoin.ChainNotify` | `[]` | Sends one initial `current`, then committed `apply` and snapshot `revert` changes; supports cancellation and reconnect. |

An empty tipset key is `[]` (`null` is also accepted for the key). Populated keys use
Lotus CID JSON objects: `[{"/":"CID"}]`. Heights/epochs are JSON integers, not Ethereum
hex quantities. Unknown keys and history outside local genesis/prepared import fail
explicitly; the node does not fabricate imported chain history.

Headers are synthetic projections, not consensus-valid miner blocks. Header CIDs hash
canonical Lotus CBOR; the tipset-key CID hashes the CBOR byte string of concatenated
header CID bytes. The FVM chain extern uses the same key CID. Beacon RPC and ref-FVM
share one deterministic raw digest source, verified through the real FIP-0095 precompile.
The other Lotus randomness APIs and native object-store APIs are not implied by this support.

The original restricted Curio interface declared 68 methods, but the default Curio build
uses Lotus FullNode bindings. The implementation follows actual PDP consumers rather than
implementing that entire declaration. In particular, PDP's beacon-digest API lies outside
the restricted interface.

## Ethereum behavior required by PDP

- `net_version`, `eth_chainId`, and transaction validation agree; Curio uses `net_version`
  as its signing chain ID.
- Pending same-sender/nonce replacement preserves queue position and successors. Both fee
  cap and premium must increase by at least 10%, rounded up, with a minimum increase of one.
  Failed validation preserves the original entry and its reservations.
- Headers include a base fee; transaction roots use the original EIP-2718 envelopes,
  including the canonical empty trie root. Full blocks, receipts, and logs decode with
  the pinned Go Ethereum client.
- Curio derives dataset/piece identities from contract events. Its Ethereum watcher needs
  one additional epoch after inclusion. Proof sends have no `message_waits_eth` row, so
  acceptance checks their actual receipts, including replacement hashes.
- Interval mining of empty blocks advances confirmations and proof schedules. A realistic
  genesis timestamp is required by the recipe; the general Anvil defaults remain unchanged.

JSON/wire projection is owned by `fvm-anvil-rpc`; the canonical chain, subscriptions,
transaction pool, and randomness are owned by `fvm-anvil-node`. CLI configuration and
transport lifetime stay in `fvm-anvil`. No second execution backend or native-message
pool was introduced.

## Indexing and database requirements

The writable profile retains the CQL store because PDP proof generation reads its cache.
It does not need a PostgreSQL-only bypass or a Curio source patch. The Compose recipe uses
PostgreSQL 16.6 and ScyllaDB 6.2.3, with single-node CQL replication factor 1.

Market/IPFS indexing is disabled without disabling proof caching:

- `Subsystems.EnableDealMarket = false`.
- `Market.StorageMarketConfig.IPNI.Disable = true`.
- `Market.StorageMarketConfig.IPNI.DirectAnnounceURLs = []`.
- The `withIPFSIndexing` metadata key is **absent**. A present key with value `"false"`
  still opts in to indexing in the pinned Curio implementation.
- Each acceptance run uses a clean dedicated database without pending advertisements.

The launcher configures a WebSocket **origin** in `FULLNODE_API_INFO`, since Curio appends
`/rpc/v1` itself. It uses the real `--repo-path` flag and Curio's PostgreSQL schema/search
path, not an assumed `public` schema. Upstream sources under `extern/` remain unmodified.

## Exercised verification

The implementation run on 2026-11-09 exercised:

- A release build and strict Clippy for the affected node, RPC, and CLI packages.
- 44 node/RPC tests, including ignored tests run with an explicit official v18 actor
  bundle: real worker/transport, raw beacon precompile equality, replacement reservations,
  direct-override native parent roots, and batched subscriptions.
- Public Ethereum acceptance with `FILECOIN_ASSERTIONS=1`, plus a real pending-replacement
  RPC scenario.
- The pinned Curio Go client: native head decoding, full empty/nonempty Ethereum blocks,
  receipts/logs, and `current/apply/revert/cancel/reconnect`.
- Fresh FWSS deployment with 26 successful FVM receipts; live versions matched the table above.
- Writable Curio creation/add/proof flow with a 64 MiB piece: dataset `1`, piece `0`, proof
  transaction `0x8275b7a3f3e269fe9123807be3ad357e6012566ecb943ffe643e1f1231bd838d` succeeded.
- 32 ScyllaDB proof-cache layer rows, false dataset IPFS-indexing intent, and no advertisement
  intents/tasks/records.

The run retained `.curio-acceptance/result.json`, `proof-receipt.json`, `proof-cache.txt`,
`skiff.log`, and a session verification record. Its deployment manifest was
`/tmp/curio-fwss-deployment.json`. These are local, ignored artifacts, not files shipped
with the repository or prerequisites for a new run.

Test processes and Docker services were stopped afterward; database volumes and results
were retained for inspection. Node state is in memory. Restarting the node requires a
fresh deployment and clean dedicated Curio databases, not reuse of an old manifest.
