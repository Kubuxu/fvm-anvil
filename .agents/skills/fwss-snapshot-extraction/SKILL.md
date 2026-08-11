---
name: fwss-snapshot-extraction
description: Extract Filecoin Warm Storage Service (FWSS), its proxies, implementations, libraries, and runtime contract dependencies from a Filecoin Forest snapshot into one Press archive. Use this skill whenever a user mentions FWSS, FilecoinWarmStorageService, warm-storage state extraction, preparing an FWSS snapshot, or importing the FWSS stack into fvm-anvil—even when no Filecoin Services source checkout exists locally.
compatibility: Requires this repository's release `press` and `fvm-anvil` binaries, a compatible ForestCAR snapshot, and `cast` for runtime checks. Network access is useful for retrieving pinned deployment manifests and source when no local checkout exists.
---

# Extract FWSS from a Forest snapshot

Produce one reproducible Press recipe and one self-contained prepared archive containing the FWSS contract graph needed by the requested experiment. Treat the snapshot as authoritative for live state and proxy targets. Treat deployment records and source as discovery inputs, not proof of what the snapshot contains.

## Required outputs

Deliver all of the following:

1. Before building, list every contract planned for extraction with its address, role, and evidence.
2. A versioned recipe under `recipes/`.
3. A prepared archive under `snapshots/`.
4. `press inspect` roots and actor inventory.
5. Runtime proof from `fvm-anvil` that every selected contract and proxy dependency works.
6. Explicit omissions and the FWSS paths those omissions place out of scope.

Do not assume `extern/filecoin-services` or any other upstream checkout exists. If one exists, it is only a convenient read-only source.

## Acquire a source snapshot

Do not assume a snapshot already exists in the repository. Use a user-supplied ForestCAR when provided; otherwise resolve the ChainSafe redirect endpoint for the target network:

- Mainnet: `https://forest-archive.chainsafe.dev/latest/mainnet`
- Calibration: `https://forest-archive.chainsafe.dev/latest/calibnet`

The endpoint redirects to a dated artifact whose filename contains the network, date, and height. Resolve that URL first and preserve its basename; never save the archive under a `latest` alias.

Prefer `aria2c` when installed because these archives are large and benefit from resumable segmented downloads:

```console
redirect_url=https://forest-archive.chainsafe.dev/latest/NETWORK
snapshot_url=$(curl --silent --show-error --location --head \
  --output /dev/null \
  --write-out '%{url_effective}' \
  \"$redirect_url\")
snapshot_name=$(basename \"$snapshot_url\")

aria2c --continue=true \
  --max-connection-per-server=16 \
  --split=16 \
  --dir=snapshots \
  \"$snapshot_url\"
```

Replace `NETWORK` with `mainnet` or `calibnet`. Use `snapshots/$snapshot_name` in every subsequent command and report. If `aria2c` is unavailable, download the resolved URL with another resumable client while preserving `$snapshot_name`.

## Inputs to establish

Record these before authoring the recipe:

- source snapshot path, epoch, timestamp, network version, and state root;
- target chain ID/network;
- canonical deployment-manifest URL and exact commit or release revision;
- intended FWSS version or snapshot epoch if the user specified one;
- intended runtime paths, such as views only, provider registration, dataset creation, payments, proving, or upgrades;
- recipe and output paths;
- whether state patches are requested.

Inspect the snapshot first:

```console
target/release/press inspect snapshots/SOURCE.forest.car.zst
```

Ensure the snapshot postdates the deployment. A current default branch is not a reproducible deployment input.

## Obtain deployment evidence without a local source tree

Use the canonical Filecoin Services repository and deployment records remotely:

The canonical repository is `https://github.com/FilOzone/filecoin-services`; its deployment manifest is `service_contracts/deployments.json`. Read a pinned file without a checkout using a URL of the form:

```text
https://raw.githubusercontent.com/FilOzone/filecoin-services/COMMIT/service_contracts/deployments.json
```


1. Locate the chain entry in the canonical `deployments.json`.
2. Pin the exact repository commit recorded by that deployment or associated release.
3. Read files at that revision through stable GitHub/raw URLs or a temporary read-only checkout.
4. Follow pinned submodule revisions when constructor or library behavior lives in another repository.
5. Record constructor arguments, proxy targets, linked libraries, and deployment version metadata.

