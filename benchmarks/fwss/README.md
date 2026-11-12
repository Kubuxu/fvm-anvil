# FWSS gas benchmark

This benchmark compares PDPVerifier and Filecoin Warm Storage Service implementations against imported mainnet state. It measures lifecycle transactions, records complete Filecoin gas traces, resumes completed cases, and searches for practical batch-size limits. See [`design.md`](design.md) for the workload and result model.

Run commands from the repository root.

## Prerequisites

- `cast` and `forge` from Foundry.
- `uv` for the runner's Python dependency.
- A release build of `fvm-anvil`.
- `snapshots/fwss-mainnet-v1.3.0.press.car.zst`.

Build the node:

```console
$ cargo build --release -p fvm-anvil
```

The local variants use independent, ignored checkouts under `extern/`; they do not depend on sibling repositories outside this checkout. Create both clones with their pinned submodules:

```console
$ git clone --recurse-submodules https://github.com/FilOzone/pdp.git extern/pdp
$ git clone --recurse-submodules https://github.com/FilOzone/filecoin-services.git extern/filecoin-services
```

Build each Solidity project once with `forge build` to confirm the checked-out revisions compile. The runner rebuilds them before deploying local variants.

## Start the node

```console
$ target/release/fvm-anvil \
    --state snapshots/fwss-mainnet-v1.3.0.press.car.zst \
    --port 18546
```

The runner mutates the in-process chain and verifies that each invocation starts at the imported state. Restart the node before each separate `run` or `max-batch` invocation.

## Run cases

List available comparison keys without starting the node:

```console
$ benchmarks/fwss/run.py list --variant deployed
```

Run every fixed case for the deployed implementation:

```console
$ benchmarks/fwss/run.py run --variant deployed
```

Select cases with repeatable shell-style patterns:

```console
$ benchmarks/fwss/run.py run \
    --variant deployed \
    --case 'createDataSet/*' \
    --case 'addPieces/batchSize-32/*'
```

Omitting `--variant` runs every variant configured in `suite.json`. Completed cases with the same measurement identity are skipped. Add `--force` to replace selected results.

Search for a batch boundary:

```console
$ benchmarks/fwss/run.py max-batch \
    --variant deployed \
    --operation addPieces \
    --metadata one \
    --starting-piece-count 0
```

The supported boundary operations are `addPieces`, `schedulePieceDeletions`, `processPieceDeletions`, `nextProvingPeriod`, and `cleanupPieces`. `processPieceDeletions` is not applicable to PDP builds that still process removals inside `nextProvingPeriod`. Use `--ceiling` to override the configured search ceiling.

## Report results

Results default to `benchmarks/fwss/results/mainnet-v1.3.0` and are ignored by Git.

```console
$ benchmarks/fwss/report.py \
    --baseline deployed \
    --variant local-pdp \
    --variant local-new
```

Filter with `--operation` or select `--format json` or `--format csv` for machine-readable output. Reporting does not require a running node.

## Historical piece-metadata cleanup

`metadata_cleanup.py` is a separate, lab-only experiment over imported historical metadata. It does
not change the lifecycle suite or upgrade FWSS. The two targeted storage families are the historical
piece-value mapping at Solidity root 13 and piece-key arrays at root 14. Dataset metadata at roots
11/12 and unrelated FWSS state are not cleanup targets.

