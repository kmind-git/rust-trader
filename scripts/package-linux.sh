#!/usr/bin/env bash
# Assemble the Linux musl candidate package (P1 of linux-rhel76-packaging-plan.md):
#   - collects the three target binaries, config examples, systemd unit, docs
#   - enforces the ELF static gate (file + readelf: x86-64, no PT_INTERP, no NEEDED)
#   - emits BUILD-INFO.json and inner SHA256SUMS; outer .sha256 next to the tarball
# usage: package-linux.sh <release-id> <target-root> <out-dir>
# example: scripts/package-linux.sh dev-abc1234-42 target dist
set -euo pipefail

RELEASE_ID="${1:?usage: package-linux.sh <release-id> <target-root> <out-dir>}"
TARGET_ROOT="${2:?missing target root (e.g. target)}"
OUT_DIR="${3:?missing out dir (e.g. dist)}"
TRIPLE="x86_64-unknown-linux-musl"
PKG="rust-trader-${RELEASE_ID}-linux-x86_64-musl"
GIT_SHA="${GITHUB_SHA:-$(git rev-parse HEAD)}"
RUNNER_DESC="${CI_RUNNER:-$(uname -srmo)}"
BUILD_TIME="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$OUT_DIR"
ROOT="$STAGE/$PKG"
mkdir -p "$ROOT/bin" "$ROOT/configs" "$ROOT/examples" "$ROOT/systemd"

# --- payload collection (explicit list, no glob of unknown files) ---
for bin in exchange client playback; do
    src="$TARGET_ROOT/$TRIPLE/release/$bin"
    [ -f "$src" ] || { echo "FATAL: missing built binary $src" >&2; exit 1; }
    cp "$src" "$ROOT/bin/$bin"
    chmod 755 "$ROOT/bin/$bin"
done
cp configs/qf_exchange_settings  "$ROOT/configs/qf_exchange_settings.example"
cp configs/qf_connector_settings "$ROOT/configs/qf_connector_settings.example"
cp configs/instruments.txt      "$ROOT/configs/instruments.txt.example"
cp configs/playback.txt         "$ROOT/examples/playback.txt"
cp deploy/rust-trader.service   "$ROOT/systemd/rust-trader.service"
cp LICENSE                      "$ROOT/LICENSE"
cp DEPLOY.md                    "$ROOT/DEPLOY.md"

# --- ELF gate: x86-64, statically linked, no interpreter, no NEEDED entries ---
for bin in "$ROOT"/bin/*; do
    name="$(basename "$bin")"
    echo "--- $name: $(file -b "$bin")"
    readelf -d "$bin" 2>/dev/null | grep -E 'NEEDED|INTERP' || echo "    (no NEEDED/INTERP entries)"
    file "$bin" | grep -q "ELF 64-bit LSB.*x86-64" || { echo "FATAL: $name is not an x86-64 ELF" >&2; exit 1; }
    # newer file(1) classifies rustc musl output as "static-pie linked"
    file "$bin" | grep -qE "statically linked|static-pie linked" || { echo "FATAL: $name is not statically linked" >&2; exit 1; }
    if readelf -l "$bin" | grep -q PT_INTERP; then
        echo "FATAL: $name has PT_INTERP (dynamic interpreter)" >&2; exit 1
    fi
    if readelf -d "$bin" 2>/dev/null | grep -q NEEDED; then
        echo "FATAL: $name has NEEDED shared library entries" >&2; exit 1
    fi
    echo "ELF gate OK: $name"
done

# --- line-ending / permission sanity for text payload ---
# check only the known text files: binaries legitimately contain 0x0D bytes
if grep -rlq $'\r' "$ROOT"/configs "$ROOT"/examples "$ROOT"/systemd "$ROOT"/LICENSE "$ROOT"/DEPLOY.md 2>/dev/null; then
    echo "FATAL: CRLF found in text payload" >&2; exit 1
fi
chmod 644 "$ROOT"/configs/* "$ROOT"/examples/* "$ROOT"/systemd/* "$ROOT"/LICENSE "$ROOT"/DEPLOY.md

# --- build metadata ---
LOCK_SHA="$(sha256sum Cargo.lock | cut -d' ' -f1)"
cat > "$ROOT/BUILD-INFO.json" <<EOF
{
  "release_id": "$RELEASE_ID",
  "version": "$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)",
  "git_sha": "$GIT_SHA",
  "rustc": "$(rustc -Vv | tr '\n' ' ' | sed 's/ *$//')",
  "target": "$TRIPLE",
  "build_command": "cargo build --locked --release --target $TRIPLE --bins",
  "cargo_lock_sha256": "$LOCK_SHA",
  "runner": "$RUNNER_DESC",
  "built_at_utc": "$BUILD_TIME",
  "acceptance": "candidate; RHEL 7.6 verification pending (plan §4.7)"
}
EOF

# --- inner checksums: every payload file except SHA256SUMS itself ---
( cd "$ROOT" && find . -type f ! -name SHA256SUMS -printf '%P\n' | LC_ALL=C sort \
    | xargs -I{} sha256sum {} > SHA256SUMS )

# --- outer archive + checksum ---
tar -czf "$OUT_DIR/$PKG.tar.gz" -C "$STAGE" "$PKG"
( cd "$OUT_DIR" && sha256sum "$PKG.tar.gz" > "$PKG.tar.gz.sha256" )

echo "packaged: $OUT_DIR/$PKG.tar.gz"
( cd "$OUT_DIR" && sha256sum -c "$PKG.tar.gz.sha256" )
