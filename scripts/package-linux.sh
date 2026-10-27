#!/usr/bin/env bash
# Build a traceable, relocatable Linux candidate from actual musl binaries.
set -euo pipefail
RELEASE_ID="${1:?usage: package-linux.sh <release-id> <target-root> <out-dir>}"
TARGET_ROOT="${2:?missing target root}"
OUT_DIR="${3:?missing output directory}"
[[ "$RELEASE_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo 'Invalid release id' >&2; exit 1; }
TARGET_ROOT="$(cd "$TARGET_ROOT" && pwd -P)"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd -P)"
SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$SOURCE_ROOT"
TRIPLE=x86_64-unknown-linux-musl
PKG="rust-trader-${RELEASE_ID}-linux-x86_64-musl"
GIT_SHA="${GITHUB_SHA:-$(git rev-parse HEAD)}"
RUNNER_DESC="${CI_RUNNER:-$(uname -srmo)}"
BUILD_TIME="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# Staging remains beside output, including when called outside the repository.
STAGE="$(mktemp -d "$OUT_DIR/.package-stage.XXXXXX")"
trap 'rm -rf -- "$STAGE"' EXIT
ROOT="$STAGE/$PKG"
mkdir -p "$ROOT/bin" "$ROOT/configs" "$ROOT/defaults" "$ROOT/examples" "$ROOT/systemd"
for bin in exchange client playback; do
    src="$TARGET_ROOT/$TRIPLE/release/$bin"
    [ -f "$src" ] || { echo "FATAL: missing built binary $src" >&2; exit 1; }
    cp "$src" "$ROOT/bin/$bin"
    chmod 755 "$ROOT/bin/$bin"
done
for config in qf_exchange_settings qf_connector_settings instruments.txt runtime.env; do
    cp "configs/$config" "$ROOT/configs/$config"
    cp "configs/$config" "$ROOT/defaults/$config"
done
cp configs/playback.txt "$ROOT/examples/playback.txt"
cp deploy/rust-trader.service "$ROOT/systemd/rust-trader.service.in"
cp traderctl "$ROOT/traderctl"
cp LICENSE "$ROOT/LICENSE"
cp DEPLOY.md "$ROOT/DEPLOY.md"
for bin in "$ROOT"/bin/*; do
    name="$(basename "$bin")"
    description="$(file -b "$bin")"
    echo "$name: $description"
    [[ "$description" =~ ELF\ 64-bit\ LSB.*x86-64 ]] || { echo "FATAL: $name is not x86-64 ELF" >&2; exit 1; }
    [[ "$description" =~ statically\ linked|static-pie\ linked ]] || { echo "FATAL: $name is not static" >&2; exit 1; }
    if readelf -l "$bin" | grep -q INTERP; then
        echo "FATAL: $name has a dynamic interpreter" >&2; exit 1
    fi
    if readelf -d "$bin" 2>/dev/null | grep -q NEEDED; then
        echo "FATAL: $name requires shared libraries" >&2; exit 1
    fi
done
if grep -rlq $'\r' "$ROOT/configs" "$ROOT/defaults" "$ROOT/examples" "$ROOT/systemd" "$ROOT/traderctl" "$ROOT/LICENSE" "$ROOT/DEPLOY.md"; then
    echo 'FATAL: CRLF found in text payload' >&2; exit 1
fi
chmod 644 "$ROOT"/configs/* "$ROOT"/defaults/* "$ROOT"/examples/* "$ROOT"/systemd/* "$ROOT/LICENSE" "$ROOT/DEPLOY.md"
chmod 755 "$ROOT/traderctl"
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
  "acceptance": "candidate; target RHEL/CentOS versions require artifact-specific acceptance"
}
EOF
( cd "$ROOT" && find . -type f ! -name SHA256SUMS -printf '%P\n' | LC_ALL=C sort | xargs -I{} sha256sum {} > SHA256SUMS )
tar -czf "$OUT_DIR/$PKG.tar.gz" -C "$STAGE" "$PKG"
( cd "$OUT_DIR" && sha256sum "$PKG.tar.gz" > "$PKG.tar.gz.sha256" && sha256sum -c "$PKG.tar.gz.sha256" )
echo "packaged: $OUT_DIR/$PKG.tar.gz"
