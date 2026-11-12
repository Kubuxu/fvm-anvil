# Press recipe authoring notes

Status: working runbook  
Last updated: 2026-08-05

This runbook covers source checks, dependency analysis, patch derivation, and runtime proof for
turning a contract deployment into a reproducible Press recipe.

## Inputs to pin

A recipe is reproducible only when all of these inputs are explicit:

1. The source ForestCAR snapshot path and head epoch.
2. The chain ID or network whose deployment record is being used.
3. The deployment manifest revision, not merely its current default branch.
4. Every selected actor address and why it is needed.
5. The exact source revision used to derive each storage slot or code patch.
6. The intended local controller address.
7. The output path and Press recipe version.

Do not infer live dependencies from contract names. Read the deployment manifest, proxy type,
constructor arguments, linked libraries, and contract source. Dynamic EVM destinations cannot be
discovered reliably from bytecode and must be selected deliberately when the experiment exercises
them.

## Generic workflow

### 1. Establish the deployment

Use the canonical deployment manifest for the target chain. Record its repository revision and any
version metadata next to the recipe. Ensure the source snapshot epoch is later than the deployment.

For a proxy, identify separately:

- the stable proxy address;
- the implementation address recorded by the deployment;
- the proxy standard;
- the live implementation slot;
- the authorization mechanism used by the implementation;
- linked libraries and fixed external contract addresses.

The deployment record is an input, not proof of current state. After building the archive, read the
proxy's implementation slot through RPC and compare it with the selected implementation actor.

### 2. Select the complete call graph needed by the experiment

Include the proxy and implementation as separate recipe actors. Add deployed libraries and fixed or
dynamic callees only when the exercised path can call them. Press automatically includes the System,
Init, Reward, EAM, and burnt-funds actors required by the runtime; recipe authors do not list them.

Omitting a dynamic callee is valid for view or control paths that never invoke it. A later call to an
omitted contract has normal absent-actor behavior, so the recipe notes must state which paths remain
out of scope.

### 3. Derive patches from pinned source

For namespaced OpenZeppelin storage, copy or recompute the slot from the exact dependency revision.
Do not use a guessed sequential Solidity slot. Encode storage keys and values as full 32-byte words.
Addresses in values are right-aligned and zero-padded.

Prefer the narrowest patch that grants the intended control. For a UUPS proxy this normally means
patching the implementation's authorization state stored in the proxy, not changing the ERC-1967
implementation slot. Keep the original implementation selected so the imported proxy remains
callable before a replacement implementation is deployed.

### 4. Build and inspect

```bash
target/release/press build \
  --snapshot SOURCE.forest.car.zst \
  --recipe recipes/RECIPE.json \
  --output snapshots/OUTPUT.press.car.zst

target/release/press inspect snapshots/OUTPUT.press.car.zst
```

Build progress is written to stderr. The final build summary remains on stdout.
Progress reports construction, blocks and uncompressed bytes written, index construction, compact
validation, and publication. The completed archive is published atomically only after validation.
Pass `--verify` to additionally read every reachable block back through the completed archive's
index before publication; this full audit is disabled by default for large actor DAGs.

Inspection must show:

- both requested addresses under the expected actor IDs;
- the expected prepared state root and source execution context;
- the patched slot's source and replacement words;
- the implementation actor with no accidental patch;
- all required actor-code dependencies.

### 5. Prove behavior through the node

Start `fvm-anvil --state OUTPUT` and check through Ethereum RPC:

1. `eth_getCode` is non-empty at both proxy and implementation addresses.
2. `eth_getStorageAt` for the ERC-1967 implementation slot names the selected implementation.
3. The proxy's public version getter identifies the expected implementation release.
4. The public authorization getter returns the patched local controller.
5. An authorization-gated transaction from that controller succeeds.
6. The same transaction from an unrelated caller fails when that distinction matters to the recipe.
7. `filecoin_getGasBreakdown` returns the FVM gas result for an adopted transaction.

A storage read alone is not sufficient proof. The delegated call through the proxy must observe the
patched authority.

## PDPVerifier mainnet v3.4.0

The reproducible recipe is
[`recipes/pdp-verifier-mainnet-v3.4.0.json`](../recipes/pdp-verifier-mainnet-v3.4.0.json).

### Deployment provenance

