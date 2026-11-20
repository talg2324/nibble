#!/usr/bin/env bash
# Release builds, organized by target, into dist/:
#
#   dist/<triple>/nib, nibd, BUILD_INFO    full client for that platform
#   dist/<triple>/.skills/                 agent skills (copied from the repo's .skills/)
#   dist/daemons/<triple>/nibd             static Linux daemons nib uploads to ssh/WSL remotes
#
# Usage:
#   scripts/build.sh                 # this machine's client + Linux daemons (x86_64, aarch64)
#   scripts/build.sh client          # this machine's client only
#   scripts/build.sh daemons         # Linux daemons only
#   scripts/build.sh client --target x86_64-pc-windows-msvc   # cross client (needs cargo-xwin)
#
# One-time setup for the Linux daemons:
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
# (nibd is pure Rust; Rust's bundled rust-lld links it. If that fails on your machine:
#  brew install zig && cargo install --locked cargo-zigbuild — used automatically when present.)
set -euo pipefail

src="${BASH_SOURCE[0]}"
while [ -L "$src" ]; do
  dir="$(cd -P "$(dirname "$src")" && pwd)"
  src="$(readlink "$src")"
  [[ "$src" != /* ]] && src="$dir/$src"
done
ROOT="$(cd -P "$(dirname "$src")/.." && pwd)"
cd "$ROOT"

HOST="$(rustc -vV | sed -n 's/^host: //p')"
DAEMON_TARGETS=(${NIB_DAEMON_TARGETS:-x86_64-unknown-linux-musl aarch64-unknown-linux-musl})
what="${1:-all}"; shift || true
client_target="$HOST"
if [[ "${1:-}" == "--target" ]]; then client_target="$2"; fi

info() {
  printf 'commit %s%s\nbuilt  %s\nhost   %s\n' \
    "$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" \
    "$(git diff --quiet 2>/dev/null || echo ' (dirty)')" \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$HOST"
}

build_client() {
  local t="$1" out="dist/$1" ext=""
  [[ "$t" == *windows* ]] && ext=".exe"
  echo "==> client for $t"
  if [[ "$t" == "$HOST" ]]; then
    cargo build --release -p nib -p nibd
    local bin="target/release"
  elif [[ "$t" == *windows-msvc ]] && command -v cargo-xwin >/dev/null; then
    cargo xwin build --release -p nib -p nibd --target "$t"
    local bin="target/$t/release"
  else
    cargo build --release -p nib -p nibd --target "$t"
    local bin="target/$t/release"
  fi
  mkdir -p "$out"
  cp "$bin/nib$ext" "$bin/nibd$ext" "$out/"
  rm -rf "$out/.skills" && cp -R .skills "$out/.skills"
  info > "$out/BUILD_INFO"
  ls -lh "$out"
}

build_daemons() {
  local runner=(cargo build)
  if command -v cargo-zigbuild >/dev/null; then
    runner=(cargo zigbuild)
  else
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
  fi
  for t in "${DAEMON_TARGETS[@]}"; do
    echo "==> nibd for $t"
    if ! rustup target list --installed 2>/dev/null | grep -qx "$t"; then
      echo "    skipped: run 'rustup target add $t' once" >&2
      continue
    fi
    "${runner[@]}" --release -p nibd --target "$t"
    mkdir -p "dist/daemons/$t"
    cp "target/$t/release/nibd" "dist/daemons/$t/nibd"
    info > "dist/daemons/$t/BUILD_INFO"
  done
}

case "$what" in
  all) build_client "$client_target"; build_daemons ;;
  client) build_client "$client_target" ;;
  daemons) build_daemons ;;
  *) sed -n '2,20p' "$0"; exit 1 ;;
esac
echo "done: $(cd dist && find . -name 'nib*' -type f | sort | tr '\n' ' ')"
