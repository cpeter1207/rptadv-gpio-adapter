#!/usr/bin/env bash
# Include the actual exported Rust DSO, not only its in-process unit test binary.
set -euo pipefail
cargo=$1
directory=$2
report=$3
shift 3

# Keep external Cargo builds in the same instrumented target directory.
export CARGO_TARGET_DIR=${CARGO_LLVM_COV_TARGET_DIR:-target/llvm-cov}

"$@" show-env --branch --export-prefix > "$directory/environment.sh"
# This file is produced by the pinned coverage tool for this workspace.
# shellcheck source=/dev/null
. "$directory/environment.sh"
"$@" clean --workspace
"$cargo" test --all-targets --locked
"$cargo" build --lib --locked

library="$CARGO_TARGET_DIR/debug/librptadv_gpio_adapter.so"
bash tests/native.sh "$library" "$directory"

# llvm-cov needs the DSO's covmap as well as the unit-test executable covmap.
LLVM_COV_FLAGS="--object=$library${LLVM_COV_FLAGS:+ $LLVM_COV_FLAGS}" \
    "$@" report --branch --json --output-path "$report"
