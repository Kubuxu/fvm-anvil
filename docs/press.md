# Press state preparation

Status: active  
Last updated: 2026-11-09

`press` turns a large Filecoin state snapshot into a smaller, independent archive for a specific
`fvm-anvil` experiment. Select contracts and dependencies in a recipe, optionally patch their code
or state, then execute transactions against the imported deployment with local development callers
and FVM gas diagnostics.

## Build, inspect, and run

From the repository root, build the tools and prepare an archive:

```bash
cargo build --release -p press -p fvm-anvil

target/release/press inspect snapshots/SOURCE.forest.car.zst
target/release/press build \
  --snapshot snapshots/SOURCE.forest.car.zst \
  --recipe recipes/RECIPE.json \
  --output snapshots/OUTPUT.press.car.zst
target/release/press inspect snapshots/OUTPUT.press.car.zst
```

Supply an existing source ForestCAR and recipe; the filenames above are placeholders. Builds accept
`--network-version 21` through `--network-version 29` (default 28), `--force` to replace an output,
and `--verify` for the additional audit described [below](#validation-and-runtime-loading).
Progress goes to stderr; the final build summary goes to stdout.

NV29 extraction preserves the source actor codes, manifest, and selected state DAGs without executing
them. The System and Init wire layouts remain compatible with the offline reconstruction. This does
not add NV29 execution support: the current node still requires NV28/v18 and rejects an NV29 archive.
Use the actual source network version rather than relabeling a newer snapshot to make it load.

Start the foreground node in **terminal A**:

```bash
target/release/fvm-anvil --state snapshots/OUTPUT.press.car.zst --port 18546
```

Use **terminal B** for RPC calls or a workload runner:

```bash
cast block-number --rpc-url http://127.0.0.1:18546
```

The node mounts the archive read-only and funds development callers in an in-memory overlay.
The output no longer depends on the source snapshot, but still needs the actor bundle selected by
`fvm-anvil`. Restarting discards local changes and returns to the prepared state.

For source pinning, dependency selection, proxy patches, and public runtime proofs, follow the
[recipe runbook](press-recipes.md), including its
[mainnet PDPVerifier v3.4.0 example](press-recipes.md#pdpverifier-mainnet-v340).
[Historical PDPVerifier benchmark reports](../benchmarks/pdp-verifier/README.md) live beside their
runners.

## Snapshot format

`press` reuses ForestCAR's physical format:

- independently compressed zstd frames;
- a persistent CID index;
- random block lookup without ingesting the archive into a database;
- a bounded decompressed-frame cache;
- streaming construction.

Production code adapts the narrow ForestCAR reader and writer with attribution rather than depending
on Forest. The source snapshot remains an immutable read-only blockstore throughout the build.

A prepared output uses a distinct `.press.car.zst` suffix to prevent it from being mistaken for a
consensus chain snapshot. Its CAR root is a versioned metadata block rather than a Filecoin tipset:

```text
PreparedStateMetadata (format version 2)
    prepared state root and source execution context
    network version and actor manifest data
    external actor code dependencies
    included actor records and source state CIDs
    requested addresses
    applied code, storage, balance, and sequence patches
```

The container is ForestCAR-compatible for indexed block access, but the output is intended as an FVM
state snapshot for `fvm-anvil`, not as a chain head for Forest synchronization.

## Recipe schema

The source snapshot is an invocation input, so the same recipe can be rerun against newer state.
Recipes do not pin an expected input root or snapshot CID; output provenance records the actual
source head and state root. Recipe version 1 selects actors by stable user-facing addresses and
optionally applies explicit transformations:

```json
{
  "version": 1,
  "actors": [
    {
      "address": "0x1111111111111111111111111111111111111111",
      "code": { "replace": "0x60006000f3" },
      "storage": {
        "0x0000000000000000000000000000000000000000000000000000000000000001":
          "0x0000000000000000000000000000000000000000000000000000000000000002"
      },
      "balance": "1000000000000000000",
      "sequence": 7
    },
    { "address": "0x2222222222222222222222222222222222222222" }
  ]
}
```

Code is hex-encoded deployed/runtime bytecode. Storage keys and values are exactly 32-byte hex
words; a zero value deletes the slot. Balance is a non-negative decimal attoFIL integer and sequence
is a JSON `u64`. Explicit `null` fields are rejected. Multiple entries resolving to the same actor
are rejected rather than merged.

Addresses are required. A build fails clearly if a requested address does not exist or an
EVM-specific patch targets a non-v18-EVM actor. Output metadata records final actor records, source
and prepared state CIDs, source and replacement code details, storage changes, requested addresses,
the source execution context, and manifest data.

## Building the custom state tree

`press` opens the source snapshot as a blockstore, obtains the state root and execution context from
its head tipset, and loads the source V5 state tree. It then creates a new V5 state tree and preserves
the original actor ID for every copied actor.

The new tree automatically contains the execution substrate required by the current runtime:

- f00 System actor;
- f01 rebuilt Init actor;
- f02 Reward actor used as the fee recipient;
- f010 EAM actor;
- f099 burnt-funds actor.

The recipe supplies target contracts and any other actors the experiment expects to call. Dynamic
EVM destinations cannot be determined reliably from bytecode alone, so an omitted dependency has
normal absent-actor behavior in the prepared state.

### Init state

The rebuilt address HAMT retains every source Init address-map entry whose value is an included actor
ID. This preserves both robust and delegated aliases even when the recipe selected the actor by ID
or by only one alias. `press` scans the source Init HAMT cachelessly to find those reverse mappings;
the scan reads the whole map but does not retain it in memory.

It preserves:

- each included actor's original ID;
- the source network name;
- the source Init actor's `next_id`.

Preserving `next_id` prevents later actor creation from reusing an ID that existed in the source
state but was omitted from the prepared tree. Singleton ID addresses do not need address-map
entries.

### Actor state

For each selected actor, `press` copies its `ActorState`, the complete IPLD DAG rooted at its state
CID, and its code block when that block is present in the source snapshot. For an EVM actor the state
DAG includes runtime bytecode and the complete storage KAMT. Code blocks absent from the snapshot
and the System actor's manifest data CID are recorded as external actor-bundle dependencies.

The exporter traverses only the new state tree and selected actor DAGs, stopping at authenticated
external links. It streams blocks to the output and uses a disk-backed visited-CID set so memory use
does not scale with the number of copied blocks.

Removing unrelated actors, chain history, messages, receipts, and unrelated state can reduce the
snapshot substantially. If a selected actor's own storage dominates the source snapshot, a faithful
copy of that actor remains large.

## State modifications

The patch surface supports existing builtin-actors v18 EVM actors:

- replace deployed/runtime bytecode;
- set or delete storage slots;
- set the existing actor balance or Filecoin sequence.

Bytecode replacement enforces the actor's 24 KiB limit and reserved `0xef` prefix rule, writes a raw
IPLD bytecode block, and records its Keccak-256 hash while preserving the contract storage root, EVM
nonce, transient data, and tombstone. Storage patches use the v18 minimal-big-endian word encoding
and KAMT configuration, delete zero values, apply all requested slots, and flush once. Actor balance
and sequence changes update the outer `ActorState`.

All changes are committed into the new outer state root before publication.


## Validation and runtime loading

Every build reopens the completed archive and validates its metadata, compact state tree, final
actor records, requested aliases, patches, and manifest linkage before atomic publication. Patch
validation recomputes replacement bytecode hashes and reads every patched storage slot back from
the emitted KAMT. `--verify` additionally traverses every reachable actor-state block through the
generated ForestCAR index. This full readback audit is useful for distributed artifacts but is
disabled by default: the export traversal has already read and copied the complete selected DAG.

Actor-bundle compatibility is not a Press build gate. Missing source code blocks and the System
manifest data remain explicit external bundle dependencies; `fvm-anvil` loads its configured bundle
and validates the prepared NV28/v18 execution context when constructing the ref-FVM machine.

Node startup uses the same bounded structural validation as a default build, rather than reading
an imported actor's full storage. An archive missing an unpatched actor-state block therefore fails
when that block is first accessed.

The root stored by Press is the prepared imported root. Adding development accounts derives a
writable current root; `filecoin_getStateRoots` reports both. The current root advances after adopted
transactions, while the prepared root remains fixed.

The shared `fvm-anvil-state` crate owns ForestCAR, prepared metadata, source-context extraction,
state-tree export, Init reconstruction, and EVM patch adapters. `press` owns the offline CLI and
recipe parsing; `fvm-anvil-node` alone owns live execution, the development-account overlay, mining,
snapshots/reverts, RPC indexes, and gas diagnostics.

## Current limitations

- The node's writable overlay and adopted root are not persisted across restarts.
- Recipes must name dynamic EVM callees explicitly; Press does not infer a complete call graph.
- Selected actors retain their complete state DAG. Workload witnesses and observed-storage pruning
  are not implemented.
- Press does not create an EVM actor at an address absent from the source snapshot.
- Prepared archives do not contain consensus-valid synthetic tipsets, historical messages,
  receipts, or Ethereum projections.
- General native-actor state rewriting is outside the supported patch surface.

Future persistence must keep the prepared ForestCAR immutable and publish the overlay plus adopted
root atomically. New-actor synthesis requires EAM-created public behavior and actor invariants to be
specified and tested first.
