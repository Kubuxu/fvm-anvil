# Press recipe authoring notes

Status: working runbook
Last updated: 2026-08-04

These notes capture the source checks, dependency analysis, patch derivation, and runtime proof needed
to turn a contract deployment into a reproducible Press recipe. They are intentionally structured so
the workflow can later become an agent skill.

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
Progress reports construction, blocks and uncompressed bytes written, index construction, reachable
blocks verified, and publication. The completed archive is published atomically only after structural
verification.

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

### Observed archive and runtime proof

The generated `snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst` is 2,062,000,595 bytes and
contains 11,134,707 blocks. Full construction and reachable-DAG verification took 2,355.0 seconds.
Its roots are:

```text
metadata: bafy2bzaceaikjfyizozynzde3sh5f36cet5omc42kvbqtohxh2u7dpfxtx26u
prepared: bafy2bzacedxp7q7gkbh7jt62b3rn7tmfbc5pvkc334ymvsu3rkqdka3adkriw
```

Inspection recorded the source owner
`0x3569b2600877a9f42d9ebdd205386f3f3788f3e5` and the replacement Anvil owner. Starting
`fvm-anvil` from the archive reached RPC readiness in under one second because node startup validates
the compact prepared boundary without repeating Press's full 11-million-block traversal.

RPC proof observed:

- proxy code size: 130 bytes;
- implementation code size: 19,726 bytes;
- ERC-1967 implementation:
  `0xb41A97FEDD2D9497C639A643ec75E56CbCeDe8BA`;
- `VERSION()`: `3.4.0`;
- `owner()`: `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266`;
- imported `getNextDataSetId()`: 1,443;
- a non-owner upgrade announcement reverted with `OwnableUnauthorizedAccount`;
- owner announcement transaction:
  `0x7d3f9d733c26fbade64103676b9f27ae32ae525fd1f4f61741a86ea640f38ec8`;
- owner `upgradeToAndCall` transaction:
  `0x18de2d4e9c2a3794eba797496889dff1422b0940ad61bf02dff392a1e37911c8`.

Both owner transactions succeeded. The second transaction emitted `Upgraded`, cleared
`nextUpgrade`, and produced a `filecoin_getGasBreakdown` result.

### PDPVerifier add-pieces gas benchmark

`benchmarks/pdp-verifier/run.py` benchmarks the imported v3.4.0 implementation against the
storage-optimized implementation at PDP revision
`391936ea0570b19eb7b92955f0ace317fdafa661` (`v3.4.0-23-g391936e`). Start the prepared node, then
run the benchmark:

```console
$ target/release/fvm-anvil \
    --state snapshots/pdp-verifier-mainnet-v3.4.0.press.car.zst \
    --port 18546
$ benchmarks/pdp-verifier/run.py
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

The runner fixes `maxFeePerGas` at 1,519,163 attoFIL and `maxPriorityFeePerGas` at 1 attoFIL.
This prevents client fee heuristics from affecting an execution-gas benchmark. During development,
receipt `effectiveGasPrice` incorrectly projected total Filecoin charges per used gas and exceeded
the submitted EIP-1559 fee cap. Foundry fed that value back into later priority fees, producing
spurious enormous balance requirements. Receipts now use the bounded EIP-1559 effective price;
exact Filecoin burns, refunds, tips, and penalties remain available from the Filecoin-specific
endpoints.

## Performance note

A profile of the 454-block WFIL extraction showed repeated decompression of the same ForestCAR frame
as the dominant cost. The bounded 512 MiB decompressed-frame cache reduced the observed profiled
build from 42.1 seconds to 2.7 seconds on the same snapshot and recipe. Buffered frame/index writes
preserve byte-for-byte output while avoiding per-block metadata and small-write system calls. The
cache is bounded and does not load the source snapshot wholesale.