Prefer, in order:

1. deployment manifest at a pinned revision;
2. source and compiler metadata at that revision;
3. historical chain RPC at the snapshot epoch;
4. bytecode and storage observed through a temporary prepared archive.

Never substitute the newest source for the deployed source. Newer source may change versions, constructor arguments, linked libraries, storage layout, or dependencies.

## Build the dependency graph

Start with the FWSS proxy and recursively classify every address discovered from deployment metadata, source, constructor arguments, linked-library metadata, and live proxy storage.

For each address, record:

- role and contract name;
- whether it has code;
- proxy standard, if any;
- live implementation or beacon target;
- evidence source;
- which exercised path can call it;
- whether it belongs in the recipe.

### Baseline FWSS graph

A complete payment-capable FWSS archive normally needs:

- FWSS proxy;
- live FWSS implementation;
- FWSS state-view contract;
- every externally linked FWSS library;
- PDPVerifier proxy and live implementation;
- ServiceProviderRegistry proxy and live implementation;
- FilecoinPay contract;
- SessionKeyRegistry contract;
- configured payment token proxy or contract;
- live payment-token implementation when the token is a proxy.

Constructor identities such as a beneficiary or controller are not contract dependencies merely because their addresses are stored. Include them only when they contain code that an exercised path calls or when preserving their actor state is otherwise required.

Sidecar contracts deployed in the same release are not dependencies unless the deployed FWSS graph references them. For example, do not include a provider-set contract solely because it appears beside FWSS in a manifest.

### Linked libraries

Read the deployment artifact's `libraries` map. Externally linked Solidity libraries are separate EVM actors and must be selected. Imports alone do not prove external linking because internal library functions may be inlined.

After import, confirm each expected linked-library address occurs in the deployed FWSS implementation bytecode and has non-empty code.

### Immutable references

Map FWSS constructor arguments to their declared fields at the pinned source revision. Typical immutable references include PDPVerifier, FilecoinPay, the payment token, ServiceProviderRegistry, and SessionKeyRegistry. Verify their public getters through the FWSS proxy after import.

An “immutable token reference” means FWSS cannot change the referenced address without a new implementation. It does not prove that the token at that address is itself non-proxy.

## Resolve proxies recursively

Probe every selected contract, including external tokens, rather than only contracts named `Proxy` in the FWSS manifest.

For ERC-1967 proxies, read:

```text
implementation slot:
0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc

beacon slot:
0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50
```

Use a historical RPC at the snapshot epoch when available:

```console
cast storage PROXY IMPLEMENTATION_SLOT \
  --rpc-url HISTORICAL_RPC \
  --block SNAPSHOT_EPOCH
```

Confirm that the RPC's Ethereum block identifier maps to the Filecoin epoch before trusting the result.

If historical RPC is unavailable:

1. Include the known proxy in a candidate recipe.
2. Build and start the candidate archive.
3. Read the implementation slot through local RPC.
4. If it points to an actor absent from the archive, add that implementation to the recipe and rebuild.
5. Repeat for the newly discovered contract until every proxy target has non-empty code.

For a beacon proxy, include the beacon and the beacon's implementation. Proxy admins are control identities, not delegatecall dependencies.

Snapshot storage wins when a deployment record's implementation differs from the live implementation slot. Record the mismatch instead of silently selecting one.

## Present the planned extraction before building

List contracts grouped by role:

- FWSS proxy, implementation, and state view;
- linked libraries;
- PDP proxy and implementation;
- provider-registry proxy and implementation;
- payment and authorization contracts;
- payment-token proxy and implementation;
- deliberate omissions.

Include addresses and evidence. Do this before writing or running the final recipe.

## Author the Press recipe

Use one actor entry per resolved actor:

```json
{
  "version": 1,
  "actors": [
    { "address": "0x..." },
    { "address": "0x..." }
  ]
}
```

Keep extraction-only recipes patch-free unless the user explicitly requests a controller, owner, code, balance, or sequence change. If a patch is requested, derive storage slots from the exact deployed dependency revision and document source and replacement words.

