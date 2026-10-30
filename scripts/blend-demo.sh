#!/usr/bin/env bash
# Builds Blend's pool contract from source, clean and with three planted bugs, and rehearses
# each build against recent mainnet calls. Run `cargo run --release -- fetch` first.
# Needs: git, rustup (Blend pins its own Rust version), the wasm32-unknown-unknown target
# and the stellar CLI.
set -euo pipefail

BLEND_COMMIT=0bf9b74d22affa230554c0422d29936011c61e75
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK="${WORK:-$ROOT/target/blend-demo}"
mkdir -p "$WORK/builds" "$ROOT/demo"

if [ ! -d "$WORK/blend-contracts-v2" ]; then
  git clone --depth 1 https://github.com/blend-capital/blend-contracts-v2.git "$WORK/blend-contracts-v2"
fi
cd "$WORK/blend-contracts-v2"
git fetch -q --depth 1 origin "$BLEND_COMMIT"
git checkout -q "$BLEND_COMMIT"

build() {
  for c in pool-factory backstop pool; do
    cargo rustc -q --manifest-path="$c/Cargo.toml" --crate-type=cdylib --target=wasm32-unknown-unknown --release
  done
  stellar contract optimize --wasm target/wasm32-unknown-unknown/release/pool.wasm \
    --wasm-out "$WORK/builds/$1.wasm" 2>&1 | tail -1
}

must_change() {
  if git diff --quiet; then
    echo "patch for $1 did not apply" >&2
    exit 1
  fi
}

git checkout -q -- pool/src
build pool_clean

# Bug 1: submit no longer requires the spender's authorization
sed -i '460s|spender.require_auth();|// BUG 1: spender.require_auth(); removed|' pool/src/contract.rs
must_change bug1
build pool_bug1_auth
git checkout -q -- pool/src

# Bug 2: the base rate term is dropped from interest below target utilization
sed -i 's|&SCALAR_7) + i128(config.r_base);|\&SCALAR_7); // BUG 2: + r_base dropped|' pool/src/pool/interest.rs
must_change bug2
build pool_bug2_rate
git checkout -q -- pool/src

# Bug 3: the flash loan contract is dropped from the flash_loan event topics
sed -i 's|(Symbol::new(e, "flash_loan"), asset, from, contract);|(Symbol::new(e, "flash_loan"), asset, from); // BUG 3: contract dropped from topics|' pool/src/events.rs
must_change bug3
build pool_bug3_event
git checkout -q -- pool/src

cd "$ROOT"
for b in clean bug1_auth bug2_rate bug3_event; do
  echo "rehearsing pool_$b.wasm"
  cargo run --release -q -- replay "$WORK/builds/pool_$b.wasm" > "demo/report_$b.txt" 2>&1
done
echo "reports written to demo/"