The Filecoin Services mainnet deployment record at commit
[`c2ff7f8ac574f62e5d9f9ace2cf2efbb64b0f3a4`](https://github.com/FilOzone/filecoin-services/tree/c2ff7f8ac574f62e5d9f9ace2cf2efbb64b0f3a4)
records PDP version 3.4.0 and these chain-314 addresses:

- UUPS/ERC-1967 proxy: `0xBADd0B92C1c71d02E7d520f64c0876538fa2557F`;
- implementation: `0xb41A97FEDD2D9497C639A643ec75E56CbCeDe8BA`.

The manifest records no linked libraries for `PDPVerifier`. Its Filecoin Services PDP submodule is
pinned to
[`b8ae60d12490d9821c86781f37470037fcc4dc10`](https://github.com/FilOzone/pdp/tree/b8ae60d12490d9821c86781f37470037fcc4dc10).
The implementation uses OpenZeppelin `OwnableUpgradeable` and `UUPSUpgradeable`. Upgrade ownership
therefore lives in the proxy's delegated storage rather than in a transparent-proxy admin actor.

The selected source snapshot is
`snapshots/forest_snapshot_mainnet_2026-08-04_height_6251280.forest.car.zst`, at epoch 6,251,280.
It postdates the deployment record's 2026-06-11 publication time.

The `snapshots/` directory is ignored by version control. Obtain the source snapshot separately and
build the prepared archive locally with the recipe above.

### Authority patch

The pinned OpenZeppelin upgradeable dependency is revision
[`2f0d9bd3da66a4072081d396760fecb517f50b56`](https://github.com/OpenZeppelin/openzeppelin-contracts-upgradeable/tree/2f0d9bd3da66a4072081d396760fecb517f50b56).
Its ERC-7201 `openzeppelin.storage.Ownable` location is:

```text
0x9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300
```

The recipe replaces that proxy storage word with the first default Anvil address:

```text
0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266
```

The recipe does not alter the standard ERC-1967 implementation slot:

```text
0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc
```

`PDPVerifier._authorizeUpgrade` additionally requires the owner to announce the exact next
implementation and a future epoch. A useful authority proof is therefore:

1. call `owner()` through the proxy and observe the default Anvil address;
2. call `announcePlannedUpgrade((address,uint96))` from that account, naming deployed code and a
   future epoch;
3. read `nextUpgrade()` back;
4. advance beyond the announced epoch;
5. call `upgradeToAndCall(address,bytes)` from the same account.

A real replacement must preserve the PDPVerifier storage layout. It must also account for immutable
implementation values such as challenge finality and the legacy-activity epoch; importing proxy
storage cannot change immutables embedded in runtime bytecode.

### Deliberate omissions

This recipe selects only the PDPVerifier proxy and its current implementation. It does not include
FWSS or dataset listener actors. Proxy views and upgrade-control paths are expected to work. Dataset
operations that invoke an omitted listener are outside this recipe's contract and require adding that
listener actor explicitly.

### Prepared archive

The local `snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst` artifact is 2,062,000,595 bytes. Its
inspected roots are:

```text
metadata: bafy2bzaceaikjfyizozynzde3sh5f36cet5omc42kvbqtohxh2u7dpfxtx26u
prepared: bafy2bzacedxp7q7gkbh7jt62b3rn7tmfbc5pvkc334ymvsu3rkqdka3adkriw
```

Inspection records the source owner
`0x3569b2600877a9f42d9ebdd205386f3f3788f3e5` and the replacement Anvil owner. The public behavior
checks for this archive are the proxy and implementation code, ERC-1967 implementation address,
`VERSION()` 3.4.0, patched `owner()`, and owner/non-owner upgrade authorization paths.

### PDPVerifier add-pieces gas benchmark

`benchmarks/pdp-verifier/run.py` benchmarks the imported v3.4.0 implementation against the
storage-optimized implementation at PDP revision
[`391936ea0570b19eb7b92955f0ace317fdafa661`](https://github.com/FilOzone/pdp/tree/391936ea0570b19eb7b92955f0ace317fdafa661)
(`v3.4.0-23-g391936e`). Check out that revision at `/path/to/pdp`, start the prepared node, then run
the benchmark:

```console
$ target/release/fvm-anvil \
    --state snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst \
    --port 18546
$ benchmarks/pdp-verifier/run.py --pdp-repo /path/to/pdp
```

Each variant starts from the same imported-state snapshot and creates data set 1,443 with no
listener. It sequentially adds batches of 1, 2, 4, 16, and 32 deterministic CommPv2 pieces, fills
the data set to exactly 5,000 pieces in batches of 16, then repeats the five measured batches. The
second variant deploys the local implementation with reinitializer version 4 and challenge finality
150, announces it, advances the epoch, and calls `upgradeToAndCall` with `migrate()`. Both data sets
finish with 5,055 pieces.

The benchmark records every measured transaction receipt, complete `filecoin_getGasTrace`, and
charge-name aggregate in
`benchmarks/pdp-verifier/results/pdp-add-pieces-gas-traces.json`. All 20 measured transactions and
620 fill transactions succeeded.

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

The runner fixes `maxFeePerGas` at 1,519,163 attoFIL and `maxPriorityFeePerGas` at 1 attoFIL so
client fee heuristics do not affect the execution-gas comparison. Receipt `effectiveGasPrice`
remains the bounded EIP-1559 price; exact Filecoin burns, refunds, tips, and penalties are available
from the Filecoin-specific endpoints.

### PDPVerifier lifecycle gas benchmark

`benchmarks/pdp-verifier/run_operations.py` uses the same imported snapshot and upgrade flow to
compare the rest of the data-set lifecycle:

```console
$ target/release/fvm-anvil \
    --state snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst \
    --port 18546
$ benchmarks/pdp-verifier/run_operations.py --pdp-repo /path/to/pdp
```

Each variant creates a data set containing 1,024 identical, valid height-5 CommPv2 zero trees. The
runner measures data-set creation; representative add, CID lookup, pagination, and CID search calls;
and data-set deletion and incremental cleanup. For removal batches of 1, 32, and 96 pieces, it
measures `schedulePieceDeletions`, the immediately following `nextProvingPeriod` that applies those
removals, and a possession proof containing the required five challenges. The zero-tree fixture
provides a valid proof for every possible piece and leaf offset, so both implementations execute the
same challenge-dependent path without mocking Filecoin randomness.

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

## FWSS metadata cleanup snapshot at epoch 6,457,920

Recipe [`recipes/fwss-mainnet-6457920.json`](../recipes/fwss-mainnet-6457920.json) selects the
FWSS 1.4.0 dependency graph without state patches. Its deployment evidence is
`service_contracts/deployments.json` at Filecoin Services checkout
`7e6bd5013f4ec9ba2b3db4e8bda7353f8cdd4e19`; that manifest records deployment revision
`4cbbea4a4f7da51ad674a1d9d122c6e677acfbec`. Snapshot proxy storage, not the original proxy constructor
arguments, determines the implementation selections.

| Role | Selected address |
| --- | --- |
| FWSS proxy | `0x8408502033C418E1bbC97cE9ac48E5528F371A9f` |
| FWSS implementation | `0xa874C7333c8e1689C33155AD7bEE980926086e21` |
| FWSS state view | `0xcf184Ab1FD8D1a563054d30Aa1fFb08136998172` |
| SignatureVerificationLib | `0x63904443e46cc9F3db35CB4Fe16c76E7ffCA453e` |
| Rails | `0x54aC29E68B3C7C5F7F315CeF1a85ECA848B88a9b` |
| PDPVerifier proxy | `0xBADd0B92C1c71d02E7d520f64c0876538fa2557F` |
| PDPVerifier implementation | `0x04D4CdB99E34D7e9a366E7eba7A835e034bc41b1` |
| ServiceProviderRegistry proxy | `0xf55dDbf63F1b55c3F1D4FA7e339a68AB7b64A5eB` |
| ServiceProviderRegistry implementation | `0x1Bb676392272313598930FEf8D5B66FFECcE02F0` |
| FilecoinPay | `0x23b1e018F08BB982348b15a86ee926eEBf7F4DAa` |
| SessionKeyRegistry | `0x74FD50525A958aF5d484601E252271f9625231aB` |
| USDFC proxy | `0x80B98d3aa09ffff255c3ba4A241111Ff1262F045` |
| USDFC implementation | `0x584c1fa4ab299ae8f7154fce5b1bea7bddacfd0b` |

The endorsement set `0x59eFa2e8324E1551d46010d7B0B140eE2F5c726b` is a sidecar, not a metadata
cleanup dependency. The FilBeam beneficiary `0x1D60d2F5960Af6341e842C539985FA297E10d6eA` is a recipient
identity, not an exercised contract. Endorsement operations and beneficiary execution are outside this
archive's validation scope.

```console
target/release/press inspect snapshots/forest_snapshot_mainnet_2026-10-15_height_6457920.forest.car.zst
target/release/press build \
  --snapshot snapshots/forest_snapshot_mainnet_2026-10-15_height_6457920.forest.car.zst \
  --recipe recipes/fwss-mainnet-6457920.json \
  --output snapshots/fwss-mainnet-6457920.press.car.zst
target/release/press inspect snapshots/fwss-mainnet-6457920.press.car.zst
target/release/fvm-anvil --state snapshots/fwss-mainnet-6457920.press.car.zst --port 18548
```

Observed archive: 3,755,008,567 bytes, 18 actors including the five automatic runtime actors, and
19,627,839 blocks. Press completed in 560.10 seconds, including indexing and structural validation.
Full reachable-DAG readback (`--verify`) was not requested.

```text
metadata: bafy2bzacedpacda35j44xc46ph5rk5uwj3qism2gjfdq6wbdn5ajsjkjo5wyc
prepared: bafy2bzaceaaehbn76hgusbcheqfufbgjsuur7xaais3nriigc5stclrzh3t5m
source:   bafy2bzaceaobifrjxaztyzwsaayt5kvbay6sbsqcmdnks6kkbcln7b4y2h5sa
```

The imported context is NV28/v18, timestamp 1,792,044,000, base fee 8,267 attoFIL/gas. Runtime checks
confirmed nonempty code at every selected address, all four ERC-1967 implementation slots, FWSS
`VERSION()` 1.4.0, PDP `VERSION()` 3.5.0, registry `VERSION()` 1.2.0, all FWSS dependency getters,
state-view `service()` and delegated provider-count access, session authorization lookup, payment
constants, and USDFC `decimals()` 18. Both linked-library addresses occur in implementation bytecode.
A reverted-after-validation zero-amount `FilecoinPay.deposit(USDFC, caller, 0)` transaction exercised
the payment/token call path successfully using 76,512,565 FVM gas; this does not prove positive-value
funding or a complete payment lifecycle.

Detailed local evidence is saved in
`benchmarks/fwss/results/metadata-cleanup-6457920/extraction-validation.json`.