Press adds System, Init, Reward, EAM, and burnt-funds actors automatically. Do not list them.

Press does not infer dynamic EVM callees. Include dynamic destinations deliberately when the requested workload can reach them.

## Build one combined archive

Build the complete graph in one recipe:

```console
cargo build --release -p press -p fvm-anvil

target/release/press build \
  --snapshot snapshots/SOURCE.forest.car.zst \
  --recipe recipes/FWSS_RECIPE.json \
  --output snapshots/FWSS_OUTPUT.press.car.zst
```

Use a command runner without a short deadline. Large PDP, payment, or token storage can contain tens of millions of IPLD blocks.

Do not split dependencies into separate prepared archives: `fvm-anvil` loads one prepared state, and Press does not merge prepared archives. Press traverses the union of all selected actor DAGs once and deduplicates shared CIDs.

Rely on the default structural validation before atomic publication, then prove the selected runtime paths through the node.

## Recognize performance regressions

Progress should remain roughly stable per 100,000 blocks after source caches warm. Diagnose a steadily worsening per-block rate rather than attributing it to a modest increase in compressed bytes.

The visited CID set must have enough buckets for multi-million-block traversals. A tiny fixed bucket table creates long on-disk collision chains with total work approaching $O(N^2/B)$. The current implementation uses a 4,194,304-head table, consuming 32 MiB for heads.

The synchronous ForestCAR writer must not flush its temporary file per IPLD block. Zstd flushes used to expose compressed frame length should remain buffered until the frame boundary.

If performance regresses, inspect:

- block count and time per 100,000 blocks;
- visited-set bucket load and collision-chain scans;
- per-block file flushes;
- source frame-cache hit behavior;
- index external-sort time separately from actor traversal.

Compressed output size alone is a poor proxy for work because EVM KAMTs can contain millions of small blocks.

## Inspect the completed archive

```console
target/release/press inspect snapshots/FWSS_OUTPUT.press.car.zst
```

Check:

- source epoch, timestamp, network version, and state root;
- expected requested address under every selected actor ID;
- selected actor count plus the five automatic runtime actors;
- no accidental patches;
- metadata and prepared roots;
- every snapshot-authoritative proxy implementation actor.

## Prove runtime behavior

Start the real node:

```console
target/release/fvm-anvil \
  --state snapshots/FWSS_OUTPUT.press.car.zst \
  --port 18547 \
  --no-mining
```

Use public Ethereum RPC to verify:

1. `eth_getCode` is non-empty at every selected address.
2. Every proxy's ERC-1967 implementation slot names an included actor with non-empty code.
3. FWSS `VERSION()` matches the snapshot deployment, not necessarily current source.
4. FWSS immutable getters return the selected PDP, payment, token, registry, and session-key addresses.
5. The state-view `service()` getter names the FWSS proxy.
6. A state-view query successfully traverses view → FWSS proxy → FWSS implementation.
7. PDP and registry version or state getters execute through their proxies.
8. FilecoinPay and SessionKeyRegistry getters execute.
9. Payment-token calls such as `decimals()` execute through the token proxy when applicable.
10. Linked-library addresses are embedded in FWSS implementation bytecode and their actors have code.

Then exercise the actual requested workflow. Views alone do not prove dataset creation, payment deposits, proving callbacks, or upgrade authorization. For a payment-capable archive, run the narrowest real operation that reaches FilecoinPay and the token.

## Handle mismatches

Use these rules:

- Manifest address differs from snapshot proxy slot: select the snapshot target and report the mismatch.
- Runtime version differs from current source: report the runtime version; do not patch it to match source.
- Address has no code and is only a recipient/controller: classify it as an identity and omit it.
- Address has no code but is a delegatecall/call target: the dependency graph is incomplete; locate and add the actor.
- A later workflow reaches an omitted dynamic callee: expand the recipe and rebuild; do not fake success.

## Report format

Return:

- final contract/address table with roles;
- omitted addresses and reasons;
- recipe and archive paths;
- archive byte size, actor count, block count, metadata root, and prepared root;
- proxy-slot and runtime-call evidence;
- exact build and validation commands;
- observed performance data without extrapolated claims.
