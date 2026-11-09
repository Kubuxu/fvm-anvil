---
name: fwss-snapshot-extraction
description: Extract FWSS (FilecoinWarmStorageService), modular delegates, proxies, libraries, and connected contracts from a Filecoin Forest snapshot into one Press archive. Use for warm-storage snapshot extraction or fvm-anvil imports, including without a local source checkout.
compatibility: Requires release press and a ForestCAR snapshot. Runtime validation additionally requires compatible fvm-anvil, actor bundle, and cast.
---

# Extract FWSS from a Forest snapshot

Snapshot state is authoritative; deployment records and source identify candidates. Produce one versioned recipe and one combined archive. Never substitute current deployment metadata for snapshot state.

## Establish inputs and compatibility

1. Use the supplied snapshot, otherwise inspect `snapshots/` before downloading. If needed, resolve `https://forest-archive.chainsafe.dev/latest/mainnet` (or `/calibnet`) and preserve the dated artifact basename. Prefer resumable, segmented `aria2c` downloads.
2. Run `target/release/press inspect snapshots/SOURCE.forest.car.zst`. Record epoch, timestamp, parent state root, chain ID, and actual network version. Raw-snapshot inspection does not establish network version: consult pinned upgrade records or native `StateNetworkVersion` at the matching tipset.
3. Check extraction and execution compatibility separately, including actor manifest/bundle and state schemas. Never relabel network metadata, replace actor codes, or bypass checks to load newer state.
4. Establish intended workflows, explicit sidecars, patches, recipe/output paths, and source revision. If execution is unsupported, agree extraction-only versus extending runtime support; do not silently expand scope.

## Pin deployment evidence

Canonical repository: `https://github.com/FilOzone/filecoin-services`.

Read `service_contracts/deployments.json` at a pinned commit. For modular deployments also read `service_contracts/josuke.json`: its facet records and deployed-source revision may supersede a stale manifest version label. Follow pinned dependency/submodule revisions, especially ERC-8167. Use stable raw URLs or a temporary read-only checkout; never assume `extern/filecoin-services` exists.

Record constructor arguments, linked libraries, proxy targets, and code hashes. Snapshot code/storage/routes win over conflicting records. Historical RPC must match the snapshot's tipset CIDs, timestamp, and imported **head parent state**; matching epoch numbers alone is insufficient. Label corroborating historical observations separately from archive runtime proof.

## Resolve the complete graph

For each candidate record address, role, code presence, proxy/delegate targets, evidence, reachable workflow, and selection decision.

Normally include:

- FWSS proxy and live monolith, or dispatcher plus every live delegate.
- Standalone state view only if the live view path uses one.
- Externally linked libraries, not inlined imports.
- PDPVerifier and ServiceProviderRegistry proxies and live implementations.
- FilecoinPay, SessionKeyRegistry, payment token, and its proxy dependencies.
- Explicitly requested sidecars such as the endorsement set, even when not FWSS callees.

Do not select unrelated release companions automatically. Beneficiaries/controllers, including coded wallets, need inclusion only when called, explicitly requested, or required for preserved state. Proxy admins are control identities, not delegatecall dependencies. State references do not imply every referenced actor belongs in the archive.

### Proxies

Probe **every selected contract**, including tokens and implementations, recursively:

```text
ERC-1967 implementation:
0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc
beacon:
0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50
```

Use historical `cast storage ADDRESS SLOT --rpc-url RPC --block EPOCH` only after establishing state correspondence. Include each implementation and beacon; resolve the beacon's `implementation()(address)` too. Every call/delegatecall target must have code.

Without historical RPC, inspect snapshot storage offline or build a candidate archive and probe it with a compatible node. Add missing targets and rebuild until closed. Never present manifest candidates as resolved live dependencies.

### Modular dispatch

An ERC-1967 target may be an ERC-8167 dispatcher:

1. Call `selectors()(bytes4[])` through the FWSS proxy at the source state.
2. Resolve **every** selector using `implementation(bytes4)(address)`.
3. Include the dispatcher and every distinct delegate, including generated Selectors and Implementation introspection contracts.
4. Compare delegate code hashes with the pinned Josuke ledger. Resolve each module's immutables and callees independently; Config-module getters do not establish other modules' immutable values.
5. Read `viewContractAddress()`: if it returns the FWSS proxy, query views there instead of assuming a separate state view.

Exclude obsolete monoliths, old linked libraries, and rollout transition/migration artifacts unless live routes, workloads, or explicit scope require them. Verify external library addresses occur in their callers' bytecode and have code. Imports alone do not prove external linking.

## Select and build

**Before building**, present the selected contract/address/role/evidence table and deliberate omissions. Keep extraction recipes patch-free unless changes were requested. Derive any storage patch from exact deployed layouts and record original/replacement words.

Use one entry per actor:

```json
{"version": 1, "actors": [{"address": "0x..."}]}
```

Press adds System, Init, Reward, EAM, and burnt-funds actors. Do not list them. Explicitly select dynamic callees needed by the workload; Press cannot infer them or merge separate prepared archives.

```console
cargo build --release -p press
target/release/press build \
  --snapshot snapshots/SOURCE.forest.car.zst \
  --recipe recipes/FWSS_RECIPE.json \
  --output snapshots/FWSS_OUTPUT.press.car.zst \
  --network-version "$NETWORK_VERSION"
target/release/press inspect snapshots/FWSS_OUTPUT.press.car.zst
```

Set `NETWORK_VERSION` to the actual source version. Allow long-running builds; selected storage can contain millions of blocks. Rely on default structural validation before atomic publication; `--verify` adds full indexed readback.

Inspect provenance, roots, every requested address/actor ID, selected count plus five runtime actors, code dependencies, and absence of unintended patches. Unpatched selected state roots must remain unchanged.

If throughput steadily worsens per 100,000 blocks, investigate visited-CID collision chains, per-block file flushes, and source-cache behavior. Measure indexing separately; compressed size is not a work estimate.

## Validate execution when in scope

Build `fvm-anvil` and start the real node with `--state ARCHIVE --port 18547 --no-mining`. Through public RPC verify:

- Nonempty code at all selected addresses; every proxy/beacon target is included.
- For modular FWSS, all local selectors resolve to the selected delegates.
- Snapshot-appropriate `VERSION()`, dependency getters, and the actual view path; standalone views must identify the FWSS proxy via `service()`.
- PDP/registry proxy getters, FilecoinPay, SessionKeyRegistry, token `decimals()`, and selected sidecar views.
- The requested workflow. Views do not prove deposits, dataset creation, proving callbacks, or upgrade authorization. For payment validation, perform a real operation reaching both FilecoinPay and the token.

For agreed extraction-only work, explicitly report local execution as unverified. Historical chain calls are not imported-runtime proof. If a workflow reaches an omitted callee, expand and rebuild rather than fake success.

## Evidence and documentation

Keep run-specific addresses, source pins, slot words, selector routes, calls, roots, inventory, timings, and validation commands in a recipe-adjacent evidence file. Distinguish historical calls, structural validation, full readback, and local execution; never claim unrun checks.

Keep `docs/press-recipes.md` and this skill reusable: procedures, constraints, pitfalls, and links to examples. Do not append session logs, artifact statistics, or validation transcripts.

Report recipe/archive/evidence paths, contract roles and omissions, archive bytes/actors/blocks/roots, and exercised verification or blockers. Keep the chat concise; link detailed evidence. Report only measured performance.
