# FWSS benchmark methodology

## Purpose

The implemented benchmark compares supported PDPVerifier callbacks into the Filecoin Warm Storage Service (FWSS) across PDP/FWSS implementation combinations. It preserves completed measurements, exposes contract-call gas attribution, and searches for practical batch-size limits without rerunning unrelated cases.

It is a local development tool, not a complete reproducibility or archival system. Operational commands and historical archive preparation are in [`README.md`](README.md).

## Deployment-model boundary

This methodology covers the imported FWSS 1.3.0 baseline and the existing
single-implementation upgrade runner. Its local FWSS build expects a monolithic
implementation artifact. The fresh FWSS 1.5.0 ERC-8167 dispatcher/facet deployment
is implemented separately in [`tests/curio`](../../tests/curio/README.md), not by
this benchmark's `local-new` variant. Configured local variants do not establish
compatibility with arbitrary newer source trees or pin historical candidate revisions.


## Scope

The benchmark measures public transactions that invoke the supported FWSS callbacks:

| Public operation | FWSS callback |
| --- | --- |
| `createDataSet` | `dataSetCreated` |
| `addPieces` | `piecesAdded` |
| `schedulePieceDeletions` | `piecesScheduledRemove` |
| `processPieceDeletions` | None |
| `nextProvingPeriod` | `nextProvingPeriod` |
| `provePossession` | `possessionProven` |
| `deleteDataSet` | `dataSetDeleted` |

It also measures `processPieceDeletions` as the PDP storage operation that applies queued removals, `terminateService` as a direct FWSS lifecycle operation, and `cleanupPieces` as the PDP teardown operation affected by storage layout. A deletion case may execute termination and settlement as unmeasured setup, but termination also has its own independent measured case.

`storageProviderChanged` is excluded because FWSS deliberately rejects it.

The original comparison targeted:

- a new PDPVerifier storage layout used only by data sets created after the upgrade, while existing data sets continue to use the legacy layout;
- a changed PDPVerifier `PiecesAdded` event format;
- removal of FWSS per-piece metadata storage, leaving that metadata in events only;
- resumable piece removals processed separately from `nextProvingPeriod`.

The benchmark reports transaction and contract-call gas. It does not claim Solidity source-line attribution.

## Imported state and addresses

