#!/usr/bin/env bash
set -euo pipefail

TEST_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
PRIVATE_KEY="${PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
EXPECTED_CHAIN_ID="${EXPECTED_CHAIN_ID:-}"
FILECOIN_ASSERTIONS="${FILECOIN_ASSERTIONS:-0}"
INITIAL_VALUE=41
UPDATED_VALUE=99
CONTRACT="src/NarrowSlice.sol:NarrowSlice"

export NO_COLOR=1
export TERM=dumb

fail() {
    printf 'acceptance failure: %s\n' "$*" >&2
    exit 1
}

assert_eq() {
    local actual="$1"
    local expected="$2"
    local description="$3"

    [[ "$actual" == "$expected" ]] ||
        fail "$description: expected '$expected', got '$actual'"
}

json_field() {
    local document="$1"
    local expression="$2"

    jq -er "$expression" <<<"$document"
}

for command in forge cast jq; do
    command -v "$command" >/dev/null || fail "missing required command: $command"
done

outer_snapshot=""
cleanup() {
    if [[ -n "$outer_snapshot" ]]; then
        cast rpc --rpc-url "$RPC_URL" anvil_revert "$outer_snapshot" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

forge build --root "$TEST_ROOT" --quiet

chain_id="$(cast chain-id --rpc-url "$RPC_URL")"
if [[ -n "$EXPECTED_CHAIN_ID" ]]; then
    assert_eq "$chain_id" "$EXPECTED_CHAIN_ID" "chain ID"
fi

sender="$(cast wallet address --private-key "$PRIVATE_KEY")"
sender_nonce_before="$(cast nonce "$sender" --rpc-url "$RPC_URL")"
block_number_before="$(cast block-number --rpc-url "$RPC_URL")"
outer_snapshot="$(cast rpc --rpc-url "$RPC_URL" anvil_snapshot | jq -er '.')"

deployment="$({
    forge create \
        --root "$TEST_ROOT" \
        --broadcast \
        --json \
        --rpc-url "$RPC_URL" \
        --private-key "$PRIVATE_KEY" \
        "$CONTRACT" \
        --constructor-args "$INITIAL_VALUE"
})"
contract_address="$(json_field "$deployment" '.deployedTo')"
deployment_hash="$(json_field "$deployment" '.transactionHash')"

deployment_receipt="$(cast receipt "$deployment_hash" --json --rpc-url "$RPC_URL")"
assert_eq "$(json_field "$deployment_receipt" '.status')" "0x1" "deployment status"
assert_eq "$(json_field "$deployment_receipt" '.transactionHash' | tr '[:upper:]' '[:lower:]')" \
    "${deployment_hash,,}" "deployment transaction hash"
assert_eq "$(json_field "$deployment_receipt" '.contractAddress' | tr '[:upper:]' '[:lower:]')" \
    "${contract_address,,}" "deployed contract address"

code="$(cast code "$contract_address" --rpc-url "$RPC_URL")"
[[ "$code" != "0x" ]] || fail "deployed contract has no bytecode"
assert_eq "$(cast call "$contract_address" 'value()(uint256)' --rpc-url "$RPC_URL")" \
    "$INITIAL_VALUE" "constructor state"

inner_snapshot="$(cast rpc --rpc-url "$RPC_URL" anvil_snapshot | jq -er '.')"
nonce_before_mutation="$(cast nonce "$sender" --rpc-url "$RPC_URL")"
estimated_gas="$(
    cast estimate \
        "$contract_address" \
        'setValue(uint256)' \
        "$UPDATED_VALUE" \
        --from "$sender" \
        --rpc-url "$RPC_URL"
)"
((estimated_gas > 0)) || fail "gas estimate must be positive"

mutation_receipt="$(
    cast send \
        "$contract_address" \
        'setValue(uint256)' \
        "$UPDATED_VALUE" \
        --private-key "$PRIVATE_KEY" \
        --json \
        --rpc-url "$RPC_URL"
)"
mutation_hash="$(json_field "$mutation_receipt" '.transactionHash')"
mutation_block_number="$(json_field "$mutation_receipt" '.blockNumber')"
mutation_block_hash="$(json_field "$mutation_receipt" '.blockHash')"
gas_used="$(json_field "$mutation_receipt" '.gasUsed')"

assert_eq "$(json_field "$mutation_receipt" '.status')" "0x1" "mutation status"
((gas_used > 0)) || fail "receipt gasUsed must be positive"
((gas_used <= estimated_gas)) ||
    fail "receipt gasUsed ($gas_used) exceeds estimate ($estimated_gas)"
assert_eq "$(json_field "$mutation_receipt" '.logs | length')" "1" "event count"
assert_eq "$(json_field "$mutation_receipt" '.logs[0].topics[0]')" \
    "$(cast keccak 'ValueChanged(address,uint256,uint256)')" "event signature"
