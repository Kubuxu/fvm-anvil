# Run Curio PDP with local FWSS

This recipe runs Curio's PDP-only **Skiff** binary against fvm-anvil, PostgreSQL, and
ScyllaDB. It deploys real contracts, registers/funds a provider, creates a dataset,
uploads/adds a 64 MiB piece, and waits for a successful real `provePossession`.
No miner, PoRep, PoSt, native wallet, or mainnet snapshot is needed.

Run commands from the repository root. Use a disposable node: the final Go client
checks mining and snapshot/revert behavior after stopping Skiff. Do not point this
recipe at a shared chain or a public network.

Choose a reader path:

- [First setup: steps 1–5](#1-install-prerequisites-and-select-sources).
- [Direct native RPC use](#use-the-native-rpc-capability-directly).
- [Resume the same live deployment](#continue-serving-after-acceptance).
- [Stop, reset, and rerun](#stop-inspect-and-run-again).


## 1. Install prerequisites and select sources

Required tools:

- Linux, Python 3, `jq`, Git, and Docker with Compose supporting `up --wait`.
- The repository's pinned Rust toolchain.
- Foundry (`forge` and `cast`; verified with 1.9.0).
- Go compatible with Curio's `go.mod` (verified with Go 1.27.3), make, and a C/C++
  toolchain; GCC works. The assembler below also needs autotools/libtool.
- Enough memory for Go compilation and the local ScyllaDB service.

For an existing checkout, skip cloning and preserve any local work before switching
revisions. To reproduce the verified versions in a fresh workspace:

```sh
git clone --recurse-submodules https://github.com/filecoin-project/curio.git extern/curio
git -C extern/curio checkout f5a416f633d82d293e278e7b1bb4ca135e0b6e16
git -C extern/curio submodule update --init --recursive

git clone --recurse-submodules https://github.com/FilOzone/filecoin-services.git extern/filecoin-services
git -C extern/filecoin-services checkout 305a44723c1f0e612e294f51f5fccfc5a4812a62
git -C extern/filecoin-services submodule update --init --recursive
```

The Curio build helper enforces that exact Curio revision. Skiff is FFI-free, but its
local `filecoin-ffi` module replacement must still resolve. The helper copies sources
into `.curio-build`; upstream sources are not edited.

The FWSS deployer uses the selected Filecoin Services checkout and its pinned submodules,
not a moving remote release. The verified checkout provides FWSS 1.5.0, PDPVerifier 3.5.0,
and registry 1.2.0. Later revisions are not automatically guaranteed compatible.

### Install the pinned ERC-8167 assembler

FWSS's dispatcher requires `wjmelements/evm` revision
`98409233b4ed9fb79efa013fe17ad42b155adf03`. If `evm` is already installed at that
revision, skip this step. The explicit fetch works with Git versions that lack
`git clone --revision`:

```sh
export PATH="$HOME/bin:$PATH"
assembler_dir=$(mktemp -d "${TMPDIR:-/tmp}/fwss-evm.XXXXXX")
git -C "$assembler_dir" init
git -C "$assembler_dir" remote add origin https://github.com/wjmelements/evm.git
git -C "$assembler_dir" fetch --depth 1 origin 98409233b4ed9fb79efa013fe17ad42b155adf03
git -C "$assembler_dir" checkout --detach FETCH_HEAD
git -C "$assembler_dir" submodule update --init --recursive
make -C "$assembler_dir" bin/evm
mkdir -p "$HOME/bin"
install "$assembler_dir/bin/evm" "$HOME/bin/evm"
```

Put Foundry on `PATH` too. No changes to an existing Foundry installation are required.

## 2. Build fvm-anvil and Skiff

```sh
cargo build --release -p fvm-anvil
tests/curio/build.sh
```

The latter builds `.curio-build/bin/skiff`, `pdptool`, and `client` against Curio's pinned
Go module. It refuses to overwrite an existing build directory. Reuse matching binaries
for later runs, or select a fresh `CURIO_BUILD_DIR`; `CURIO_SOURCE` selects the source
checkout. If changing the build directory, adjust the binary paths below.

## 3. Start a fresh node (terminal A)

```sh
mkdir -p .curio-acceptance
target/release/fvm-anvil --port 18545 --chain-id 31415926 \
  --timestamp "$(date +%s)" --block-time 1 \
  --config-out .curio-acceptance/node.json
```

Keep this process running throughout deployment and acceptance. The explicit timestamp
avoids the generic Anvil default of zero. Interval mining, including empty blocks,
advances receipt confidence and proof schedules; automine alone is insufficient.

The configuration file contains development private keys and is created with mode `0600`.
Do not commit or share it, and do not use these accounts on a public network. The default
funded account covers deployment, the registry's 5 FIL registration burn, payments, and gas.

## 4. Deploy the selected FWSS stack (terminal B)

```sh
export PATH="$HOME/bin:$PATH"
export KEY="$(jq -r '.private_keys[0]' .curio-acceptance/node.json)"
export DEPLOYMENT="$(mktemp -d "${TMPDIR:-/tmp}/fvm-anvil-fwss.XXXXXX")/deployment.json"

python3 tests/curio/deploy_fwss.py \
  --rpc-url http://127.0.0.1:18545/rpc/v1 \
  --private-key "$KEY" \
  --source extern/filecoin-services/service_contracts \
  --output "$DEPLOYMENT"

jq '{versions, contracts, curioEnv}' "$DEPLOYMENT"
```

The deployer copies/builds the source outside `extern/`, deploys the complete current
ERC-8167 dispatcher/facets/selector delegates/proxy graph plus PDP, registry, payment,
and session-key dependencies, then validates confirmed receipts and live getters.
The FWSS view address is the FWSS proxy itself, not a separate legacy view deployment.

The output manifest and build directory must not already exist. By default,
`deployment.json` has a sibling `deployment.build` directory containing compiler artifacts
and Foundry's confirmed broadcast journal. `--build-directory` can select another fresh
location outside the source checkout. The manifest records exact revisions, versions,
addresses, transactions, and `curioEnv` settings; never substitute mainnet addresses.

**Default payment asset:** official Solady WETH backed by a real 100-FIL deposit,
**not USDFC or a stablecoin**. This is a development ERC-20 payment path. Supply
`--token-address ADDRESS` for an existing real 18-decimal token and fund the payer with
that token; the acceptance helper deposits 10 tokens into FilecoinPay. `--wrap-amount`
changes the default wrapped funding amount in native attoFIL.

Default development proving parameters are challenge finality 5 epochs, proving period
120 epochs, and challenge window 10 epochs. See `deploy_fwss.py --help` for overrides.

## 5. Run writable PDP acceptance

Use a fresh dedicated database. On a first run, the launcher creates/starts the Docker
services automatically. For a repeat run, follow the reset instructions below first.

```sh
python3 tests/curio/run.py --manifest "$DEPLOYMENT" \
  --private-key "$KEY" \
  --skiff .curio-build/bin/skiff \
  --pdptool .curio-build/bin/pdptool \
  --client .curio-build/bin/client
```

The launcher:

1. Starts PostgreSQL/ScyllaDB, creates a single-node CQL keyspace with replication factor 1,
   and lets Skiff apply real migrations in Curio's database schema.
2. Installs isolated configuration, local piece storage, and the supplied raw PDP signer
   key; then restarts writable Skiff. The same funded key is provider, payer, and signer.
3. Runs `authorize.py` to register the provider, approve/deposit payment tokens, approve
   FWSS as payment operator, and sign EIP-712 requests with unused payer nonces.
4. Creates a dataset, uploads a random 64 MiB piece, and adds it. Authorization signs the
   exact **PieceCIDv2** used by the HTTP request, not the database's PieceCIDv1.
5. Checks event-derived dataset/piece IDs, a successful on-chain proof receipt, populated
   ScyllaDB proof-cache layers, and absent advertisement work/records.
6. Stops Skiff and exercises the real pinned Go clients: native heads, full Ethereum
   block bodies/receipts/logs, and `current/apply/revert/cancel/reconnect` head streaming.

On success, stdout contains `signer`, `dataSetId`, `pieceId`, `pieceCid`, `createTx`,
`addTx`, and `proofTx`. Evidence is retained in `.curio-acceptance/`:

| File | Contents |
| --- | --- |
| `result.json` | Dataset, piece, and transaction identities. |
| `proof-receipt.json` | Successful real proof receipt. |
| `proof-cache.txt` | CQL proof-cache layer count. |
| `skiff.log` | Startup/task diagnostics, including failed stages. |
| `auth.json` | Signed create/add payloads and client nonce bookkeeping. |

`--work-dir` selects another results/storage directory. `--timeout` changes the default
1800-second dataset/piece/proof stage timeout; startup and database readiness have their
own bounded waits. A timeout is an error, never a successful or skipped proof.

### Database, networking, and indexing behavior

| Service | Local address |
| --- | --- |
| fvm-anvil HTTP / WebSocket | `http://127.0.0.1:18545/rpc/v1` / `ws://127.0.0.1:18545/rpc/v1` |
| Public PDP HTTP | `http://127.0.0.1:18080` |
| Skiff admin | `127.0.0.1:14701` |
| PostgreSQL 16.6 | `127.0.0.1:15432` |
| ScyllaDB 6.2.3 CQL | `127.0.0.1:19042` |

Skiff runs on the host; only databases run in Docker. This avoids exposing unauthenticated
RPC outside loopback. `--rpc-url` selects the node endpoint; `--service-url` selects a local
plain-HTTP origin/listener. Database ports are fixed by the recipe. ScyllaDB runs with one
CPU and 1 GiB RAM. `CURIO_DB_CASSANDRA_PORT` is the upstream CQL-port variable spelling;
it does not mean a Cassandra service is needed.

The profile disables deal-market/IPNI announcements, empties `DirectAnnounceURLs`, and
omits the `withIPFSIndexing` metadata key entirely. A present key with value `"false"`
still opts in. The CQL store and proof-cache generation remain enabled: a 64 MiB piece
exceeds Curio's 32 MiB padded cache threshold. Read-only Skiff mode is not a proving test.

Private signing keys remain in this dedicated local database. The profile is local-only.

## Prepare an imported mainnet-state run

Use this path instead of fresh deployment when testing an upgrade against existing FWSS
storage. Do not run `deploy_fwss.py` against the imported node: that creates a different
stack and does not exercise the mainnet proxies or migration.

### Runtime prerequisite

Select the archive's recorded network version when starting the node. Startup rejects a
network-version or bundle-manifest mismatch; do not relabel the state or replace its actor codes.

Inspect the final `.press.car.zst` file's source epoch, network version, actor inventory,
patches, and roots. Temporary Press build files are not archives:

```sh
export ARCHIVE=snapshots/fwss-mainnet-v1.5.0-height-6530880.press.car.zst
target/release/press inspect "$ARCHIVE"
```

The expected graph and omissions are recorded in
[`recipes/fwss-mainnet-v1.5.0.evidence.json`](../../recipes/fwss-mainnet-v1.5.0.evidence.json).
The live imported graph, not deployment labels, is authoritative. It must include FWSS's
dispatcher/delegates, PDP, registry, FilecoinPay, session keys, and USDFC proxy/implementation.
The omitted Safe owner does not need importing: local preparation explicitly patches only
FWSS's ownership word to our development signer and records that mutation.

### Upgrade the existing modular proxy

Start a **dedicated loopback node**, with chain ID 314 and one-second interval mining.
An imported archive supplies its own epoch/timestamp/base fee; do not add `--timestamp`.
Use new output directories and a separate Docker Compose project to preserve earlier runs.

```sh
export WORK="$(mktemp -d "${TMPDIR:-/tmp}/curio-mainnet.XXXXXX")"
target/release/fvm-anvil --state "$ARCHIVE" --network-version 29 --chain-id 314 \
  --port 18548 --block-time 1 --config-out "$WORK/node.json"
```

In another terminal, set `WORK` to that directory and select the reviewed candidate source.
The StorageTerms rehearsal target is PR #653, commit
`371e8b0a48ccf622288e0af0f02a4468d5c6a7be`, not a moving `main`. Keep the source checkout
read-only; the helper exports committed files and recursively pinned submodules into its build.

```sh
export PATH="$HOME/bin:$PATH"
export RPC_URL=http://127.0.0.1:18548/rpc/v1
export PRIVATE_KEY="$(jq -r '.private_keys[0]' "$WORK/node.json")"
export FWSS_CHECKOUT="$WORK/filecoin-services"
git clone --no-checkout https://github.com/FilOzone/filecoin-services.git "$FWSS_CHECKOUT"
git -C "$FWSS_CHECKOUT" checkout --detach 371e8b0a48ccf622288e0af0f02a4468d5c6a7be
git -C "$FWSS_CHECKOUT" submodule update --init --recursive
export FWSS_SOURCE="$FWSS_CHECKOUT/service_contracts"
export DEPLOYMENT="$WORK/upgraded.json"

python3 tests/curio/upgrade_fwss.py --rpc-url "$RPC_URL" \
  --source "$FWSS_SOURCE" --output "$DEPLOYMENT"
```

The separate checkout preserves the source selected for the fresh-deployment recipe.
Untracked and ignored files are excluded from the build; every recursive submodule checkout
must match its pinned revision. No source checkout is modified by the helper.
The helper preserves dependency addresses, proxy, dispatcher, and imported proving parameters;
it deploys candidate facets and a route migration, wraps that migration in
`FWSSStorageTermsMigration`, then executes the real announce/wait/migrate path.
Do not execute the raw route migration: it does not initialize the legacy terms record.
The manifest records the local ownership patch, roots, upgrade transactions, and post-upgrade
checks. A fresh-state rehearsal is identified separately from an imported-state run.

### Fund local keys with the imported payment token

Keep the imported USDFC implementation and storage layout. Do not deploy a substitute token
or patch FilecoinPay account balances. The acceptance helper can assign a synthetic ERC-20
wallet balance with `anvil_setERC20Balance`, then use real approval and deposit transactions.
This is local test funding, not collateral-backed minting; it does not establish token supply
or collateral invariants. Funding must fail if the token's balance layout is unsupported.

First run the legacy creation format against the upgraded proxy:

```sh
export COMPOSE_PROJECT_NAME="fvm-anvil-curio-legacy-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
python3 tests/curio/run.py --manifest "$DEPLOYMENT" --rpc-url "$RPC_URL" \
  --private-key "$PRIVATE_KEY" --fund-token-balance 100000000000000000000 \
  --skiff .curio-build/bin/skiff --pdptool .curio-build/bin/pdptool \
  --client .curio-build/bin/client --work-dir "$WORK/legacy" --timeout 7200
```

`--fund-token-balance` is an absolute wallet balance in smallest token units, not an
increment. The example assigns 100 USDFC; the default real deposit is 10 tokens.
Funding mutations are restricted to a loopback fvm-anvil endpoint.
All native FIL fees and provider registration burns still need a funded development account.

For explicit StorageTerms, register a definition through the locally controlled FWSS proxy,
then pass its ID. For example, use the same imported USDFC at 5 tokens/TiB/month:

```sh
export FWSS="$(jq -r '.contracts.fwss' "$DEPLOYMENT")"
export TOKEN="$(jq -r '.contracts.token' "$DEPLOYMENT")"
export SALT=0x0000000000000000000000000000000000000000000000000000000000000001
export TERMS="($TOKEN,18,$SALT,5000000000000000000)"
cast send "$FWSS" 'registerStorageTerms((address,uint8,bytes32,uint96))' "$TERMS" \
  --rpc-url "$RPC_URL" --private-key "$PRIVATE_KEY" --gas-limit 10000000000
export TERMS_ID="$(cast call "$FWSS" \
  'getStorageTermsId((address,uint8,bytes32,uint96))(bytes32)' "$TERMS" \
  --rpc-url "$RPC_URL")"

docker compose -p "$COMPOSE_PROJECT_NAME" -f tests/curio/compose.yaml stop
export COMPOSE_PROJECT_NAME="fvm-anvil-curio-terms-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
python3 tests/curio/run.py --manifest "$DEPLOYMENT" --rpc-url "$RPC_URL" \
  --private-key "$PRIVATE_KEY" --storage-terms-id "$TERMS_ID" \
  --fund-token-balance 100000000000000000000 \
  --skiff .curio-build/bin/skiff --pdptool .curio-build/bin/pdptool \
  --client .curio-build/bin/client --work-dir "$WORK/terms" --timeout 7200
```

Keep the same node alive between these runs. Each invocation above selects a unique Compose
project and fresh database volumes, even when reusing `$WORK`. Stop the previous project before
starting another attempt or switching projects: the database host ports are shared. Preserve its
`COMPOSE_PROJECT_NAME` long enough to run the `docker compose -p ... stop` command above.
Imported finality/proving periods are not shortened; allow the larger stage timeout.
The acceptance gate still requires real ingestion, a successful proof receipt, CQL proof
cache, indexing isolation, and Go-client checks. It additionally verifies the selected
terms and actual payment-rail token/rate. Curio's acceptance of a signed terms payload is
not evidence that it enforces a provider token/price policy.

This path does not upgrade PDP, registry, payments, session keys, or token contracts. It does
not exercise omitted token minting/collateral contracts, Safe signing, or other PDP listeners.
The 2026-11-09 preparation run loaded this archive with FVM 4.9.0 and actors v19.0.1,
executed the wrapped migration with 19 successful receipts, preserved sampled views of 13
existing datasets and the proving configuration, assigned synthetic USDFC, and deposited
10 USDFC through the real token and FilecoinPay contracts. Full Curio ingestion/proving on
the imported state has not been run. The separate fresh-state preparation rehearsal used the
previous NV28/v18 runtime and passed migration, explicit terms, synthetic funding, real deposit,
ingestion, and proving; it does not establish coverage of the imported-state runtime.

## Use the native RPC capability directly

The same `/rpc/v1` endpoint accepts Ethereum and supported `Filecoin.*` methods:

```sh
export RPC_URL=http://127.0.0.1:18545/rpc/v1
cast chain-id --rpc-url "$RPC_URL"
cast rpc Filecoin.StateNetworkName --rpc-url "$RPC_URL"
cast rpc Filecoin.StateNetworkVersion '[]' --rpc-url "$RPC_URL"
cast rpc Filecoin.ChainHead --rpc-url "$RPC_URL"
cast rpc Filecoin.ChainGetTipSetByHeight 0 '[]' --rpc-url "$RPC_URL"
cast rpc Filecoin.StateGetRandomnessDigestFromBeacon 0 '[]' --rpc-url "$RPC_URL"
```

Here `[]` selects the current head. Native epochs are decimal JSON integers, not hex
quantities. The beacon result is a base64-encoded raw 32-byte digest shared with the FVM
syscall. The epoch-zero examples assume the fresh genesis used above; imported state
does not provide history before its import epoch.

For an independently configured Skiff, use a WebSocket **origin**, without `/rpc/v1`:

```sh
export FULLNODE_API_INFO=ws://127.0.0.1:18545
export CURIO_DEVNET_PDP_VERIFIER_ADDRESS="$(jq -r '.contracts.pdp' "$DEPLOYMENT")"
export CURIO_DEVNET_FWSS_ADDRESS="$(jq -r '.contracts.fwss' "$DEPLOYMENT")"
export CURIO_DEVNET_SERVICE_REGISTRY_ADDRESS="$(jq -r '.contracts.registry' "$DEPLOYMENT")"
```

Curio appends `/rpc/v1` itself. Including it in `FULLNODE_API_INFO` duplicates the path;
using an HTTP-only client cannot establish `ChainNotify`. The launcher sets these values
and the PostgreSQL/ScyllaDB connection settings automatically. These exports alone do
not replace writable DB, storage, signer, and provider/payment setup.

`Filecoin.ChainNotify` uses Go `go-jsonrpc` WebSocket channels, not `eth_subscribe`.
See the [implemented API contracts](../../docs/curio-pdp-compatibility.md#implemented-api-boundary).
Native miners, PoRep/PoSt, general Lotus wallet/object methods, and Ethereum subscriptions
remain unsupported. The [gas benchmark](../../benchmarks/fwss/README.md) is a separate
imported-state experiment, not this fresh deployment workflow.

## Continue serving after acceptance

If the same fvm-anvil process and deployment are still alive, restart Skiff with the
database, signer, configuration, and storage initialized by the successful run. Keep the
`FULLNODE_API_INFO` and three `CURIO_DEVNET_*` exports from the preceding section:

```sh
docker compose -f tests/curio/compose.yaml up -d --wait --wait-timeout 900
CURIO_DB_HOST=127.0.0.1 CURIO_DB_PORT=15432 \
CURIO_DB_HOST_CQL=127.0.0.1 CURIO_DB_CASSANDRA_PORT=19042 \
CURIO_DB_NAME=curio CURIO_DB_USER=curio CURIO_DB_PASSWORD=curio-local-only \
CURIO_DB_LOAD_BALANCE=false \
  .curio-build/bin/skiff \
    --repo-path "$PWD/.curio-acceptance/repo" \
    --data "$PWD/.curio-acceptance/pieces"
```

Adjust the repo/data paths if acceptance used another `--work-dir`. Public PDP remains
at `http://127.0.0.1:18080` with the default profile; stop Skiff with Ctrl-C before the
cleanup below. This resumes the existing dataset, not a new acceptance run. It is not
valid after restarting the in-memory node; reset/redeploy instead.

## Stop, inspect, and run again

The launcher stops Skiff on success or failure. It leaves fvm-anvil, Docker services,
volumes, and evidence available for inspection. Stop fvm-anvil with Ctrl-C in terminal A.
To stop databases without deleting evidence:

```sh
docker compose -f tests/curio/compose.yaml stop
```

**For a fresh acceptance run only:** the next command deletes this recipe's dedicated
PostgreSQL and ScyllaDB volumes, including stored keys and dataset records. Save any
wanted evidence first; do not use it against a reused/shared Compose project.

```sh
docker compose -f tests/curio/compose.yaml down -v
```

A node restart loses its in-memory chain: start a fresh node, deploy again into a new
manifest/build directory, reset the dedicated databases, and rerun acceptance. An old
manifest is not a state-restoration mechanism. The launcher rejects populated signer or
dataset tables rather than treating old records as a new successful run.

Do not rerun `build.sh` just to repeat acceptance; reuse the matching binaries. Logs/results
in the default work directory can be overwritten/appended by another run; select a new
`--work-dir` to preserve a previous run. Earlier Cassandra containers must be stopped
before using the same CQL host port; their volumes are not reused by ScyllaDB.

## Verified result

The pinned stack completed creation/add/proving on 2026-11-09. Tested revisions and
dated verification evidence are in the
[compatibility record](../../docs/curio-pdp-compatibility.md#exercised-verification);
they are not a guarantee for arbitrary future checkouts.