The configured archive is `snapshots/fwss-mainnet-v1.3.0.press.car.zst`, with the expected mainnet deployment at epoch 6,251,280, PDPVerifier 3.4.0, and FWSS 1.3.0. Its original ForestCAR source path and availability are not established; see [archive preparation](README.md#prepare-the-historical-archive). Validate proxy code, implementation targets, and runtime versions rather than trusting an archive filename.

| Component | Address |
| --- | --- |
| PDPVerifier proxy | `0xBADd0B92C1c71d02E7d520f64c0876538fa2557F` |
| FWSS proxy | `0x8408502033C418E1bbC97cE9ac48E5528F371A9f` |
| FilecoinPay | `0x23b1e018F08BB982348b15a86ee926eEBf7F4DAa` |
| USDFC | `0x80B98d3aa09ffff255c3ba4A241111Ff1262F045` |
| ServiceProviderRegistry proxy | `0xf55dDbf63F1b55c3F1D4FA7e339a68AB7b64A5eB` |
| FWSS state view | `0xAD28BBF18A72f728Ed816D07F5a1d7Ec40D68b5e` |

The runner calls `VERSION()` through the configured PDP and FWSS proxies and requires `3.4.0` and `1.3.0` before setup. It also compares the node's current root with the recorded imported root. These are runtime compatibility checks, not complete archive authentication.

## Repository layout

Responsibilities are split as follows:

```text
benchmarks/fwss/
├── README.md
├── methodology.md
├── suite.json
├── run.py
├── variants.py
├── chain.py
├── fixtures.py
├── operations.py
├── traces.py
└── report.py
```

- `run.py`: CLI, variant/case selection, orchestration, resumption, and atomic result writes.
- `chain.py`: `cast` commands, JSON-RPC, transaction submission, receipts, snapshots, and upgrades.
- `variants.py`: Solidity build fingerprints, source captures, library linking, deployment, and proxy upgrades.
- `fixtures.py`: imported-state patching, funding, signatures, and deterministic lifecycle pre-states.
- `operations.py`: supported operation handlers, fixed cases, postconditions, and maximum-batch probing.
- `traces.py`: Filecoin gas trace validation and contract-call attribution.
- `report.py`: arbitrary cross-variant tables and machine-readable output.
- `suite.json`: builds, variants, fixed cases, and boundary-search settings.


## Builds and variants

A build describes one PDP or FWSS implementation. A variant selects one build of each component plus when its fixtures are created. Keeping builds separate avoids rebuilding and re-recording the same implementation for combinatorial variants.


Ordinary implementation variants are configured with build and variant entries in `suite.json`. A different constructor, upgrade protocol, or public operation can require runner changes; configuration alone does not establish compatibility.

Every link reference in a local implementation artifact is resolved to the
matching Forge artifact, built from the same repository revision, included in
the build fingerprint, deployed before the implementation, and linked at its
new address. A missing, mismatched, or nested unresolved library artifact fails
build preparation. Imported library addresses under `contracts` label the
deployed baseline only; they are never linked into a local FWSS build.

`fixtureCreation` means:

- `before-upgrades`: construct each operation fixture with the imported implementations, then upgrade and measure it. This exercises existing/legacy data-set state under candidate code.
- `after-upgrades`: install the variant first, then construct and measure the fixture. This exercises state created by the selected implementations.

Set `compactEmptyMetadata` on a variant when its FWSS build accepts the compact
empty-metadata representation. The runner then signs and submits empty outer
metadata arrays for measured metadata-free `addPieces` calls. Setup performed
before an upgrade remains expanded so it is valid against the imported FWSS.

Set `separatePieceDeletions` on a PDP build that exposes
`processPieceDeletions`. The runner measures that operation for those builds
and processes queued removals as unmeasured setup before measuring
`nextProvingPeriod`. The operation is `not-applicable` for older PDP builds;
their `nextProvingPeriod` measurement continues to include PDP removal work.

For the deployed variant, `after-upgrades` performs no upgrade. Its data sets use the deployed legacy layout.

Some cases are naturally not applicable. For example, candidate `createDataSet` cannot both be measured and have its resulting data set predate the upgrade. The result is `not-applicable`, not a synthetic measurement.

## Version capture

Build provenance is deliberately lightweight. Source/build captures and upgrade records include:

- `git rev-parse HEAD`;
- `git describe --tags --always --dirty`;
- `git diff HEAD --binary` when dirty;
- `git status --porcelain`;
- creation and deployed runtime bytecode hashes;
- deployed implementation address.

A parent repository diff does not preserve dirty submodule contents. The runner also captures the FWSS build's `lib/pdp` repository when present, with dirty patches saved separately. It stops if a captured repository reports an untracked Solidity file, because `git diff` cannot preserve that file.

Captures are stored by computed build fingerprint, not only by the mutable name from `suite.json`. The variant manifest maps configured build names to fingerprints. A changed captured commit, dirty patch, or artifact bytecode produces a new capture without overwriting provenance referenced by completed cases.

The execution plan records the fvm-anvil repository revision and dirty description plus `cast`/`forge` versions. This describes the checkout, not the live node executable. Compiler metadata is recorded when present; full repositories, Git bundles, and duplicate build artifacts are not retained.

## Result storage and resumption

Each measured case is stored independently:

```text
benchmarks/fwss/results/mainnet-v1.3.0/
├── manifest.json
├── builds/
│   ├── <pdp-build-fingerprint>.json
│   ├── <pdp-build-fingerprint>.patch
│   ├── <fwss-build-fingerprint>.json
│   └── <fwss-build-fingerprint>.patch
└── variants/
    ├── deployed/
    │   ├── variant.json
    │   └── cases/
    ├── candidate-legacy/
    │   ├── variant.json
    │   └── cases/
    └── candidate-new/
        ├── variant.json
        └── cases/
```

Every result has two identifiers:

- `comparisonKey`: operation, parameters, and semantic fixture profile. Reports join variants on this value.
- `measurementId`: the comparison key plus archive, variant/build, execution-plan, and schema fingerprints. Resumption uses this value.

The execution-plan fingerprint includes the recorded archive digest, fvm-anvil repository revision/dirty description, tool versions, resolved variant configuration, build fingerprints, contract addresses, funding, case parameters, transaction gas/fee envelope, result schema, and runner-source fingerprint. Adding an unrelated case does not invalidate existing results.

On initial manifest creation the archive is hashed and imported roots are recorded. On reuse, `_manifest` checks only the archive size and reuses its recorded SHA-256; same-size replacement content is not detected by that check. Startup compares the node's current root with the manifest and checks baseline proxy versions. The execution plan does not hash or authenticate the live node binary, nor capture every live chain limit. Reuse therefore requires an unchanged archive and node build; if either changes, start a fresh node with a separate results directory. Matching identities are not a complete reproducibility guarantee.

A case is written to a temporary file and atomically renamed after its terminal outcome is fully recorded:

- committed success, revert, or out-of-gas: receipt, Filecoin trace, outcome classification, receipt/trace gas agreement, and success postconditions where applicable;
- admission failure: structured RPC rejection plus the submitted transaction envelope;
- `setup-unreachable` or `not-applicable`: deterministic reason plus fixture, configuration, comparison key, and measurement identity.

On restart, skip only terminal case files whose measurement identity still matches. Adding a batch size or variant does not invalidate unrelated cases. `--force` replaces a selected completed case.

## Deterministic setup

The benchmark sender is the first default Anvil account. The imported state requires four setup actions before variant work.

### Upgrade authority

Patch the ERC-7201 Ownable storage word on the PDPVerifier and FWSS proxies to the benchmark sender:

```text
0x9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300
```

Do not patch the ERC-1967 implementation slot. Deploy candidates and use the real announce, mine, and `upgradeToAndCall` path. Verify the adopted implementation address and runtime bytecode hash.

### Provider identity

The default Anvil account is not an imported approved provider. Repoint approved provider ID 1 to the sender and set its reverse address mapping. These current-layout slots were verified against the imported registry:

```text
provider[1].serviceProvider:
0xcc69885fda6bcc1a4ace058b4a62bf5e179ea78fd58a1ccd71c22cc9b688792f

addressToProviderId[sender]:
0xc651ee22c6951bb8b5bd29e8210fb394645a94315fe10eff2cc73de1aa75c137
```

Validate `providers(1)` and `getProviderIdByAddress(sender)` after patching. Keep this patch in setup only; it is never measured.

### USDFC and FilecoinPay

Use hex quantities with:

- `anvil_setERC20Balance(sender, USDFC, amount)`;
- `anvil_setERC20Allowance(sender, FilecoinPay, USDFC, amount)`.

Then execute the real FilecoinPay setup:

1. deposit USDFC for the payer;
2. grant FWSS operator approval with sufficient rate, lockup, and period allowances;
3. verify token balance, deposited balance, and approval.

### Signatures

Generate real EIP-712 signatures with `cast wallet sign --data` for create, add, removal, and termination operations. Do not patch signature verification or session-key state.

## Snapshots and fixtures

Anvil snapshots are in-process setup accelerators, not durable result state. The ordering depends on the variant:

```text
after-upgrades:
  import archive -> patch setup -> base snapshot
  -> configure variant -> build fixture -> measurement snapshot

before-upgrades:
  import archive -> patch setup -> base snapshot
  -> build fixture with imported code -> configure variant -> measurement snapshot

either:
  measurement snapshot -> execute one case -> revert measurement snapshot
```

This branch is required: a `before-upgrades` fixture must exist before candidate code is installed, while an `after-upgrades` fixture must be created by that candidate code.

A reverted snapshot ID is consumed. Immediately take a replacement snapshot before the next case.

Every fixed-size case and every maximum-search probe starts from the same fixture state. This prevents batch size from being confounded with a progressively changing data set. After a process restart, rebuild only the fixture needed by missing cases; completed measurements remain untouched.

Use the existing valid height-5 zero-tree CommPv2 fixture. It supplies a valid proof for every possible challenge offset without mocked randomness.

Fixture families include:

- funded sender with no data set;
- empty data set;
- data set with 128 homogeneous pieces;
- activated data set with 128 or more pieces;
- activated data set with exactly `N` scheduled removals;
- data set mined into its proof window;
- terminated and settled data set for deletion;
- deleted data set for cleanup.

Fixture size may be raised for a requested large-batch case. Setup transactions are recorded but excluded from measured gas.

## Cases

### Data-set creation

Measure:

- no dataset metadata;
- one short dataset metadata pair.

### Adding pieces

The configured fixed sizes are:

```text
1, 8, 32, 64, 128, 256, 512, 1024, 2048
```

For each size measure the cross-product of:

- no per-piece metadata and one fixed, short key/value pair per piece;
- starting piece counts 0 and 1,024.

Starting count is a real benchmark dimension: PDP tree/storage cost can differ between the first addition and an addition to an established data set. Both fixed-size cases and maximum searches use the configured starting count, and each case begins from its own snapshot.

### Scheduling and applying piece removals

This is the piece-deletion benchmark. Prepare a populated data set with enough pieces for the requested size, using one metadata pair per piece so the current FWSS metadata cleanup cost is exercised. For every configured size where reachable, measure separately:

- `schedulePieceDeletions`, which invokes `piecesScheduledRemove`;
- `processPieceDeletions`, for PDP builds that apply queued removals in a separate resumable call;
- the following `nextProvingPeriod`, which invokes the FWSS callback that deletes scheduled piece metadata.

For older PDP builds, `processPieceDeletions` is `not-applicable` and `nextProvingPeriod` applies the queued removals itself. A failure to schedule or process removals means the next-period case is `setup-unreachable`, not a next-period failure.

### Proving possession

A valid proof uses five challenges for each relevant legacy/new variant.

### Service termination, data-set deletion, and cleanup

Measure independent cases for:

- payer-initiated `terminateService`;
- `deleteDataSet` for an empty, never-activated data set;
- `deleteDataSet` for a populated data set after unmeasured termination, epoch advancement, and rail settlement;
- fixed and maximum `cleanupPieces` batch sizes from an identical snapshot of the populated deleted data set.

The measured `deleteDataSet` transaction invokes `dataSetDeleted`; it does not include the gas of termination or settlement. Those steps only construct the deletable fixture. Piece removals are measured separately in the preceding section because they exercise `piecesScheduledRemove`, `processPieceDeletions` where supported, and `nextProvingPeriod`, while `cleanupPieces` measures the PDP storage teardown that follows whole-data-set deletion.

## Failure results

Large cases are expected to fail on some variants. A failed case is data, not a runner error. Classify it as one of:

- `success`;
- `contract-revert`;
- `out-of-gas`;
- `admission-failure`;
- `setup-unreachable`;
- `not-applicable`;
- `not-run` in reports only.

Submit measured transactions asynchronously to obtain the hash before execution outcome is known, then fetch the receipt and trace. Do not depend on `cast send` returning zero for a reverted transaction.

Runner/configuration errors still stop the run. Examples include a missing contract, mismatched version, failed funding, invalid signature fixture, failed snapshot restore, or missing trace.

## Maximum-batch discovery

Run boundary discovery independently for each variant, operation, fixture, and metadata profile.

1. Treat batch size 0 as a non-measured lower sentinel and probe 1 first.
2. Continue with powers of two, always probing `min(nextPowerOfTwo, ceiling)` so a non-power-of-two ceiling is actually measured.
3. Stop at the first failure or the measured ceiling.
4. If 1 fails, report that no positive batch succeeded.
5. Otherwise, if a failure was found, binary-search between the last success and first failure.
6. Save every probe as a normal case and write a small boundary summary referencing them.

The search retains the observed adjacent success and failure without duplicate confirmation attempts. If every probe through the ceiling succeeds, it reports `successfulThrough: ceiling`, not an exact maximum.

Only an observed outcome from the measured operation can bracket its exact boundary. `setup-unreachable` stops the search without converting the prerequisite's limit into the measured operation's limit. Report the operation boundary as prerequisite-limited/unknown, include any measured `successfulThrough` lower bound, and identify the blocking prerequisite and size. Discover and report that prerequisite's boundary separately.

Boundary discovery assumes monotonic behavior for homogeneous inputs and identical fixture state. If a success is observed above a failure, mark the search non-monotonic, retain all probes, and do not report a single maximum.

Discover separate boundaries for:

- `addPieces` by metadata profile and starting state;
- `schedulePieceDeletions`;
- `processPieceDeletions` where supported;
- `nextProvingPeriod` applying FWSS removal effects;
- `cleanupPieces`.

The default ceiling is 4096. `schedulePieceDeletions`, `processPieceDeletions`, and the removal setup for `nextProvingPeriod` are capped at the contract's 2000-piece scheduling limit; other ceilings remain configurable.

## Gas traces and component attribution

Save the complete `filecoin_getGasTrace` for every transaction that reaches execution, including reverts.

For each call node compute:

```text
selfMilligas = sum(node.gasCharges[*].totalMilligas)
inclusiveMilligas = selfMilligas + sum(child.inclusiveMilligas)
```

Decode `f410...` delegated addresses to Ethereum addresses and label known PDP, FWSS, FilecoinPay, USDFC, registry, implementation, and library actors. Keep unknown/system calls explicit.

Produce:

- a call tree with self and inclusive milligas;
- additive component totals obtained by grouping self milligas;
- top-level message overhead;
- any rounding difference between milligas and receipt gas.

Inclusive totals overlap and are diagnostic. Grouped self gas is the additive component comparison.

## Reporting across variants

`report.py` loads every available variant and joins cases by `comparisonKey`. It never assumes a before/after pair or a complete matrix.

Examples:

```console
benchmarks/fwss/report.py \
  --results benchmarks/fwss/results/mainnet-v1.3.0 \
  --baseline deployed

benchmarks/fwss/report.py \
  --results benchmarks/fwss/results/mainnet-v1.3.0 \
  --baseline deployed \
  --variant local-legacy \
  --variant local-new \
  --operation addPieces
```

Output:

- a human-readable comparison table;
- long-form JSON or CSV;
- total gas and percentage change when both baseline and variant succeed;
- component gas and component deltas;
- explicit success/failure status;
- maximum successful batches or lower bounds.

If the baseline fails and another variant succeeds, report the capacity change without inventing a percentage. Missing cases remain visible as `not-run` and do not block comparison of completed cases.

## Result validity requirements

A usable measurement depends on the imported archive, versions, and addresses;
verified owner/provider setup and funding; real signatures and lifecycle calls;
public upgrade paths with adopted bytecode validation; and fixtures whose
pre-/post-upgrade chronology matches the selected variant. Setup/configuration
failures must not be relabeled as measured contract outcomes. The presence of a
result file alone does not prove these requirements or authenticate its inputs.
