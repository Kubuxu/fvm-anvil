# Press state preparation

Status: active  
Last updated: 2026-08-05

`press build` resolves recipe addresses, preserves actor IDs and source Init aliases, rebuilds a V5
state tree, applies builtin-actors v18 EVM runtime-code and storage changes, writes indexed
ForestCAR, and reopens the result for structural validation. Passing `--verify` additionally reads
every reachable block back through the completed archive's index.

`fvm-anvil --state <PRESS_ARCHIVE>` mounts the output as a read-only base, validates its NV28/v18
execution context, and funds development callers in a writable in-memory overlay.

## Purpose

`press` is an offline tool for turning a large Filecoin state snapshot into a smaller state snapshot
for a specific `fvm-anvil` experiment. It reads the source snapshot without loading it wholesale,
selects actors, applies explicit state modifications, builds a new FVM state tree, and writes the
result in Forest's indexed compressed CAR format.

The primary workflow is:

```text
Filecoin mainnet snapshot
    -> press selects contracts and dependencies
    -> press copies their complete actor state
    -> press applies code and storage patches
    -> press writes a smaller ForestCAR-backed state snapshot
    -> fvm-anvil adds development callers and funding
    -> fvm-anvil executes transactions and reports results and FVM gas
```

The output no longer depends on the source snapshot. It still uses the actor bundle selected by
`fvm-anvil`, as ordinary Forest snapshots do.

## Scope decisions

The implementation follows these decisions:

- Recipes describe reusable actor selections and transformations. They do not pin an expected input
  state root or snapshot CID.
- Output provenance records the source head and state root actually used, without making them recipe
  preconditions.
- The workflow copies selected state into a new snapshot. Copy-on-write overlays and
  workload-specific read witnesses are not implemented.
- `press` does not create or fund development callers. `fvm-anvil` applies those local changes after
  opening the prepared state.
- Selected actors are copied with their complete internal state by default. Selective or observed
  storage pruning is deferred because omitted storage changes contract behavior.
- Actor dependencies are explicit. `press` does not claim to infer the complete dynamic EVM call
  graph.

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

## Recipe and command model

The source snapshot is an invocation input so the same recipe can be rerun against newer state:

```text
press inspect SNAPSHOT
press build --snapshot SNAPSHOT --recipe RECIPE --output OUTPUT
  [--network-version 28] [--force] [--verify]
press inspect OUTPUT
```

Every build reopens the completed archive and validates its metadata, compact state tree, actor
records, requested aliases, patches, and manifest linkage before atomic publication. `--verify`
additionally traverses every reachable actor-state block through the generated ForestCAR index. That
full readback audit is useful for distributed artifacts but is disabled by default because the
build's export traversal has already read and copied the complete selected DAG.

The reusable source-pinning, dependency-discovery, proxy-patch, and runtime-proof workflow is
recorded in [`press-recipes.md`](press-recipes.md).

Recipe version 1 selects actors by stable user-facing addresses and optionally applies explicit
transformations:

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

Structural verification reloads the completed archive, checks final actor records, recomputes
replacement bytecode hashes, and reads every patched storage slot back from the emitted KAMT. All
changes are committed into the new outer state root before publication.

Press does not synthesize a new EVM contract at an address that is absent from the source. That
requires separate EAM and actor-invariant work and is not needed for replacing code on an imported
contract.

## Responsibility split

### `press`

`press` owns deterministic offline preparation:

- indexed source snapshot access;
- source context and actor inspection;
- actor selection;
- Init map reconstruction;
- versioned EVM state transformations;
- custom state-tree construction;
- streaming ForestCAR output;
- output provenance.

A successful build reopens the completed archive and validates metadata, actor records, requested
addresses, patches, and manifest linkage before publication. With `--verify`, it also traverses every
reachable actor-state block through the completed archive's index. Actor-bundle compatibility is not
a `press` build gate.

### `fvm-anvil`

`fvm-anvil` owns local runtime behavior:

- loading the configured actor bundle;
- validating what it needs to construct the ref-FVM machine;
- opening the prepared state root and execution context;
- adding and funding generated development callers through the imported Init state;
- maintaining the writable execution overlay;
- mining, snapshots, reverts, RPC indexes, and gas diagnostics.

Node startup uses the same bounded validation as a default Press build. It validates prepared
metadata, the compact outer state tree, actor records, requested aliases, patches, and manifest
linkage. This keeps startup proportional to the prepared state boundary rather than an imported
actor's full storage; an archive missing an unpatched actor-state block fails when that block is
first accessed.

The root stored by `press` is the prepared imported root. The node immediately derives a writable
current root when it adds development accounts. `filecoin_getStateRoots` reports both roots, and the
current root advances after adopted transactions while the prepared root remains fixed.

## Code ownership

The separate tool establishes a real shared state boundary:

```text
fvm-anvil-state
    ForestCAR reader and writer
    prepared-state metadata
    source snapshot context extraction
    state-tree construction and export
    Init map reconstruction
    EVM state adapters and patches

press
    offline CLI and recipe parsing

fvm-anvil-node
    live mutable execution and development-account overlay
```

Both binaries use the same prepared-state metadata and ForestCAR blockstore implementation.
`fvm-anvil-node` remains the sole owner of live chain state; the shared state crate owns only storage
formats and deterministic offline transformations.

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
