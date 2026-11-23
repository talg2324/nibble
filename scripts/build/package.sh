#!/usr/bin/env bash
# Pack what scripts/build/build.sh put in dist/ into release archives, in dist/release/:
#
#   scripts/build/package.sh client <triple> <version>   dist/<triple>/          -> nibble-<version>-<triple>.tar.gz
#                                                   (.zip for Windows): nib, nibd, BUILD_INFO,
#                                                   .skills/, LICENSE, README.md, icon.svg;
#                                                   macOS also nibble.icns
#   scripts/build/package.sh daemon <triple> <version>   dist/daemons/<triple>/  -> nibd-<version>-<triple>.tar.gz:
#                                                   nibd, BUILD_INFO, LICENSE
#   scripts/build/package.sh grammars <triple> <version> dist/grammars/<triple>/ -> nib-grammars-<version>-<triple>.tar.gz
#                                                   (all platforms): what `nib grammar build` wrote;
#                                                   `nib grammar add` downloads it
#
# Each archive holds one top folder named like the archive. The Linux daemons are not in the client
# archives: the installer fetches them when the user wants SSH / WSL remotes, into
# <install>/daemons/<triple>/nibd (where nib looks).
set -euo pipefail

src="${BASH_SOURCE[0]}"
while [ -L "$src" ]; do
  dir="$(cd -P "$(dirname "$src")" && pwd)"
  src="$(readlink "$src")"
  [[ "$src" != /* ]] && src="$dir/$src"
done
ROOT="$(cd -P "$(dirname "$src")/../.." && pwd)"
cd "$ROOT"

[[ $# -eq 3 ]] || { sed -n '2,16p' "$0"; exit 1; }
what="$1" triple="$2" version="$3"
ext=""; [[ "$triple" == *windows* ]] && ext=".exe"

case "$what" in
  client) from="dist/$triple"; name="nibble-$version-$triple"; files=("nib$ext" "nibd$ext" BUILD_INFO .skills) ;;
  daemon) from="dist/daemons/$triple"; name="nibd-$version-$triple"; files=(nibd BUILD_INFO) ;;
  grammars) from="dist/grammars/$triple"; name="nib-grammars-$version-$triple"; files=(manifest.toml grammars queries) ;;
  *) sed -n '2,16p' "$0"; exit 1 ;;
esac

stage="dist/release/stage/$name"
rm -rf "$stage" && mkdir -p "$stage"
for f in "${files[@]}"; do
  [[ -e "$from/$f" ]] || { echo "package: $from/$f is missing (run scripts/build/build.sh first)" >&2; exit 1; }
  cp -R "$from/$f" "$stage/"
done
[[ "$what" != grammars ]] && cp LICENSE "$stage/"
[[ "$what" == client ]] && cp README.md assets/icon.svg "$stage/"   # icon.svg: the installer's Linux .desktop entry
# nibble.icns: the icon of the installer's ~/Applications/nibble.app (Finder, Launchpad). The window
# makes its own nibble.app beside nib, with the same icon built in (crates/nib/src/app_bundle.rs).
[[ "$what" == client && "$triple" == *apple-darwin ]] && cp assets/nibble.icns "$stage/"

cd dist/release/stage
if [[ "$triple" == *windows* && "$what" != grammars ]]; then
  out="$name.zip"; rm -f "../$out"
  if command -v 7z >/dev/null; then 7z a -tzip -bso0 "../$out" "$name"; else zip -qr "../$out" "$name"; fi
else
  out="$name.tar.gz"
  tar -czf "../$out" "$name"
fi
cd ..
rm -rf "stage/$name"
echo "dist/release/$out ($(du -h "$out" | cut -f1))"
