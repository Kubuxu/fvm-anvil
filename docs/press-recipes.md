# Press recipe authoring notes

Status: working runbook  
Last updated: 2026-11-09

This runbook covers source checks, dependency analysis, patch derivation, and runtime proof for
turning a contract deployment into a reproducible Press recipe.

These recipes reproduce historical on-chain deployments; their recorded versions and
addresses are intentional, not the latest local FWSS target. For a fresh FWSS 1.5.0 /
PDPVerifier 3.5.0 deployment and writable Curio PDP workflow, use the
[Curio/FWSS guide](../tests/curio/README.md). That workflow needs no Forest snapshot
or prepared archive.

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

Build progress is written to stderr; the final summary remains on stdout. Progress covers
construction, exported blocks and bytes, indexing, validation, and atomic publication. See the
[Press validation reference](press.md#validation-and-runtime-loading) for default validation and the
optional `--verify` full readback audit.

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

### Historical benchmark reports

The [PDPVerifier benchmark README](../benchmarks/pdp-verifier/README.md) preserves the historical
[add-pieces comparison](../benchmarks/pdp-verifier/README.md#pdpverifier-add-pieces-gas-benchmark)
and [lifecycle comparison](../benchmarks/pdp-verifier/README.md#pdpverifier-lifecycle-gas-benchmark),
including candidate provenance, fixture chronology, result tables, and terminal A/B commands.
Restart the node from this prepared archive between independent runner invocations; both runners
mutate state.
