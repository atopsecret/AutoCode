#!/bin/bash
# Tauri 2 | Rust / JavaScript / Python
# Run: bash scripts/test-local.sh
# 不启动 App、监控或系统输入。测试包只引用生产函数与合成 fixture。
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
python3 scripts/verify-build.py --source-only
python3 -m unittest discover -s tests -p 'test_build_*.py'
npm run test:ui
npm run test:update-manifest
python3 tests/notification_sql.py
cargo_args=(--locked)
if [[ "${AUTOCODE_TEST_OFFLINE:-1}" == "1" ]]; then
    cargo_args+=(--offline)
fi
for harness in core ingestion focus; do
    cargo test "${cargo_args[@]}" --manifest-path "tests/$harness-harness/Cargo.toml" --lib
done
