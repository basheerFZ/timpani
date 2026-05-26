#!/bin/bash
# SPDX-FileCopyrightText: Copyright 2024
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

PROJECT_ROOT=${GITHUB_WORKSPACE:-$(cd "$(dirname "$0")/.." && pwd)}
COVERAGE_ROOT="$PROJECT_ROOT/dist/coverage"
LOG_FILE="$COVERAGE_ROOT/test_coverage_log.txt"

mkdir -p "$COVERAGE_ROOT"
rm -f "$LOG_FILE"
touch "$LOG_FILE"

echo "🧪 Starting test coverage collection..." | tee -a "$LOG_FILE"

cd "$PROJECT_ROOT/timpani_rust"

if ! command -v cargo-tarpaulin &>/dev/null; then
  echo "📦 Installing cargo-tarpaulin..." | tee -a "$LOG_FILE"
  cargo install cargo-tarpaulin
fi

export RUSTC_BOOTSTRAP=1

COVERAGE_THRESHOLD=80

echo "📂 Running tarpaulin for workspace" | tee -a "$LOG_FILE"
mkdir -p "$COVERAGE_ROOT/workspace"

TARPAULIN_OUTPUT=$(cargo tarpaulin --packages timpani-n timpani-o --out Html --out Lcov --out Xml \
  --output-dir "$COVERAGE_ROOT/workspace" \
   --timeout 60 --skip-clean \
  --ignore-panics --no-fail-fast \
  2>&1) || {
  echo "$TARPAULIN_OUTPUT" | tee -a "$LOG_FILE"
  echo "::error ::tarpaulin failed or no tests found" | tee -a "$LOG_FILE"
  exit 1
}
echo "$TARPAULIN_OUTPUT" | tee -a "$LOG_FILE"

echo "✅ Coverage generated successfully" | tee -a "$LOG_FILE"

# Parse the coverage percentage from tarpaulin output (line like "X.XX% coverage")
COVERAGE=$(echo "$TARPAULIN_OUTPUT" | grep -oP '\d+\.\d+(?=% coverage)' | tail -1)

if [ -z "$COVERAGE" ]; then
  echo "::error ::Could not parse coverage percentage from tarpaulin output" | tee -a "$LOG_FILE"
  exit 1
fi

echo "📊 Measured coverage: ${COVERAGE}%" | tee -a "$LOG_FILE"
echo "🎯 Required threshold: ${COVERAGE_THRESHOLD}%" | tee -a "$LOG_FILE"

# Compare using awk (bash arithmetic doesn't handle floats)
PASS=$(awk -v cov="$COVERAGE" -v threshold="$COVERAGE_THRESHOLD" 'BEGIN { print (cov >= threshold) ? "yes" : "no" }')

if [ "$PASS" = "yes" ]; then
  echo "✅ Coverage check passed: ${COVERAGE}% >= ${COVERAGE_THRESHOLD}%" | tee -a "$LOG_FILE"
else
  echo "::error ::Coverage check FAILED: ${COVERAGE}% is below the required ${COVERAGE_THRESHOLD}% threshold" | tee -a "$LOG_FILE"
  exit 1
fi

echo "✅ All test coverage reports generated at: $COVERAGE_ROOT" | tee -a "$LOG_FILE"