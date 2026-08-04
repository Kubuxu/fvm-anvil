# Press state preparation

Status: active  
Last updated: 2026-08-04

Current implementation: storage and standalone actor extraction are complete. `press build` resolves
recipe addresses, preserves actor IDs and all their source Init aliases, rebuilds a V5 state tree,
copies complete actor state DAGs and available code blocks, writes indexed ForestCAR, and reopens the
result for structural verification. The workflow has extracted mainnet WFIL at epoch 6,251,280 into
an independently readable 798,060-byte archive containing 454 blocks. Contract state patches remain
the next stage.

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

The initial implementation follows these decisions:

- Recipes describe reusable actor selections and transformations. They do not pin an expected input
  state root or snapshot CID.
- Output provenance records the source head and state root actually used, without making them recipe
  preconditions.
- The main workflow copies selected state into a new snapshot. Copy-on-write overlays are possible
  later but are not the initial product.
- Workload-specific read witnesses are deferred.
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
PressStateMetadataV1
    state_root
    source_head_tipset
    source_parent_state_root
    epoch
    timestamp
    base_fee
    network_version
    actor_manifest
    included_actor_ids
    applied_patches
    recipe_digest
```

The container is ForestCAR-compatible for indexed block access, but the output is intended as an FVM
state snapshot for `fvm-anvil`, not as a chain head for Forest synchronization.

## Recipe and command model

The source snapshot is an invocation input so the same recipe can be rerun against newer state:

```text
press inspect SNAPSHOT
press build --snapshot SNAPSHOT --recipe RECIPE --output OUTPUT
press inspect OUTPUT
```

The current extraction recipe identifies actors by stable user-facing addresses:

```json
{
  "version": 1,
  "actors": [
    { "address": "0xTarget" },
    { "address": "0xDependency" }
  ]
}
```

Stage 3 extends the same actor entries with explicit `code`, `storage`, `balance`, and `sequence`
patches. The Stage 2 binary rejects those fields rather than silently ignoring an unimplemented
transformation.

Addresses are required by default. A build fails clearly if a requested address does not exist. The
output metadata records resolved actor IDs, requested addresses, source state CIDs, prepared state
CIDs, code CIDs, the source execution context, and manifest data for inspection and provenance.

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

The initial patch surface supports existing EVM actors:

- replace deployed/runtime bytecode;
- set or delete storage slots;
- optionally set the existing actor balance or Filecoin sequence.

Bytecode replacement writes a new raw bytecode block and Keccak-256 bytecode hash while preserving
the contract storage root and EVM nonce. Storage patches load the actor's KAMT, delete zero values,
set nonzero values, flush once, and update the EVM actor state. All requested actor changes are
committed into the new outer state root before export.

The initial implementation does not synthesize a new EVM contract at an address that is absent from
the source. That requires separate EAM and actor-invariant work and is not needed for replacing code
on an imported mainnet contract.

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

A successful build performs lightweight structural checks while the output is still open: the
metadata and root decode, the new state tree loads, requested addresses resolve to the copied actor
IDs, and requested patches read back from the new state. Actor-bundle compatibility is not a `press`
build gate.

### `fvm-anvil`

`fvm-anvil` owns local runtime behavior:

- loading the configured actor bundle;
- validating what it needs to construct the ref-FVM machine;
- opening the prepared state root and execution context;
- adding and funding generated development callers through the imported Init state;
- maintaining the writable execution overlay;
- mining, snapshots, reverts, RPC indexes, and gas diagnostics.

The root stored by `press` is the prepared imported root. The node may immediately derive a new root
when it adds development accounts. Filecoin observability should distinguish the prepared root from
the current execution root.

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

## Implementation plan

### Stage 1: indexed snapshot storage — delivered

1. Adapt ForestCAR indexed reading, frame decompression, bounded caching, and streaming writing.
2. Expose it through `fvm_ipld_blockstore::Blockstore` without Forest production dependencies.
3. Add prepared-state metadata encoding and decoding.
4. Add small multi-frame fixtures that prove random lookup does not ingest the archive wholesale.

Gate: `press inspect` opens a Forest snapshot and reports its head execution context; `press build`
reads chosen actors through indexed block access without ingesting the source archive.

### Stage 2: custom state construction — delivered

1. Resolve recipe addresses against the source Init actor.
2. Preserve selected actor IDs and records.
3. Copy the mandatory runtime actors into a new V5 state tree.
4. Rebuild the Init address map for included actors while preserving source `next_id`.
5. Flush the new state root.
6. Traverse the new tree and complete selected actor-state DAGs into `.press.car.zst`.

Gate: the prepared archive opens without the source snapshot, its state tree loads, included
addresses resolve to their original actor IDs, and unrelated actors are absent.

### Stage 3: contract patches — planned

1. Add the versioned v18 EVM state adapter needed for serialization and mutation.
2. Replace existing contract runtime bytecode while preserving storage and EVM nonce.
3. Apply batched storage-slot changes with one KAMT and state-tree flush.
4. Record resolved source actors and applied transformations in output metadata.
5. Read patches back from the completed archive as part of `press build`.

Gate: a prepared contract exposes the replacement bytecode and patched storage while an unpatched
copy exposes the original values.

### Stage 4: node import

1. Add prepared-state input to genesis-time node configuration.
2. Mount the prepared ForestCAR as a read-only base blockstore.
3. Initialize the machine at the recorded root, epoch, timestamp, base fee, and network version.
4. Add generated development accounts and funding in the node's writable overlay.
5. Expose prepared and current state roots separately.
6. Exercise the real endpoint against an imported and patched contract.

Gate: the actual binary starts from a prepared archive, funds its development caller, executes a
transaction against copied mainnet contract state, and returns the result, receipt, and FVM gas
breakdown.

### Stage 5: persistence and broader preparation

1. Persist the node's writable overlay and adopted root.
2. Add prepared-state diff and richer inspection commands when they serve concrete workflows.
3. Add new-actor synthesis only after EAM-created public behavior is specified and tested.
4. Consider overlay outputs and workload witnesses separately after the standalone-copy workflow is
proven.

## Deferred work

The initial plan deliberately defers:

- workload read witnesses;
- automatic dynamic call-graph discovery;
- storage pruning based on observed execution;
- consensus-valid synthetic Filecoin tipsets;
- importing historical messages, receipts, or Ethereum projections;
- general native-actor state rewriting;
- creating a new EVM actor at an address absent from the source snapshot.
