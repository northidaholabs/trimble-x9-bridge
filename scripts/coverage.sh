#!/usr/bin/env bash
# Line coverage for the Rust workspace: the unit tests plus the end-to-end
# test (crates/x9-bridge/tests/e2e.py) against instrumented debug binaries.
#
#   scripts/coverage.sh [out-dir]          # default: target/coverage
#   COVERAGE_MIN=90 scripts/coverage.sh    # also fail below 90% of lines
#
# Needs cargo-llvm-cov and llvm-tools matching rustc (rustup component
# llvm-tools-preview, or LLVM_COV and LLVM_PROFDATA pointing at a matching
# LLVM). Writes summary.txt, summary.md and lcov.info to the out dir. The
# browser tests in crates/x9-bridge/ui-tests report the web UI's coverage
# separately.
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-target/coverage}"
mkdir -p "$out"
eval "$(cargo llvm-cov show-env --export-prefix)"
cargo llvm-cov clean --workspace
cargo test --locked
cargo build --locked
python3 crates/x9-bridge/tests/e2e.py target/debug
cargo llvm-cov report --summary-only ${COVERAGE_MIN:+--fail-under-lines "$COVERAGE_MIN"} | tee "$out/summary.txt"
cargo llvm-cov report --lcov --output-path "$out/lcov.info"
# A Markdown table for the CI job summary: one row per file, then the total.
awk 'BEGIN { print "| File | Lines | Missed | Line coverage |"; print "| --- | ---: | ---: | ---: |" }
     $1 ~ /\.rs$/ || $1 == "TOTAL" { printf "| %s | %s | %s | %s |\n", ($1 == "TOTAL" ? "**Total**" : $1), $8, $9, $10 }' \
  "$out/summary.txt" > "$out/summary.md"