assert_eq "$(json_field "$mutation_receipt" '.logs[0].data')" \
    "$(cast abi-encode 'f(uint256,uint256)' "$INITIAL_VALUE" "$UPDATED_VALUE")" "event data"
indexed_caller="$(json_field "$mutation_receipt" '.logs[0].topics[1]')"
assert_eq "0x${indexed_caller:26}" "${sender,,}" "indexed event caller"

gas_values=()
if [[ "$FILECOIN_ASSERTIONS" == "1" ]]; then
    gas_breakdown="$(cast rpc --rpc-url "$RPC_URL" filecoin_getGasBreakdown "$mutation_hash" 2>&1)" ||
        fail "gas breakdown lookup failed: $gas_breakdown"
    gas_fields=(
        baseFeeBurn
        overEstimationBurn
        minerPenalty
        minerTip
        refund
        gasRefund
        gasBurned
    )
    for gas_field in "${gas_fields[@]}"; do
        gas_value="$(json_field "$gas_breakdown" ".${gas_field}" 2>&1)" ||
            fail "gas breakdown missing $gas_field: $gas_value"
        [[ "$gas_value" =~ ^0x(0|[1-9a-f][0-9a-f]*)$ ]] ||
            fail "gas breakdown $gas_field must be a minimal lowercase hex quantity, got '$gas_value'"
        gas_values+=("$gas_value")
    done
    [[ "${gas_values[0]}" != "0x0" ]] ||
        fail "gas breakdown baseFeeBurn must be nonzero"
fi

transaction="$(cast tx "$mutation_hash" --json --rpc-url "$RPC_URL")"
assert_eq "$(json_field "$transaction" '.hash')" "$mutation_hash" "indexed transaction"
assert_eq "$(json_field "$transaction" '.blockHash')" "$mutation_block_hash" \
    "transaction block hash"

block="$(cast block "$mutation_block_number" --json --rpc-url "$RPC_URL")"
assert_eq "$(json_field "$block" '.hash')" "$mutation_block_hash" "indexed block"
assert_eq "$(cast call "$contract_address" 'value()(uint256)' --rpc-url "$RPC_URL")" \
    "$UPDATED_VALUE" "mutated state"
assert_eq "$(cast nonce "$sender" --rpc-url "$RPC_URL")" "$((nonce_before_mutation + 1))" \
    "sender nonce after mutation"

assert_eq "$(cast rpc --rpc-url "$RPC_URL" anvil_revert "$inner_snapshot")" "true" \
    "inner snapshot revert"
assert_eq "$(cast call "$contract_address" 'value()(uint256)' --rpc-url "$RPC_URL")" \
    "$INITIAL_VALUE" "state after inner revert"
assert_eq "$(cast nonce "$sender" --rpc-url "$RPC_URL")" "$nonce_before_mutation" \
    "sender nonce after inner revert"
assert_eq "$(cast rpc --rpc-url "$RPC_URL" eth_getTransactionReceipt "$mutation_hash")" "null" \
    "reverted mutation receipt"
if [[ "$FILECOIN_ASSERTIONS" == "1" ]]; then
    assert_eq "$(cast rpc --rpc-url "$RPC_URL" filecoin_getGasBreakdown "$mutation_hash" 2>&1)" "null" \
        "reverted mutation gas breakdown"
fi

assert_eq "$(cast rpc --rpc-url "$RPC_URL" anvil_revert "$outer_snapshot")" "true" \
    "outer snapshot revert"
outer_snapshot=""
assert_eq "$(cast code "$contract_address" --rpc-url "$RPC_URL")" "0x" \
    "deployment cleanup"
assert_eq "$(cast nonce "$sender" --rpc-url "$RPC_URL")" "$sender_nonce_before" \
    "sender nonce after cleanup"
assert_eq "$(cast block-number --rpc-url "$RPC_URL")" "$block_number_before" \
    "block number after cleanup"
assert_eq "$(cast rpc --rpc-url "$RPC_URL" eth_getTransactionReceipt "$deployment_hash")" "null" \
    "deployment receipt cleanup"

if [[ "$FILECOIN_ASSERTIONS" == "1" ]]; then
    printf 'acceptance passed: chain=%s sender=%s contract=%s deploy=%s mutate=%s estimate=%s gasUsed=%s baseFeeBurn=%s overEstimationBurn=%s minerPenalty=%s minerTip=%s refund=%s gasRefund=%s gasBurned=%s\n' \
        "$chain_id" \
        "$sender" \
        "$contract_address" \
        "$deployment_hash" \
        "$mutation_hash" \
        "$estimated_gas" \
        "$gas_used" \
        "${gas_values[@]}"
else
    printf 'acceptance passed: chain=%s sender=%s contract=%s deploy=%s mutate=%s estimate=%s gasUsed=%s\n' \
        "$chain_id" \
        "$sender" \
        "$contract_address" \
        "$deployment_hash" \
        "$mutation_hash" \
        "$estimated_gas" \
        "$gas_used"
fi
