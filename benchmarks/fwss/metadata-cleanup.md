# Permissionless legacy metadata cleanup

This process targets [Filecoin Services PR #658](https://github.com/FilOzone/filecoin-services/pull/658), source revision `35c163640aa20c65da04b481a460ab416af3df9c`. Recheck the packed protocol and deployed facet bytecode if the PR changes. It removes **all retained legacy per-piece metadata**, not just metadata for removed pieces. Dataset metadata, indexed metadata, PDP piece state and payment state are not cleanup targets.

## Preconditions and completeness

1. Deploy the PR's complete upgrade, including the authorization/proving/dataset changes that stop writing and opportunistically deleting legacy metadata. Installing only the cleanup selector is insufficient on a live network.
2. Select an unpatched, dependency-complete Press archive from a pinned canonical state. Existing `snapshots/fwss-mainnet-v1.5.0-height-6530880.press.car.zst` is a **pre-upgrade rehearsal source**, network version 29 / actor bundle 19; do not represent it as post-upgrade mainnet state. Follow the [snapshot preparation procedure](../../docs/press-recipes.md).
3. Export and preserve the inventory **before the first cleanup operation**. Keep the original inventory and plan until retirement of the facet. Zeroing a key/length first is safe for the contract but destroys information needed to rediscover payloads from a later snapshot.
4. Establish historical piece-ID high-water marks for every historical dataset, including terminated/deleted datasets. Current PDP counters can shrink; FWSS's retained packed client-nonce values provide useful candidate high-water marks, not authenticated mapping preimages. An external historical allocation index can supply a JSON object of dataset ID to **exclusive** bound, e.g. `{"12": 48000, "19": 73}`. Derive each bound as one plus the largest piece ID ever allocated through the snapshot, not the current live-piece count. Include allocations made before packed nonce encoding existed and preserve the index's chain range, event versions, canonical block hashes and extraction provenance beside the JSON. A complete, canonical history index is the evidence that bounds are exhaustive; a supplied JSON file alone is not.

The scanner is exhaustive **within the chosen dataset/piece ranges and retained key arrays**. It cannot invert Solidity mapping hashes. A zero-hit padding probe does not prove historical completeness. Unknown keys whose array entries were already erased require an earlier inventory or historical addition calldata/index records; extending piece bounds cannot recover those string preimages. Do not remove the facet or declare all metadata gone while these gaps remain unresolved.

## 1. Build the immutable inventory

```bash
cargo build --release -p fvm-anvil-state --example fwss_metadata_inventory

target/release/examples/fwss_metadata_inventory \
  snapshots/fwss-mainnet-v1.5.0-height-6530880.press.car.zst \
  benchmarks/fwss/results/cleanup-slots --export-slots-only

target/release/examples/fwss_metadata_inventory --intersect-slots \
  benchmarks/fwss/results/cleanup-slots \
  benchmarks/fwss/results/cleanup-inventory \
  --piece-bounds historical-piece-bounds.json --extra-pieces 1000
```

Omit `--piece-bounds` only for explicitly bounded discovery/rehearsal, not a completeness claim. `--max-piece-bound N` on the export sets the resource guard (default ten million per dataset); increase it deliberately if authentic historical bounds exceed it. Export and discovery refuse existing output files. The direct `ARCHIVE OUTPUT_DIR` mode combines export and point-lookup discovery; the two-stage merge intersection avoids binary-searching the large occupied index for every candidate piece.

Outputs:

- `occupied-slots.bin`: every FWSS KAMT entry, sorted by raw unsigned 256-bit slot; fixed 64-byte `slot || value` records. It includes non-metadata state. Do not mutate this file or its hard-linked copies while mapped.
- `occupied-slots.json`: actor identity, source epoch/state roots, FWSS storage root, PDP bounds, nonce-shaped candidate bounds and explicit historical bounds.
- `pieces.jsonl`: dataset/piece identity, raw key/value bytes, and exact nonzero owned slots. Includes both live and deleted pieces/datasets.
- `queues.jsonl`: every historical dataset's queue length, derived storage locations and whether its FWSS payment rail still exists.
- `summary.json`: reconciled counts, per-dataset ranges, distributions, coverage limitations and discovery timing.
- `added-range-hits.tsv`: metadata found above the recovered bound. Investigate every hit and extend/reconcile historical bounds before completion; padding is diagnostic, not an allocation index.
- `unresolved_nonce_candidates.jsonl` in the export directory: values shaped like packed nonces but outside the probe guard. These are unverified candidates, not automatically genuine missing datasets or proof of corruption.

External GNU `sort` uses 128 MiB and two workers. The exporter periodically reloads the KAMT to bound its cache; the slot index is disk-backed. Allow space for the full 64-byte export, candidate/hit sorting files, decoded metadata, SQLite sorting and the transaction plan. Identical counts alone do not authenticate files: retain immutable artifacts and their provenance together.

## 2. Construct the sequence

```bash
uv run benchmarks/fwss/cleanup_plan.py \
  --inventory benchmarks/fwss/results/cleanup-inventory \
  --output benchmarks/fwss/results/cleanup-plan \
  --max-operations 256 --max-writes 256 \
  --max-packed-bytes 16000 --queue-pops 128
```

These are conservative planning limits, **not measured network maxima**. Tune them on equivalent imported state using the deployed runtime; the executor checks current gas and signed-envelope size before broadcast. Use a new output directory for each plan and retain an in-use plan unchanged.

### Ordering algorithm

1. Pop **live dataset queues first**. A nonempty old queue otherwise causes a payment-rate refresh on every proving-period callback. Pops retain that signal in `pieceRemovalsPending`; they do not erase metadata. Pop deleted-dataset queues next. Queue operations are separate from the globally sorted phase because they read/mutate their lengths, must pop tail-first, and can write a distant live-dataset flag.
2. Derive independent writes for value heads (kind 0), value payloads (1), key-array lengths (2), key heads (3), and the detached payload word of every 32-byte key (5). Long value runs contain up to four adjacent words. Skip operations with no nonzero target; deduplicate identical operations. Validate every inventoried nonzero metadata slot is addressable. Reject keys/values/IDs outside the PR's bounds rather than silently omit them.
3. Globally sort metadata operations by their first raw storage slot, with deterministic packed-operation tie breaking. Keep bounded contiguous runs atomic. **Do not sort by dataset, piece, operation kind, or `keccak(slot)`**: the EVM actor KAMT uses the raw slot as its key.
4. Greedily fill transactions under operation, write and packed-byte ceilings. Never insert an external call between operations: the KAMT flushes across that boundary. Heads may precede their payloads; unlike the previous experimental typed cleaner, this PR's operations do not depend on retained string lengths or key-array contents.

Why this is near-optimal for locality: all descendants of any KAMT prefix are consecutive in lexicographic order. Sorting therefore concentrates modifications into the smallest available trie subtrees rather than repeatedly revisiting scattered paths. It also reduces transaction count by filling batches, rather than selecting a small cheap prefix and leaving the expensive remainder unplanned. Atomic runs and transaction capacity can split prefix groups; there is no claim of a globally optimal FVM gas schedule.

`manifest.json` reports a checkable locality bound at depths 0, 5, 10 and 15 bits. For each prefix, let `O`, `W`, `B` be the count of touching operations, their writes in that prefix, and their packed bytes. Every valid schedule needs at least

```text
max(ceil(O / maxOperations), ceil(W / maxWrites), ceil(B / maxPackedBytes))
```

transactions touching that prefix. Sum this over prefixes to obtain a lower bound; compare it with the plan's distinct-prefix touches per transaction. `metadataLocalityBound[*].ratio` bounds the plan's distance from optimum **for that fixed-depth prefix-touch model**, not actual gas. It excludes queue work and does not model variable KAMT occupancy, collapse, hashing, interpreter cost or blockstore cache behavior. A ratio materially above one is a reason to tune batch limits/repartition and compare actual FVM traces before calling the schedule near-optimal for the workload.

SQLite performs disk-backed global sorting; Python memory does not retain all operations. Outputs are immutable `batches.jsonl` and `manifest.json`, including source identities, inventory digests, packed-protocol revision, counts, locality metrics, batch limits and a digest of the entire batch stream. No unrestricted storage-slot deletion contract is used.

## 3. Rehearse, execute and resume

Rehearse against the actual imported FVM state with the exact deployed/candidate facet, not a `revm` simulation. For a pre-upgrade archive, install the complete candidate upgrade for lifecycle compatibility testing. A dev-only selector routing patch can test the cleanup entry point but does not prove the complete upgrade. Check target-slot deletion, live rate-refresh flag preservation, unrelated-slot sentinels, gas and native message admission. Restore the rehearsal snapshot afterward.

After dev-only `anvil_setStorageAt` setup, mine a checkpoint before using the executor: it validates the route and storage at explicit canonical block numbers, not the unmined control overlay.

Execution requires explicit RPC, chain ID, a trusted deployed facet **Keccak-256 runtime code hash**, and a funded signer. Obtain the hash from independently verified deployed bytecode/source artifacts, not from an untrusted RPC and then blindly accept it. The executor checks `implementation(bytes4)` for the cleanup selector and hashes the routed delegate's runtime. Keep the signer nonce exclusive to this process.

```bash
# Set FWSS_CLEANUP_PRIVATE_KEY securely in the environment; never put it in a plan.
uv run benchmarks/fwss/cleanup_execute.py execute \
  benchmarks/fwss/results/cleanup-plan \
  --rpc-url "$RPC_URL" --expected-chain-id "$CHAIN_ID" \
  --expected-facet-code-hash "$FACET_CODE_HASH" \
  --journal benchmarks/fwss/results/cleanup-journal

uv run benchmarks/fwss/cleanup_execute.py verify \
  benchmarks/fwss/results/cleanup-plan \
  --rpc-url "$RPC_URL" --expected-chain-id "$CHAIN_ID" \
  --expected-facet-code-hash "$FACET_CODE_HASH"
```

Use an appropriate confirmation depth for the target network; inspect `--help` for gas, envelope-size, timeout and confirmation controls. Submit sequentially, not with concurrent nonces: KAMT gas changes as deletion progresses. The queue phase must finish and its lengths read back as zero before metadata execution starts. Every successful receipt is followed by storage verification. Already-cleaned slots remain safe if another permissionless caller made progress.

The endpoint must support JSON-RPC batches, canonical block history, historical receipt-block storage reads, and Filecoin/FVM-backed `eth_estimateGas`; a vanilla Ethereum opcode estimate is not sufficient. Verification reads up to `--read-batch-size 128` slots per HTTP request by default and validates response IDs rather than response order. The separate `verify` command needs no signer; pass the same `--journal` explicitly if it is outside the plan directory and receipt-history verification is desired.

The journal binds the plan, chain, proxy and signer, and writes signed raw bytes/hash **before** broadcast. Rerunning with the same journal resumes rather than allocating a replacement nonce. A pending timeout is not permission to send a new transaction. `--rebroadcast-pending` explicitly permits only identical signed-byte rebroadcast; a revert, conflicting nonce or noncanonical receipt stops execution for investigation. Signed transactions in the journal are public broadcast material, but protect the journal against alteration and concurrent writers.

If a batch exceeds current native FVM gas/admission limits, do not suppress the failure or trust Ethereum opcode gas. Stop, use the original inventory to build a smaller plan in a new directory, and verify canonical progress. Independent metadata operations can be replayed; oversized queue pops must also be reduced with the planner. Preserve the old plan/journal as evidence. Do not rebuild discovery from partially erased key arrays.

## 4. Reconcile and retire

- Verify every target in the original immutable plan is zero at the canonical head; verify every original removal queue is empty.
- Reconcile historical allocation bounds, padding hits and any unknown key preimages. A successful plan verifies its known targets, not historical completeness.
- Re-extract post-cleanup state and scan the same historical ranges; compare unchanged non-target state, allowing normal live-chain activity and the intentional live `pieceRemovalsPending` flag. An occupied-slot count alone is not a metadata-completeness check.
- After canonical completion and a sufficiently deep reorg window, remove the temporary facet/selector/errors through the normal reviewed FWSS upgrade. Archive original inventory, bounds provenance, manifests and receipts so a reorg does not destroy recovery information.

## Exercised snapshot and evidence

This section summarizes the measured inventory and rehearsal outcomes. Detailed run artifacts under `benchmarks/fwss/results/cleanup-658-{slots,inventory,plan,proof}` are local outputs and are not committed.

On the epoch 6,530,880 snapshot, bounded discovery identified 22,325,764 nonzero metadata words belonging to 4,465,423 pieces and 4,485,536 keys. All 1,908 historical dataset IDs were scanned; the 1,000-piece-per-dataset padding added no hits. There are 38,026 queued entries in 131 datasets; 49 live queues contain 10,733 entries and precede deleted queues. Historical bounds were not independently authenticated, so these remain recovered-range counts, not an unconditional historical total.

The complete generated plan contains 87,441 transactions at the documented limits. Across the fixed-depth locality model at 0/5/10/15 bits, its 382,762 prefix touches are within **6.16%** of a 360,559-touch lower bound for the same atomic operations and capacity limits. This certifies near-optimality only for that model, not gas: the gap permits at most 5.80% reduction from current aggregate touches and need not be attainable. The 15-bit bound alone is looser (21.81%). Larger batches change those limits, so this certificate does not bound their possible improvement.

An equivalent-work comparison cleared the same first 512 inventoried pieces: 1,995 operations and 2,470 target words, ten transactions per ordering, with exact original roots restored between cases.

| Ordering | Total FVM gas |
| --- | ---: |
| Dataset/piece iteration | 36,580,531,476 |
| Global raw-slot order | 28,175,529,469 |

The reduction is trace-backed: `OnBlockLink` dropped from 30,438,005,458,000 to 23,193,946,052,800 milligas. These are newly measured local FEVM results for that sample, not reused lifecycle benchmark measurements or a maximum-batch search.

A separate eight-transaction fixture exercised all six operation kinds, maximum uint40 IDs, empty/31/32-byte keys and values through 128 bytes. Execution, resume and read-only verification passed; the live rate-refresh flag preserved neighboring packed fees, the deleted dataset flag stayed zero, eight unrelated sentinels stayed unchanged, and resume used no extra nonce. A mined-receipt/storage-read failure was recovered from the journal. Three focused planner tests passed, and the full generated batch stream passed the executor's offline plan validation.

The initial small rehearsal routed only the exact PR cleanup facet into the imported proxy; it did not exercise the complete upgrade, proving/payment lifecycle, production transactions or network batch maxima. Full-inventory execution and accounting are covered below.

## Global selection versus random slots

The earlier 22.98% result is `(36,580,531,476 - 28,175,529,469) / 36,580,531,476`: reordering **the same** first 512 pieces. It does not measure selecting adjacent operations from the complete global queue.

The stronger comparison below was newly measured on the identical archive and pinned PR facet:

- Dataset **1293**, whose inventory contains 2,197,350 metadata-bearing pieces.
- Global selection: take operations in global queue order, filtered to this dataset, until exactly **2,560 nonzero slots** are covered. This touches 2,021 pieces and completes none; detached heads/payloads are deliberately independent.
- Random selection: uniformly sample 1,024 pieces without replacement, seed **658**, then select operations matching the global sample's `(kind, packed length, target-word count)` distribution. The matched subset touches 958 pieces and completes 58. This is not labeled “512 pieces cleaned.”
- Every case clears exactly **2,560** initially nonzero slots using **2,023 operations** and **35,925 packed bytes**: 471 key-array lengths, 516 value heads, 537 two-word value payloads and 499 key heads.
- Restore identical FVM roots before each case. Verify all target words become zero, all remaining inventoried words of selected owners stay unchanged, and eight unrelated sentinels remain unchanged. Preserve every signed-envelope size, receipt and Filecoin trace.

| Selection and execution | Write ceiling | Transactions | Total FVM gas | FVM gas / cleared slot |
| --- | ---: | ---: | ---: | ---: |
| Random, piece order | 256 | 11 | 37,351,879,345 | 14,590,578 |
| Same random operations, slot order | 256 | 11 | 28,568,138,276 | 11,159,429 |
| Global queue prefix | 256 | 11 | 2,300,818,233 | 898,757 |
| Same global operations | 512 | 6 | 2,069,946,412 | 808,573 |
| Same global operations | 1,024 | 3 | 1,930,263,918 | 754,009 |

At the same 256-write ceiling, global selection saves **93.84%** against random piece order and **91.95%** against sorting the random operations. Increasing global batches to 1,024 writes saves another **16.11%**, or **94.83%** combined against the random piece-order baseline. Global selection reduces `OnBlockLink` from 31,045,718,226,800 to 913,166,880,000 milligas at the same ceiling; the advantage is predominantly fewer storage blocks relinked, not shorter calldata.

The largest observed 1,024-write transaction used 766,749,007 FVM gas and 14,579 signed Ethereum bytes. This proves that case succeeds, **not** a network maximum or that every later batch fits. The full-inventory production plan retains its conservative defaults; tune both `--max-operations` and `--max-writes` together, and keep live native-FVM estimation, gas headroom and signed-envelope checks.

### Reproduce the selection experiment

`cleanup_compare.py` is lab-only: it requires chain ID 31337, checks the imported archive root and baseline FWSS version, deploys the fingerprinted cleanup artifact, patches only its selector route with Anvil controls, and restores the original roots afterward. It does not exercise the complete PR upgrade. Reuse the already measured JSON for reporting; a new output directory starts new measurements.

```bash
target/release/fvm-anvil \
  --state snapshots/fwss-mainnet-v1.5.0-height-6530880.press.car.zst \
  --network-version 29 --port 18558

# In another terminal, set FWSS_CLEANUP_PRIVATE_KEY to the local funded signer.
uv run benchmarks/fwss/cleanup_compare.py \
  --inventory benchmarks/fwss/results/cleanup-658-inventory \
  --plan benchmarks/fwss/results/cleanup-658-plan \
  --facet-artifact benchmarks/fwss/results/cleanup-658-selection/facet-artifact.json \
  --facet-build benchmarks/fwss/results/cleanup-658-proof/facet-build.json \
  --output benchmarks/fwss/results/cleanup-658-selection/NEW-RUN \
  --rpc-url http://127.0.0.1:18558 \
  --dataset 1293 --random-pieces 1024 --slots 2560 --seed 658

jq '.cases | map({name,words,gasUsed,gasPerSlot,transactions:(.transactions|length)})' \
  benchmarks/fwss/results/cleanup-658-selection/matched/comparison.json
```

The artifact is the previously compiled exact PR facet; no candidate rebuild or source substitution was needed for this comparison. Creation/runtime hashes and source fingerprints are recorded in local build and measurement outputs. The reproduction commands require these locally generated artifacts.

### Further reductions

1. **Use global operation locality, not whole-piece grouping.** This is the dominant measured gain. Returning to a selected piece to finish its distant slots sacrifices it.
2. **Tune larger batches against actual FVM gas and envelope bytes.** This provided the additional measured gain above; fixed 256-write defaults are not a gas optimum.
3. **Consider prefix-aware batch cuts.** A bounded dynamic program over the sorted queue can minimize an additive transaction-plus-prefix-touch cost, allowing an earlier cut when it avoids splitting a subtree. Weight node depths using traces and include transaction overhead. This remains an unmeasured surrogate optimization, not proven additional gas savings.
4. **Queue-specific contract changes need separate evidence.** Clearing a bounded queue tail with one final length write, or transferring its rate-refresh signal only on the final pop, may reduce bookkeeping/distant flag writes. Preserve live/deleted behavior, packed fee neighbors and every rate-refresh consumer. No queue change was implemented or benchmarked here.
5. **Optimize decoding/hashing last.** Reducing repeated header reads or key-memory copies is a candidate, but storage relinking dominated the random baseline. Retain restricted metadata targets and the no-external-calls loop; an unrestricted slot clearer is not an equivalent safe optimization.

This is one deterministic queue window and random seed, not a full-cleanup gas estimate. Do not extrapolate its percentage to queues, every dataset, every later trie state or the entire cleanup.

## Complete inventory execution and accounting

`cleanup_rehearse.py` drives the real `cleanup_execute.Cleanup` executor against a quiet local imported node. It coalesces consecutive source metadata batches up to 1,024 operations, 1,024 target words and 16,000 packed bytes; queue batches remain unchanged. The concatenated packed-operation stream is authenticated before and after coalescing. No new inventory selection, sorting or sampled extrapolation replaces the full plan.

```bash
# Start a fresh NV29 node as above, then use the funded local signer environment.
uv run benchmarks/fwss/cleanup_rehearse.py \
  --source-plan benchmarks/fwss/results/cleanup-658-plan \
  --output benchmarks/fwss/results/cleanup-658-full \
  --facet-artifact benchmarks/fwss/results/cleanup-658-selection/facet-artifact.json \
  --facet-build benchmarks/fwss/results/cleanup-658-proof/facet-build.json \
  --rpc-url http://127.0.0.1:18558

uv run benchmarks/fwss/cleanup_reconcile.py \
  --run benchmarks/fwss/results/cleanup-658-full \
  --inventory benchmarks/fwss/results/cleanup-658-inventory \
  --rpc-url http://127.0.0.1:18558

uv run benchmarks/fwss/cleanup_stats.py \
  benchmarks/fwss/results/cleanup-658-full --require-complete
```

Keep the node running and untouched until reconciliation finishes. Like the smaller rehearsal, setup patches only the exact cleanup facet's selector; no background proving or metadata writers run. This isolates cleanup gas and preservation, not full-upgrade lifecycle compatibility. The original archive stays read-only. This command does **not** send production messages.

Artifacts:

- `plan/`: authenticated coalesced plan and source-plan digest.
- `setup.json`: archive roots, baseline version, deployed runtime/source fingerprints, route slot, node binary digest and setup receipt.
- `journal/`: every signed raw message and canonical receipt, including estimates, declared gas limits and actual gas.
- `messages.jsonl`: per-message timing, slot/operation counts, gas, envelope size and cumulative totals.
- `execution.log`: executor confirmations; `traces/`: compressed trace checkpoints at phase changes, every 1,000th message and the final message.
- `execution.json`: final all-target zero verification at a pinned canonical block and execution wall time.
- `reconcile.json`: every original occupied FWSS word accounted for as a zero target or a checked non-target, all historical queues empty, live refresh flags correct with neighboring packed bytes preserved, and the sole setup route change identified. Target-zero evidence is the executor's final full readback, not another redundant read of every metadata word.
- `report/messages.csv`: one row per message; `report/summary.json`: exact gas sums, per-phase totals, gas distributions, progress deciles, envelope limits and timing models. The reporter validates persisted signatures/receipts and requires complete coverage when passed `--require-complete`. It can be rerun offline without repeating cleanup.

The report separates:

1. **Measured gas used:** sum of all successful cleanup receipts; excludes deployment/setup.
2. **Measured declared gas limits:** sum of all cleanup message limits, which govern block admission and packing. Native estimation and the executor's headroom mean this exceeds used gas.
3. **Nominal capacity scenarios:** 10 billion gas per block, a 30-second epoch, and an optimistic five-distinct-block epoch give a nominal 50 billion gas ceiling. The base-fee target is half the ceiling. Expected Consensus's `e=5` counts election wins, not a guarantee of five distinct blocks: multiple wins by one miner still produce one block, and null rounds, failures and duplicated messages reduce useful capacity. Capacity-share calculations are scenarios, not observed mainnet throughput or an inclusion SLA.
4. **Sequential receipt cadence:** the unmodified executor waits before sending the next message. Roughly one message per epoch means approximately `messageCount * 30 seconds`, independent of the smaller gas-only bound. Pipelining would require a different execution policy; it was not silently assumed.

Network assumptions: [Lotus gas parameters](https://raw.githubusercontent.com/filecoin-project/lotus/master/build/buildconstants/params_shared_vals.go), [epoch duration](https://raw.githubusercontent.com/filecoin-project/filecoin-docs/main/core-concepts/filecoin-virtual-machine/blocks-and-tipsets.md), [Expected Consensus](https://spec.filecoin.io/algorithms/expected_consensus/) and [gas/admission mechanics](https://spec.filecoin.io/systems/filecoin_vm/gas_fee/).

### Full execution results

A successful end-to-end run of the recovered inventory was measured on the same NV29 archive and exact pinned PR facet. **All 21,983 cleanup messages succeeded**, executing 17,881,108 operations. This is newly measured full-inventory execution, not extrapolation from the earlier selection sample. Deployment/setup gas is excluded.

| Phase | Messages | Actual FVM gas used |
| --- | ---: | ---: |
| Live queues | 46 | 9,801,060,296 |
| Deleted queues | 117 | 19,401,207,540 |
| Metadata | 21,820 | 10,671,042,705,378 |
| **Total** | **21,983** | **10,700,244,973,214** |

Total declared gas limits were **16,050,453,210,050**. Used gas per message had median **488,917,964**, p95 **500,586,588**, p99 **505,539,345**, and maximum **562,160,116**; the maximum metadata-only message used **516,806,381**. The largest signed Ethereum envelope was **15,092 bytes**. No batch limit search was performed.

The limits are approximately 1.5 times actual gas because this node's native estimate includes 25% headroom and the executor adds another 20%; their product is 1.5. This is measured admission reservation, not extra executed work. Do not confuse gas-limit capacity consumption with actual gas burned or a production fee estimate.

Verification at canonical block **6,552,865**:

- All **22,325,764** inventoried nonzero metadata words were cleared.
- All **38,026** original queue entries and 131 nonempty queue lengths were targeted; 65 queue words were already zero. Full final readback checked **22,363,921** unique zero targets.
- All **24,007,508** original occupied FWSS words were accounted for: **22,363,856** originally nonzero targets plus **1,643,652** checked non-target words. Only the specified live queue flag changes and the setup route were allowed.
- All **1,908** historical queues were empty; all 49 live refresh flags and every neighboring packed byte matched expectations.
- The offline reporter validated every signed journal record, receipt, nonce, gas sum and final target-check count with no issues.

Local monotonic runtime was **95.03 minutes** for execution plus complete final target readback; summed per-message execution/verification time was **65.29 minutes**. Independent original-state/non-target reconciliation took another **3.60 minutes**. These are local runtime observations, not Filecoin inclusion estimates.

#### Duration from measured gas limits

For an assumed average cleanup admission budget `C` gas per 30-second epoch, the gas-capacity component is `16,050,453,210,050 / C * 30 seconds`. Messages are indivisible; at low average budgets they must be spaced across epochs while still fitting a single block. Gas does not carry between epochs. These budgets are assumptions, not measured spare mainnet capacity.

| Average cleanup budget per epoch | Gas-limit capacity time |
| --- | ---: |
| 0.5 billion gas | 267.5 hours |
| 2.5 billion gas | 53.5 hours |
| 5 billion gas | 26.8 hours |
| 50 billion gas, optimistic nominal ceiling | 2.68 hours |

The nominal-ceiling row assumes five distinct fully available blocks and ignores duplication, missing blocks and competing traffic; it is not a practical completion prediction. Congestion, fee choice, message-pool policy, reorgs, confirmation depth and block packing can lengthen every scenario.

The current sequential executor would instead take roughly **7.63 days** at one receipt per 30-second epoch, before extra delays/finality. Approaching the shorter capacity times requires a separate nonce-safe pipelined submission policy; it was not used or assumed by this measurement.

Detailed per-message CSVs, JSON reports and reconciliation records are generated locally by the commands above and are not committed. Re-run only the offline reporter to derive new summaries; do not repeat cleanup to relabel results.
