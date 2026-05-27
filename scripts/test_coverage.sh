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

# Pinned version — change here to upgrade everywhere.
TARPAULIN_VERSION="0.32.7"

# Install via pre-built binary (seconds) instead of compiling from source (3-5 min).
# Falls back to cargo install if the binary download fails.
install_tarpaulin() {
  local arch
  arch=$(uname -m)  # x86_64 or aarch64
  local url="https://github.com/xd009642/tarpaulin/releases/download/${TARPAULIN_VERSION}/cargo-tarpaulin-${arch}-unknown-linux-musl.tar.gz"
  echo "📦 Downloading cargo-tarpaulin ${TARPAULIN_VERSION} (pre-built)..." | tee -a "$LOG_FILE"
  if curl -fsSL "$url" | tar -xz -C "${HOME}/.cargo/bin"; then
    echo "✅ cargo-tarpaulin ${TARPAULIN_VERSION} installed from pre-built binary" | tee -a "$LOG_FILE"
  else
    echo "⚠️  Binary download failed, falling back to cargo install (slow)..." | tee -a "$LOG_FILE"
    cargo install cargo-tarpaulin --version "$TARPAULIN_VERSION" --locked
  fi
}

if ! command -v cargo-tarpaulin &>/dev/null; then
  install_tarpaulin
else
  INSTALLED=$(cargo-tarpaulin --version 2>/dev/null | grep -oP '\d+\.\d+\.\d+' || echo "unknown")
  if [[ "$INSTALLED" != "$TARPAULIN_VERSION" ]]; then
    echo "🔄 Replacing cargo-tarpaulin $INSTALLED → $TARPAULIN_VERSION" | tee -a "$LOG_FILE"
    install_tarpaulin
  else
    echo "✅ cargo-tarpaulin $INSTALLED already installed" | tee -a "$LOG_FILE"
  fi
fi

export RUSTC_BOOTSTRAP=1

COVERAGE_THRESHOLD=80

echo "📂 Running tarpaulin for workspace" | tee -a "$LOG_FILE"
mkdir -p "$COVERAGE_ROOT/workspace"

TARPAULIN_RAW_LOG="$COVERAGE_ROOT/workspace/tarpaulin_raw.log"

# Stream tarpaulin output live (visible in CI logs) while also saving to file.
# --engine llvm : compile-time instrumentation; avoids ptrace which hangs with tokio async tests.
# --timeout 120 : kill any single test that exceeds 120 s — prevents infinite hangs.
# --skip-clean  : reuse existing build artifacts for faster reruns.
set +e
cargo tarpaulin --packages timpani-n timpani-o --out Html --out Lcov --out Xml \
  --output-dir "$COVERAGE_ROOT/workspace" \
  --engine llvm --timeout 120 --skip-clean \
  --ignore-panics --no-fail-fast \
  2>&1 | tee -a "$LOG_FILE" "$TARPAULIN_RAW_LOG"
TARPAULIN_EXIT=${PIPESTATUS[0]}
set -e

if [ "$TARPAULIN_EXIT" -ne 0 ]; then
  echo "::error ::tarpaulin failed or no tests found (exit $TARPAULIN_EXIT)" | tee -a "$LOG_FILE"
  exit 1
fi

echo "✅ Coverage generated successfully" | tee -a "$LOG_FILE"

# Parse coverage from the saved raw log (line like "X.XX% coverage")
COVERAGE=$(grep -oP '\d+\.\d+(?=% coverage)' "$TARPAULIN_RAW_LOG" | tail -1)

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
