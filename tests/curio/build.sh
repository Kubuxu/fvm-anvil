#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
source_tree=${CURIO_SOURCE:-$root/extern/curio}
work=${CURIO_BUILD_DIR:-$root/.curio-build}
expected=f5a416f633d82d293e278e7b1bb4ca135e0b6e16
actual=$(git -C "$source_tree" rev-parse HEAD)
if [[ "$actual" != "$expected" ]]; then
  printf 'Curio revision mismatch: expected %s, found %s\n' "$expected" "$actual" >&2
  exit 1
fi
if [[ -e "$work" ]]; then
  printf 'Build directory already exists; select a new CURIO_BUILD_DIR: %s\n' "$work" >&2
  exit 1
fi
mkdir -p "$work"
# All generated files and module modifications are confined to this copy.
cp -a --reflink=auto "$source_tree/." "$work/"
mkdir -p "$work/cmd/fvm-anvil-acceptance" "$work/bin"
cp "$root/tests/curio/client/main.go" "$work/cmd/fvm-anvil-acceptance/main.go"
make -C "$work" 2k-skiff
cp "$work/curio" "$work/bin/skiff"
(cd "$work" && go build -tags='skiff,2k' -o bin/pdptool ./cmd/pdptool && go build -tags='skiff,2k' -o bin/client ./cmd/fvm-anvil-acceptance)
printf 'Binaries: %s/bin/{skiff,pdptool,client}\n' "$work"