Build the patch-free archive using
[`recipes/fwss-mainnet-6457920.json`](../../recipes/fwss-mainnet-6457920.json). Snapshot provenance,
dependency addresses, roots, omissions, and runtime validation are recorded in
[`docs/press-recipes.md`](../../docs/press-recipes.md#fwss-metadata-cleanup-snapshot-at-epoch-6457920).
Export every occupied FWSS storage word once, then run discovery without reopening the Press archive:

```console
cargo build --release -p fvm-anvil-state --example fwss_metadata_inventory
target/release/examples/fwss_metadata_inventory \
  snapshots/fwss-mainnet-6457920.press.car.zst \
  benchmarks/fwss/results/metadata-cleanup-6457920/slots \
  --export-slots-only
target/release/examples/fwss_metadata_inventory --intersect-slots \
  benchmarks/fwss/results/metadata-cleanup-6457920/slots \
  benchmarks/fwss/results/metadata-cleanup-6457920/inventory-plus-1000 \
  --extra-pieces 1000
```

`slots/occupied-slots.bin` contains **every** FWSS proxy KAMT entry, without metadata filtering.
It has no header: each 64-byte record is the original 32-byte slot followed by its 32-byte value.
Records are strictly increasing in unsigned big-endian slot order. `occupied-slots.json` records the
record count, byte size, actor/storage roots, source epoch, and recovered dataset/piece bounds.
The binary export and manifest are sufficient for subsequent processing; the source archive is not
read by `--intersect-slots`. Treat the export as immutable while a reader uses it.

The intersection command requires GNU `sort`. For each recovered exclusive bound `B`, it generates
root-14 key-array candidates over `[0, B + N)`, where `--extra-pieces N` defaults to 1,000.
Zero-bound datasets therefore probe IDs 0 through 999. `--extra-pieces 0` reproduces the original
recovered-range scan. The recovered bound is validated against the manifest cap before checked
addition of the extension, so cap-bound datasets still receive the full extension.
Candidates are externally sorted with a 128 MiB sort-memory budget, then sequentially intersected
against the complete occupied-slot export. Retained
`candidate-metadata-key-slots.tsv` rows are `slot-hex<TAB>dataset<TAB>piece`, sorted by slot.
`metadata-key-hits.tsv` rows are `dataset<TAB>piece<TAB>key-count`, sorted by dataset/piece.
Only occupied hits proceed to string decoding. Stored key bytes determine the root-13 value slots,
which are read and cross-verified against the exported values.

The command writes aggregate `summary.json` and dataset/piece-ordered `pieces.jsonl`. Each piece
includes raw key/value bytes and the exact nonzero storage words belonging to its metadata, including
long Solidity strings. The decoder accepts historical records exceeding today's admission limits;
resource guards are 1,024 keys per piece and 1 MiB per string. Payload-byte counts are not compressed
archive sizes; nonzero-word counts include Solidity bookkeeping. Export also writes
`unresolved_nonce_candidates.jsonl`; unresolved bounds must remain visible.
`added-range-hits.tsv` lists `dataset<TAB>piece<TAB>key-count<TAB>nonzero-word-count` only for
metadata beyond recovered bounds. Global and per-dataset summaries separate recovered counts from
added candidate, metadata-piece, key, and storage-word counts and reconcile them against totals.

**Coverage is explicit, not assumed.** Historical dataset IDs come from PDP's monotonic dataset
counter. Piece bounds combine retained legacy counters, compact-array lengths, and FWSS packed-nonce
high-water candidates. PDP cleanup can shrink counters/arrays to zero; older nonce formats may not
retain a high-water mark. Solidity mapping hashes do not reveal their preimages. Consequently the
reported inventory is exhaustive within recovered ranges plus the explicit extension, not proof that
every historical orphan has been identified. Unresolved bounds must not be treated as empty datasets.
Complete historical coverage requires sufficient preimage/high-water evidence, such as indexed
addition events.

Start a fresh node, then compare a bounded, identical selection:

```console
target/release/fvm-anvil --state snapshots/fwss-mainnet-6457920.press.car.zst --port 18548
uv run benchmarks/fwss/metadata_cleanup.py \
  --inventory benchmarks/fwss/results/metadata-cleanup-6457920/inventory-plus-1000 \
  --rpc-url http://127.0.0.1:18548 \
  --max-pieces 512 \
  --piece-batch-sizes 32,64 \
  --word-batch-sizes 64,128,256 \
  --output benchmarks/fwss/results/metadata-cleanup-6457920/sample-512
```

The natural prefix is frozen before sorting. SQLite bounds planning memory. Natural cleanup scans
piece ranges, including empty pieces before/between populated entries in selected datasets, deletes
mapping strings, then deletes their key arrays. The raw-natural control clears the same inventoried
nonzero words in dataset/piece order. The second prototype sorts those words globally by unsigned
256-bit Solidity slot, then batches that one sorted stream. It does not sort each natural batch
independently. The raw-natural versus sorted comparison isolates ordering at equal word-batch sizes;
the natural comparison additionally includes discovery, string decoding, and Solidity deletion work.
Batch sizes are experimental parameters, not measured transaction-capacity maxima.
Use `--max-pieces 0` to select the complete recovered inventory instead of a bounded prototype.

The contract is compiled in the isolated `contracts/` project, deployed to bind its immutable
operator, and temporarily installed at the imported FWSS proxy with `anvil_setCode`. This is a
deliberately unsafe production mechanism: arbitrary supplied storage slots can be cleared. The
prototype is for an isolated local node, not a production migration or authorization design.
Execution uses the real FVM and a 10-billion-gas transaction limit. Every variant begins at the same
snapshot. The runner checks inventory source words, verifies all targeted words become zero and
piece-key getters become empty, checks unrelated/dataset-metadata sentinels, and restores the original
runtime and state afterward.

Results contain receipts, full per-transaction `filecoin_getGasTrace` files, charge aggregates,
workload digests, and restoration evidence. `OnBlock*` charges expose observed IPLD operations; they
also include actor/message overhead and must not be labeled exact KAMT-node counts. Slot order is
KAMT-prefix order because the v18 actor uses raw big-endian slot bytes as its KAMT key. That motivates
locality across transaction boundaries but does not establish globally gas-optimal batch boundaries.
The actor caches mutations and flushes on return/external send, not after every `SSTORE`.

### Observed epoch-6,457,920 results

The patch-free export contains 23,979,574 occupied words: 1,534,692,736 bytes. Its SHA-256 is
`21f1f81df277dbdee6af903e0ce5af5983fc6e46722ec17cc3ee1ea4de38b481`.
Export took 64.08 seconds with 594,404 KiB maximum RSS. Archive-independent processing took
85.13 seconds with 1,504,728 KiB maximum RSS: 9.00 seconds generating/sorting 5,842,509 candidates,
9.34 seconds intersecting/sorting 4,469,179 occupied hits, and 66.74 seconds decoding and checking
their keys/values. These are individual local observations, not controlled cold-cache benchmarks.
The old random-KAMT scanner was stopped after 20 minutes 57 seconds while still incomplete;
its first 2,167,079,066 output bytes matched the indexed implementation. The complete intersection
inventory is byte-for-byte identical to the 4,476,875,394-byte indexed inventory.

Within the recovered ranges, 1,046 datasets retain metadata for 4,469,179 pieces, with 4,489,292
key/value entries and 312,185,294 payload bytes. They own 22,344,544 nonzero storage words
(715,025,408 bytes of word values), accounting for 93.18% of the occupied FWSS words.
There are 424 empty values and 115 pieces with four metadata keys. No candidate bounds exceeded
the configured cap, but this does not remove the historical-preimage coverage limitation above.
The later `inventory-plus-1000` scan added 1,733,000 candidates across all 1,733 datasets, including
34 with zero recovered bounds. It found no additional metadata; its complete `pieces.jsonl` is
byte-for-byte identical to the original inventory. The expanded run took 88.23 seconds:
11.75 seconds generating/sorting 7,575,509 candidates, 10.54 seconds intersecting/sorting hits,
and 65.88 seconds decoding/output. The existing join was retained because decoding/output dominated
and no measured safe join optimization was established. These timings are individual local
observations under differing cache conditions, not a controlled speedup claim.
`padding-boundary-smoke.json` records exact first/last included IDs, zero-bound datasets,
zero-extension reproduction, expansion beyond the recovered-bound cap, and checked overflow.

All following cases used the same 512-piece natural prefix: 2,470 nonzero target words across
14 datasets, with no empty-piece gaps inside the selected ranges. Each case was successful,
cleared every target word/key array, preserved the dataset-metadata and unrelated sentinels, and
restored the starting state before the next case.

| Plan | Batch unit/size | Transactions | Total FVM gas | `OnBlockLink` charges |
| --- | ---: | ---: | ---: | ---: |
| Solidity dataset/piece iteration | 32 pieces | 16 | 37,902,560,498 | 6,609 |
| Solidity dataset/piece iteration | 64 pieces | 8 | 35,398,827,667 | 6,235 |
| Raw dataset/piece order | 64 words | 39 | 41,909,943,908 | 7,259 |
| Slot-sorted same natural subset | 64 words | 39 | 28,288,109,890 | 5,227 |
| Raw dataset/piece order | 128 words | 20 | 38,551,839,741 | 6,747 |
| Slot-sorted same natural subset | 128 words | 20 | 27,829,316,152 | 5,148 |
| Raw dataset/piece order | 256 words | 10 | 35,969,249,723 | 6,356 |
| Slot-sorted same natural subset | 256 words | 10 | 27,582,219,152 | 5,106 |

At 256 words/transaction, ordering alone reduced gas by 23.32% and `OnBlockLink` counts by 19.67%.
The sorted plan used 22.08% less total gas than Solidity iteration at 64 pieces/transaction; those
plans have different transaction counts and do not isolate ordering alone. Do not extrapolate
the sample's gas to the complete 4.47-million-piece cleanup or call these batch sizes capacity limits.

These measurements were newly executed against the indexed inventory; the later intersection's
complete `pieces.jsonl` was compared byte-for-byte, so the transactions were not repeated merely
to relabel their source. Results and full transaction traces are under
`results/metadata-cleanup-6457920/sample-512/`. The operator-bound runtime SHA-256 was
`f3a64126fd6540503f7fd19294310830a0dfd72490dfd31e361dd4c2a6610bb5`, built with Solidity 0.8.26 and
optimizer runs 200. `encoding-smoke.json` also records runtime checks for empty keys/values,
31/32-byte keys, 32/33/96-byte values, and a zero long-string payload word.
`native-encoding-smoke.json` verifies exact standalone decoding of the same boundaries, including
all target words and exclusion of an unrelated occupied word. `native-export-to-cleanup-smoke.json`
records passing those decoded records through all three runtime plans with `--max-pieces 0`.

Both imported FWSS runtime code and the exact post-import FVM root were restored after measurement.
The archive was never modified. These experiments omit production upgrade authorization, persistent
on-chain progress, transaction-capacity search, and a full-state cleanup run.

### Equal-word comparison against the full global inventory

`--mode global-prefix` selects the first 512 metadata-bearing pieces in natural dataset/piece
order, counts their nonzero storage words `N`, then selects the lowest `N` storage slots from
the **entire** decoded metadata inventory. The unit is an occupied EVM slot/value pair, not a
complete application metadata string key/value entry. Selection uses a bounded `N+1` max-heap
in one streaming pass, carries source owner words for preservation checks, and verifies the full
source counts against the inventory summary. It does not independently sort only the natural subset.

```console
target/release/fvm-anvil --state snapshots/fwss-mainnet-6457920.press.car.zst --port 18549
uv run --script benchmarks/fwss/metadata_cleanup.py \
  --inventory benchmarks/fwss/results/metadata-cleanup-6457920/inventory-plus-1000 \
  --rpc-url http://127.0.0.1:18549 \
  --mode global-prefix \
  --variant natural-pieces-64 \
  --variant raw-natural-words-256 \
  --variant raw-global-prefix-words-256 \
  --output benchmarks/fwss/results/metadata-cleanup-6457920/global-prefix-512
```

Omitting `--variant` requests those three cases. Global mode fixes the natural sample at 512 pieces,
the Solidity natural batch at 64 pieces, and both raw batch sizes at 256 words. The default
`--reuse-from` directory is `results/metadata-cleanup-6457920/sample-512`; another directory can
be supplied explicitly. Matching measurements are reused only after checking execution identity.
For legacy measurements, the runner reconstructs and compares exact selected rows, source digest,
ranges, imported roots, gas limit, and runtime-code fingerprints. Reused trace paths point back
to their original files. Incompatible results are not overwritten.

Both `natural-selection.json` and `global-selection.json` persist slot/value words, owner IDs,
selection/content digests, source digest, slot range, and ownership distributions. The global case
can leave partial metadata structures inconsistent: it must not be presented as a production-safe
migration with concurrent legacy metadata reads or cleanup. Postconditions check all selected words
are zero and that the first excluded global word plus up to two untargeted sibling words per selected
owner remain unchanged. Whole-piece key-array-empty checks apply only to the natural cases.
Preservation checks use batched EVM calls, not thousands of individual RPC reads.
The final exact FVM state-root commitment and original runtime bytecode verify restoration of all
selected and untargeted words.

The measured natural sample contains 512 pieces, 520 application metadata entries, and 2,470 storage
words across 14 datasets; dataset 64 contributes 440 pieces. The full-global prefix contains exactly
2,470 words from 1,935 partially selected pieces across 62 datasets, selected from all 22,344,544
metadata words. Its largest contributor is dataset 1293, with 1,231 words. There is **zero overlap**
between the natural and global word sets.

| Selection/plan | Batch | Transactions | Total FVM gas | `OnBlockLink` charges | Measurement |
| --- | ---: | ---: | ---: | ---: | --- |
| Natural 512-piece Solidity cleanup | 64 pieces | 8 | 35,398,827,667 | 6,235 | Reused |
| Same natural words, raw dataset order | 256 words | 10 | 35,969,249,723 | 6,356 | Reused |
| Full-inventory global slot prefix | 256 words | 10 | 913,053,214 | 118 | Newly executed |

The global prefix used 97.42% less gas than natural Solidity cleanup and 97.46% less than the
equal-batch raw-natural control. Unlike the preceding same-subset experiment, this intentionally
changes the selected word population to a narrow global slot interval. The trace-backed decrease
in `OnBlockLink` counts supports the expected KAMT-prefix locality benefit, but those charges include
actor/message overhead and are not exact KAMT modification counts. This one interval does not
establish an aggregate saving for the complete cleanup.

Global selection/validation of the full 4,476,875,394-byte JSONL took 102.26 seconds; the complete
invocation, including setup, identity checks, execution, and restoration, took 125.07 seconds with
82,072 KiB peak RSS. The global run cleared all 2,470 selected words, preserved 3,871 untargeted
metadata-word sentinels plus the original non-target sentinels, and restored the exact imported root.
All cases use the original helper runtime fingerprint recorded above.
`global-prefix-selection-smoke.json` independently compares bounded-heap selection with a full sort
and verifies that a low-slot piece outside the first 512 enters the global selection.


### Typed, locality-batched metadata cleanup

`contracts/TypedMetadataCleanup.sol` prototypes metadata-only operations. It has no arbitrary-slot
write entry point. Both entry points accept a whole batch in caller-selected order:

- `cleanup((uint32 dataSetId,uint32 pieceId,uint8 target,bytes key)[])` uses ordinary ABI tuples.
- `cleanupPacked(bytes)` accepts concatenated records:
  `dataset:u32 | piece:u32 | target:u8 | keyLength:u16 | key:keyLength`, with big-endian integers.
  Target `0` deletes one mapping value; target `1` deletes the key array and requires an empty key;
  target `2` replaces a value with the same number of zero bytes, preserving its length.
  Invalid tags, truncated records, and nonempty list keys revert the entire batch.

Every storage address is derived inside Solidity from the historical metadata mappings at roots
13/14. Solidity `delete` clears complete strings/arrays, including long-string payloads. Empty keys,
binary keys, and the literal key `list` are distinct from the tagged list operation. Lists may be
deleted before their values: subsequent value operations require only the explicit identity/key,
not the stored list. This intentionally permits orphaned metadata until its scheduled deletion.
Operations are idempotent while metadata writes remain disabled.

Smaller integer types do **not** compress normal ABI words. A single-array call costs
`68 + sum(192 + ceil32(keyLength))` calldata bytes with ordinary tuples. Packed calls cost
`68 + ceil32(sum(11 + keyLength))` bytes. The packed prototype supports keys up to 65,535 bytes;
it is not a replacement for the ordinary ABI when a longer historical key must be addressed.
The runner checks that every inventoried dataset/piece ID fits `uint32`; this is a historical
cleanup constraint, not a change to FWSS's general ID types.

`typed_metadata_cleanup.py` compares natural batches, locality-sorted batches of the exact same
piece population, and a globally selected prefix of whole operations. Its scheduling anchor is the
start of an operation's longest contiguous run of occupied slots, choosing the lowest start on ties.
For a long value this commonly groups its payload, but its header remains elsewhere. For a list,
both its header and element/string storage are inseparable. This is a locality heuristic, not an
optimal KAMT plan or a claim that all touched words share a prefix.

The global selector streams the complete decoded inventory and retains only a bounded operation
heap. It uses the same operation count as the natural sample, **not** the same occupied-word count;
whole-object deletion cannot generally hit an exact word budget. Selected operations, target words,
receipts, full FVM traces, calldata sizes, and signed transaction sizes are persisted. Every measured
case starts at the same state, verifies all target words are zero and selected-owner untargeted
words are preserved, and finally restores the exact imported roots/runtime. Untargeted-owner word
sentinels apply to the global selection; natural cases delete their complete selected pieces.

```console
target/release/fvm-anvil --state snapshots/fwss-mainnet-6457920.press.car.zst --port 18550
uv run --script benchmarks/fwss/typed_metadata_cleanup.py \
  --inventory benchmarks/fwss/results/metadata-cleanup-6457920/inventory-plus-1000 \
  --rpc-url http://127.0.0.1:18550 \
  --pieces 512 --batch-size 128 \
  --output benchmarks/fwss/results/metadata-cleanup-6457920/typed-batches-128
```

As with the raw-word experiments, installation uses lab-only runtime replacement, **not** a real FWSS
upgrade. Production integration must retain FWSS authorization and prohibit metadata writes for the
duration of cleanup. Retain the complete off-chain plan before deleting any list: the key list is no
longer an on-chain recovery source afterward. No persistent on-chain cursor is required by this
prototype; the operator tracks submitted batches and may replay them after a failure.
`typed-batching-boundary-smoke.json` records real-runtime checks at both maximum `uint32` IDs,
empty/literal/binary/31/32/33-byte keys, short/long values with a zero payload word, list-first
orphaning followed by value deletion, idempotence, authorization, and atomic malformed-input rejection.

#### Observed typed-batch results at epoch 6,457,920

All four cases below were newly executed with 128 operations per transaction, nine transactions
per case, Solidity 0.8.26/optimizer 200, NV28/actors v18, and the same imported starting root.
The runtime SHA-256 was `a7eee2ee39b5bb77b7f2b2106f41b0ba882f5497b4c3f35c57bb9a6cb4d83c2c`.
Those historical ordinary-ABI cases used the previous `bool list` field. The current ABI uses
`uint8 target` to support length-preserving clears; its selector and runtime fingerprint differ.
The packed layout is unchanged, with target `2` newly defined.
Every inventoried metadata ID fits `uint32`: maximum dataset ID 1,559, maximum piece ID 2,197,352.
The maximum decoded key length is 17 bytes. These maxima describe recovered metadata, not
necessarily every historical ID/key whose mapping preimage is no longer recoverable.

| Plan | Operations | Target words | Total FVM gas | Calldata bytes | `OnBlockLink` charges |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ordinary ABI, natural order | 1,032 | 2,470 | 35,904,966,424 | 215,396 | 6,264 |
| Ordinary ABI, locality order, same pieces | 1,032 | 2,470 | 33,863,985,914 | 215,396 | 5,962 |
| Packed, locality order, same pieces | 1,032 | 2,470 | 33,565,390,475 | 17,924 | 5,962 |
| Packed, full-inventory locality prefix | 1,032 | 2,451 | 20,142,415,645 | 16,388 | 3,516 |

The first three cases delete the exact same 520 values and 512 lists from 512 pieces/14 datasets.
Scheduling reduced gas by 5.68%; packing then reduced calldata by 91.68% and total gas by 0.88%.
Trace `OnChainMessage` totals fell from 286,768,967 gas to 30,055,367 gas with packing. These are
Filecoin inclusion charges, not an Ethereum 4/16-gas-per-calldata-byte estimate.

The global selection contains 388 values and 644 lists from 1,032 pieces/36 datasets. Its 39.99%
lower gas than packed same-piece ordering is a different-population comparison: operation counts
match but word counts do not. All 2,451 target words were cleared and 2,706 untargeted sentinels
preserved. Whole-string/list writes necessarily reach outside their scheduling-anchor interval.
This explains why raw single-word global-prefix results are not a realistic savings prediction.
No full cleanup, optimality proof, or maximum-batch search was performed; 128 operations is an
observed successful batch size, not a capacity limit.

Results and traces: `results/metadata-cleanup-6457920/typed-batches-128/summary.json`.
Every case verified its targets and preservation sentinels; final restoration verified exact
imported state roots and runtime. Packed boundary checks additionally proved list-first orphaning
and later value cleanup, plus whole-transaction rollback for malformed trailing records.

### Adaptive payload clearing and deletion

Target `2` uses ordinary Solidity assignment:
`pieceMetadata[datasetId][pieceId][key] = new string(currentLength)`. For long strings this clears
the payload but preserves the encoded length header. Short strings still store their contents and
length together; the scheduler deletes these directly instead. It also directly deletes long values
whose payload words are already zero.

`adaptive_metadata_cleanup.py` schedules a ready frontier, not global clear/delete phases:

1. Initially enqueue long-value clears, direct deletes for embedded/already-zero values, and list deletes.
2. Prioritize by the **highest modified slot** in each operation's complete footprint, not one selected
   anchor. Unchanged header writes are not modified slots, although they still require reads/execution.
3. Submit the lowest-priority ready operations as a batch. Only after successful execution and storage
   verification enqueue the corresponding header deletions. They immediately compete with other clears
   and deletes; there is no requirement to finish clearing every payload first.

Deleting an already-cleared long value still uses Solidity `delete`, so it may read/rewrite zero payload
words. Those are no-op mutations, not extra occupied-word removals. Lists retain whole-array deletion;
no list-payload-only operation was added.

The full-inventory experiment keeps the lowest `N+1` initially ready operations in a bounded heap and
executes at most `N` operations, adding newly unlocked deletes as it proceeds. This is a ready-prefix
measurement, **not** complete cleanup of its selected owners or a full-state migration. Its preserved
owner words, including retained headers, are checked explicitly. A separate finite 512-piece case runs
the same dependency scheduler to completion and verifies every metadata word disappears.

`MetadataNoopProbe.sol` supplies metadata-address-derived controls for same-value header writes:
read versus same-value `SSTORE`, both with and without payload mutations. Real NV28/v18 execution
produced zero `OnBlockLink` charges for both read-only-header and same-header-write transactions.
Payload clearing produced six link charges with identical link-gas totals whether or not the header
was rewritten. Execution gas still increased for the redundant write. The actual target-2 Solidity
assignment also produced those same six link charges and preserved the header at `0x77`.
Results: `results/metadata-cleanup-6457920/clear-value-proof/`.

`adaptive-scheduler-smoke.json` records immediate followup eligibility before unrelated clears finish,
direct deletion of embedded/already-zero values, and real-runtime coverage of empty, short, long,
binary-key, and zero-payload-word cases in both ABI encodings. All runtime checks restore the exact
starting roots. These are lab runtime replacements, not production-authorized FWSS upgrades.

```console
target/release/fvm-anvil --state snapshots/fwss-mainnet-6457920.press.car.zst --port 18553
uv run --script benchmarks/fwss/adaptive_metadata_cleanup.py \
  --inventory benchmarks/fwss/results/metadata-cleanup-6457920/inventory-plus-1000 \
  --rpc-url http://127.0.0.1:18553 \
  --pieces 512 --operations 1024 --batch-size 128 \
  --output benchmarks/fwss/results/metadata-cleanup-6457920/adaptive-batches-128
```

#### Observed adaptive results

These newly measured cases used the same epoch-6,457,920 archive, NV28/v18, Solidity 0.8.26 with
optimizer 200, packed encoding, and a 128-operation batch ceiling. Runtime SHA-256:
`a75bf7b98e8df97de96183f4f1d7f1b043ca6117154a1f5cdba90e5508d0da50`.
The changed runtime requires a newly measured direct control rather than reuse of older-runtime gas.

| Workload | Operations | Transactions | Occupied words cleared | Total FVM gas | `OnBlockLink` charges |
| --- | ---: | ---: | ---: | ---: | ---: |
| Complete same 512 pieces, direct deletes | 1,032 | 9 | 2,470 | 33,589,965,687 | 5,962 |
| Complete same 512 pieces, adaptive | 1,475 | 13 | 2,470 | 34,507,543,041 | 5,840 |
| Global ready prefix, partial owners | 1,024 | 8 | 2,039 | 3,956,471,532 | 358 |

The complete adaptive case issued 475 payload clears, 488 value deletes, and 512 list deletes.
It skipped the 32 already-empty mapping values included as no-op deletes in the direct control.
All 475 dependent value deletes occurred after their clears; the first appeared in the second
transaction while unrelated clears continued. For this small, fixed population the extra operations
and transactions outweighed write locality: total gas increased 2.73%, despite 2.05% fewer link charges.

The global ready prefix issued 1,014 payload clears, nine direct value deletes, and one list delete.
All 2,039 modified words occupy first-byte prefix `00` and only 15 first-two-byte prefixes, ranging
from `0x00000074c8a1add7ccaa50500c0a4e4ad5c793af1f22846e9739f1cb3ea3f635` through
`0x000e285b56a3066c7397991a8257163a549e88339141fef2509c32ae6b3a5218`.
There are no mandatory scattered header mutations in that write set. All 3,094 untargeted owner words
and non-target sentinels were preserved, including retained length headers. Reads of those scattered
headers still cost gas. This prefix had no newly unlocked header deletion become competitive within
its first 1,024 operations; it is not evidence for whole-cleanup gas savings.

Every complete-case metadata word was cleared; every target-2 operation preserved its length header.
Final restoration verified the exact original imported roots and runtime.
Results, actual transaction calldata, selections, and full traces:
`results/metadata-cleanup-6457920/adaptive-batches-128/summary.json`.
