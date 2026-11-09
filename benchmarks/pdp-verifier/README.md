# PDPVerifier historical gas benchmarks

These reports compare the imported mainnet v3.4.0 implementation with the storage-optimized PDP
revision [`391936ea0570b19eb7b92955f0ace317fdafa661`](https://github.com/FilOzone/pdp/tree/391936ea0570b19eb7b92955f0ace317fdafa661)
(`v3.4.0-23-g391936e`). They are historical results, not a comparison of current releases.

## Archive and execution prerequisites

Use `snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst`, prepared from the mainnet Forest snapshot
at epoch 6,251,280 with the patched default Anvil owner. The source and prepared archives are
ignored local artifacts, not bundled downloads. The [Press extraction runbook](../../docs/press-recipes.md#pdpverifier-mainnet-v340)
preserves the deployment manifest and dependency revisions, owner-slot derivation, selected proxy
and implementation, deliberate listener/FWSS omissions, archive roots, and runtime proof.

Commands below run from the repository root and require a release `fvm-anvil`, Python 3, `cast`,
`forge`, and a PDP source checkout at the candidate revision above, supplied as `/path/to/pdp`.
The runners build that checkout with `forge build`; its current branch is not a substitute for the
recorded candidate revision.

Each runner mutates node state and leaves the upgraded variant active. **Stop and restart the node
from the prepared archive before every independent `run.py` or `run_operations.py` invocation**, even
when switching runners. In-process snapshots restore equivalent starting state between variants
within an invocation; they do not reset the node for the next invocation.

### Fee mechanism

The shared `Benchmark` initializer in [`run.py`](run.py) reads `eth_gasPrice` once and uses that value
as `maxFeePerGas` throughout the invocation. It fixes `maxPriorityFeePerGas` at 1 attoFIL; the fee cap
is node-dependent, not a universal runner constant. Receipt `effectiveGasPrice` remains the bounded EIP-1559 price;
exact Filecoin burns, refunds, tips, and penalties are available from the Filecoin-specific endpoints.
The measured execution-gas tables below are preserved unchanged.
The named JSON results are generated, ignored local artifacts, not files shipped with this
repository. Historical tables do not establish that those original result files are present.


## PDPVerifier add-pieces gas benchmark

[`run.py`](run.py) benchmarks the imported v3.4.0 implementation against the candidate above.
Start the foreground node in **terminal A**:

```bash
target/release/fvm-anvil \
  --state snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst \
  --port 18546
```

Then run the benchmark in **terminal B**:

```bash
benchmarks/pdp-verifier/run.py --pdp-repo /path/to/pdp
```

Each variant starts from the same imported-state snapshot and creates data set 1,443 with no
listener. It sequentially adds batches of 1, 2, 4, 16, and 32 deterministic CommPv2 pieces, fills
the data set to exactly 5,000 pieces in batches of 16, then repeats the five measured batches. The
second variant deploys the local implementation with reinitializer version 4 and challenge finality
150, announces it, advances the epoch, and calls `upgradeToAndCall` with `migrate()`. Both data sets
finish with 5,055 pieces.

The benchmark records every measured transaction receipt, complete `filecoin_getGasTrace`, and
charge-name aggregate in
`benchmarks/pdp-verifier/results/pdp-add-pieces-gas-traces.json`. All 20 measured
transactions and 620 fill transactions succeeded.

| Starting pieces | Batch | Deployed gas | Optimized gas | Change |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 1 | 174,804,468 | 103,427,766 | -40.8% |
| 1 | 2 | 274,396,633 | 107,981,119 | -60.6% |
| 3 | 4 | 467,703,099 | 115,512,520 | -75.3% |
| 7 | 16 | 1,380,851,109 | 156,713,998 | -88.7% |
| 23 | 32 | 2,599,489,786 | 210,682,378 | -91.9% |
| 5,000 | 1 | 174,047,378 | 115,081,164 | -33.9% |
| 5,001 | 2 | 265,841,318 | 118,833,270 | -55.3% |
| 5,003 | 4 | 462,398,923 | 126,207,859 | -72.7% |
| 5,007 | 16 | 1,446,052,854 | 168,536,029 | -88.3% |
| 5,023 | 32 | 2,602,930,372 | 220,412,079 | -91.5% |

## PDPVerifier lifecycle gas benchmark

[`run_operations.py`](run_operations.py) uses the same imported snapshot and upgrade flow to
compare the rest of the data-set lifecycle. **Restart terminal A from the archive first**, including
if the add-pieces runner has just finished:

```bash
target/release/fvm-anvil \
  --state snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst \
  --port 18546
```

Then run in **terminal B**:

```bash
benchmarks/pdp-verifier/run_operations.py --pdp-repo /path/to/pdp
```

Each variant creates a data set containing 1,024 identical, valid height-5 CommPv2 zero trees. The
runner measures data-set creation; representative add, CID lookup, pagination, and CID search calls;
and data-set deletion and incremental cleanup. For removal batches of 1, 32, and 96 pieces, it
measures `schedulePieceDeletions`, the immediately following `nextProvingPeriod` that applies those
removals, and a possession proof containing the required five challenges. The zero-tree fixture
provides a valid proof for every possible piece and leaf offset, so both implementations execute the
same challenge-dependent path without mocking Filecoin randomness.

The fixtures are created separately under each variant: the baseline fixture precedes the snapshot
restore and upgrade, and the candidate fixture is created after the upgrade. This comparison is not
an exercise of a baseline-created fixture surviving migration.

The 96-piece case is the largest removal batch in this comparison because the deployed
implementation already consumes 7.14 billion gas while applying it, leaving bounded headroom below
the 10-billion block gas limit.

The runner checks piece counts, scheduled removal application, proof success, and final cleanup. It
records receipts, complete Filecoin gas traces, charge-name aggregates, and the baseline/optimized
comparison in `benchmarks/pdp-verifier/results/pdp-operation-gas-traces.json`.

Read-only calls are submitted as transactions so they produce receipts and Filecoin gas traces; they
do not mutate verifier state.

| Operation | Case | Deployed gas | Optimized gas | Change |
| --- | --- | ---: | ---: | ---: |
| `createDataSet` | empty | 123,076,468 | 123,280,376 | +0.2% |
| `addPieces` | 1 | 174,824,288 | 104,081,347 | -40.5% |
| `addPieces` | 8 | 799,346,484 | 131,556,419 | -83.5% |
| `addPieces` | 32 | 2,548,376,069 | 216,751,448 | -91.5% |
| `getPieceCid` | tail | 19,602,345 | 24,729,807 | +26.2% |
| `getActivePiecesByCursor` | 32 full | 226,066,144 | 132,714,635 | -41.3% |
| `findPieceIdsByCid` | limit 32 | 213,416,170 | 58,433,616 | -72.6% |
| `schedulePieceDeletions` | 1 | 77,567,624 | 81,705,016 | +5.3% |
| `nextProvingPeriod` | remove 1 | 463,004,407 | 240,960,202 | -48.0% |
| `provePossession` | 5 challenges after remove 1 | 185,953,204 | 101,444,392 | -45.4% |
| `schedulePieceDeletions` | 32 | 197,760,288 | 152,283,667 | -23.0% |
| `nextProvingPeriod` | remove 32 | 2,753,390,816 | 460,964,008 | -83.3% |
| `provePossession` | 5 challenges after remove 32 | 183,231,003 | 97,787,401 | -46.6% |
| `schedulePieceDeletions` | 96 | 382,838,618 | 252,093,235 | -34.2% |
| `nextProvingPeriod` | remove 96 | 7,142,448,164 | 811,948,603 | -88.6% |
| `provePossession` | 5 challenges after remove 96 | 184,633,835 | 100,597,572 | -45.5% |
| `getActivePiecesByCursor` | 32 after 129 holes | 436,279,799 | 162,286,483 | -62.8% |
| `deleteDataSet` | 1,024 slots | 94,626,110 | 77,991,600 | -17.6% |
| `cleanupPieces` | 1 | 149,154,504 | 89,602,117 | -39.9% |
| `cleanupPieces` | 16 | 1,341,620,438 | 98,475,885 | -92.7% |
| `cleanupPieces` | 64 | 4,553,835,628 | 136,541,618 | -97.0% |
| `cleanupPieces` | 96 | 6,598,846,685 | 161,517,495 | -97.6% |
| `cleanupPieces` | 96 and finalize | 658,885,130 | 194,883,685 | -70.4% |
