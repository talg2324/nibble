#!/usr/bin/env bash
# nibble installer for macOS, Linux and WSL. On Windows, use install.ps1.
#
#   curl -fsSL https://github.com/talg2324/nibble/releases/latest/download/install.sh | bash
#   curl -fsSL …/install.sh | bash -s -- --yes --remotes
#   curl -fsSL …/install.sh | bash -s -- --uninstall
#   curl -fsSL …/install.sh | NIB_VERSION=v0.4-rc.1 bash      # that release (also --version TAG)
#
# Installs the latest release into ~/.local/lib/nib, with `nib` and `nibd` in ~/.local/bin.
# Never into ~/.local/share/nib: that is where nib keeps what is yours (conversations, each
# project's NIBDIR, agents' work trees). Installers before v0.4 put the program there too; this
# one takes the program's files out of it, by name, and leaves the rest.
# "Latest" never means a prerelease (a release candidate, vX.Y-rc.N): those only by name.
# Running it again updates in place. Bash 3.2 compatible (macOS /bin/bash).
set -u

REPO="talg2324/nibble"
PREFIX="${NIB_PREFIX:-$HOME/.local}"
YES=0; UNINSTALL=0
VERSION="${NIB_VERSION:-}"   # a release tag; empty = the latest
# -1 = not decided yet (ask, or take the default)
OPT_REMOTES=-1; OPT_PATH=-1; OPT_UPSTREAM=-1
MARK="# added by the nib installer"
DAEMON_TRIPLES="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

usage() { cat <<EOF
nibble installer

  --yes, -y            no questions; take the defaults (also when there is no terminal)
  --prefix DIR         install under DIR (default: ~/.local)
  --version TAG        that release (v0.4, v0.4-rc.1) instead of the latest; also NIB_VERSION
  --[no-]remotes       the Linux daemon, for opening projects on Linux machines over SSH
  --no-modify-path     leave your shell's rc file alone
  --[no-]upstream      the nib-upstream skill (off unless asked)
  --uninstall          remove nib
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    -y|--yes) YES=1 ;;
    --prefix) [ $# -ge 2 ] || { echo "--prefix needs a folder" >&2; exit 2; }; PREFIX="$2"; shift ;;
    --version) [ $# -ge 2 ] || { echo "--version needs a tag (v0.4)" >&2; exit 2; }; VERSION="$2"; shift ;;
    --remotes) OPT_REMOTES=1 ;;        --no-remotes) OPT_REMOTES=0 ;;
    --no-modify-path) OPT_PATH=0 ;;
    --upstream) OPT_UPSTREAM=1 ;;      --no-upstream) OPT_UPSTREAM=0 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done
case "$PREFIX" in /*) ;; *) PREFIX="$(pwd)/$PREFIX" ;; esac

# ── look: nib's default theme ───────────────────────────────────────────────
# 24-bit where the terminal says so, else the nearest xterm-256 colour.
if [ -t 1 ]; then
  case "${COLORTERM:-}" in
    truecolor|24bit)
      c()  { printf '\033[38;2;%d;%d;%dm' "0x${1:0:2}" "0x${1:2:2}" "0x${1:4:2}"; }
      bg() { printf '\033[48;2;%d;%d;%dm' "0x${1:0:2}" "0x${1:2:2}" "0x${1:4:2}"; } ;;
    *)
      c256() { case "$1" in
        ffffff) echo 231;; d4d4d4) echo 252;; 6e7681) echo 243;; 3b8eea) echo 33;;
        007acc) echo 32;; 3fb950) echo 71;; e5c07b) echo 180;; f85149) echo 203;;
        264f78) echo 24;; *) echo 7;; esac; }
      c()  { printf '\033[38;5;%sm' "$(c256 "$1")"; }
      bg() { printf '\033[48;5;%sm' "$(c256 "$1")"; } ;;
  esac
  WHITE=$(c ffffff); FG=$(c d4d4d4); D=$(c 6e7681); A=$(c 3b8eea); LOGO=$(c 007acc)
  G=$(c 3fb950); Y=$(c e5c07b); RED=$(c f85149); SEL=$(bg 264f78)
  B=$'\033[1m'; R=$'\033[0m'"$FG"; EL=$'\033[K'
else WHITE='' FG='' D='' A='' LOGO='' G='' Y='' RED='' SEL='' B='' R='' EL=''; fi

# Questions come from the terminal, not stdin: under `curl | bash` stdin is the script.
TTY=""
if { : </dev/tty; } 2>/dev/null; then TTY=/dev/tty; fi
[ -z "$TTY" ] && YES=1

TMPD=$(mktemp -d 2>/dev/null || mktemp -d -t nib-install)
cleanup() {
  [ -t 1 ] && printf '\033[?25h\033[0m'
  rm -rf "$TMPD"
  [ -n "${HOME_DIR:-}" ] && rm -rf "$HOME_DIR.new"
}
trap cleanup EXIT
trap 'echo; echo "  ${D}cancelled.${R}"; exit 130' INT

say()   { printf '%s\n' "$*"; }
rule()  { printf '  %s─────────────────────────────────────────────────────────────%s\n' "$D" "$R"; }
tilde() { case "$1" in "$HOME"*) echo "~${1#"$HOME"}" ;; *) echo "$1" ;; esac; }
die()   { say "  ${RED}✗${R} $*"; exit 1; }
note()  { echo "$*" >> "$TMPD/notes"; }   # shown after the steps (steps run in a subshell)

# assets/logo.svg: three bits and a fourth, blue, with a bite out of its corner.
banner() {
  say ""
  say "    ${WHITE}▄████▄ ▄████▄ ▄████▄ ${LOGO}▄██▄${R}"
  say "    ${WHITE}██████ ██████ ██████ ${LOGO}█████▄${R}"
  say "    ${WHITE}▀████▀ ▀████▀ ▀████▀ ${LOGO}▀████▀${R}"
  say "               ${B}${WHITE}nibble${R}"
  say ""
}

# ── downloads ───────────────────────────────────────────────────────────────
fetch() {  # fetch URL FILE
  if command -v curl >/dev/null 2>&1; then curl -fsSL --retry 2 -o "$2" "$1"
  else wget -q -O "$2" "$1"; fi
}
latest_tag() {
  # releases/latest redirects to …/releases/tag/vX.Y (no API call, no rate limit)
  local url="https://github.com/$REPO/releases/latest" to=""
  if command -v curl >/dev/null 2>&1; then
    to=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$url" 2>/dev/null)
  else
    to=$(wget -S --spider --max-redirect=0 "$url" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -1 | tr -d '\r')
  fi
  case "$to" in */tag/*) echo "${to##*/tag/}" ;; *) return 1 ;; esac
}
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
verify() {  # verify FILE: against SHA256SUMS from the same release
  local want; want=$(awk -v f="$(basename "$1")" '$2 == f || $2 == "*"f { print $1 }' "$TMPD/SHA256SUMS")
  [ -n "$want" ] || { echo "$(basename "$1") is not in SHA256SUMS"; return 1; }
  [ "$(sha256 "$1")" = "$want" ] || { echo "$(basename "$1"): checksum does not match"; return 1; }
}
# A folder's contents as one hash: tells whether the user edited a skill we installed.
dirhash() {
  (cd "$1" && find . -type f | LC_ALL=C sort | while IFS= read -r f; do printf '%s %s\n' "$f" "$(sha256 "$f")"; done) \
    | { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; } | cut -d' ' -f1
}

# ── the machine ─────────────────────────────────────────────────────────────
detect() {
  OS=$(uname -s); ARCH=$(uname -m)
  case "$ARCH" in arm64|aarch64) ARCH=aarch64 ;; x86_64|amd64) ARCH=x86_64 ;; esac
  WSL=0
  case "$OS" in
    Darwin) TRIPLE="$ARCH-apple-darwin"; OSNAME="macOS $(sw_vers -productVersion 2>/dev/null)" ;;
    Linux)  TRIPLE="$ARCH-unknown-linux-gnu"; OSNAME="Linux"
            [ -r /etc/os-release ] && OSNAME=$(. /etc/os-release; echo "${PRETTY_NAME:-Linux}")
            grep -qi microsoft /proc/version 2>/dev/null && WSL=1 ;;
    *) say "  ${RED}✗${R} $OS isn't supported by this script. On Windows, in PowerShell:"
       say "    ${A}irm https://github.com/$REPO/releases/latest/download/install.ps1 | iex${R}"
       exit 1 ;;
  esac
  case "$TRIPLE" in
    aarch64-apple-darwin|x86_64-unknown-linux-gnu) ;;
    *) die "There is no release build for $OSNAME on $ARCH yet. Build it from source: https://github.com/$REPO#getting-started" ;;
  esac
  SH=$(basename "${SHELL:-sh}")
  case "$SH" in
    zsh)  RC="$HOME/.zshrc" ;;
    bash) if [ "$OS" = Darwin ]; then RC="$HOME/.bash_profile"; else RC="$HOME/.bashrc"; fi ;;
    fish) RC="$HOME/.config/fish/conf.d/nib.fish" ;;
    *)    RC="$HOME/.profile" ;;
  esac
  BIN="$PREFIX/bin"; HOME_DIR="$PREFIX/lib/nib"; STATE="$HOME_DIR/install-state"
  # Where installers before v0.4 put the program: also nib's data folder, so never removed whole.
  OLD_DIR="$PREFIX/share/nib"
  LEGACY=0; [ -e "$OLD_DIR/nib" ] || [ -e "$OLD_DIR/install-state" ] && LEGACY=1
  # The state to read: this install's, or the one an earlier installer left.
  STATE_READ="$STATE"; [ -f "$STATE" ] || { [ -f "$OLD_DIR/install-state" ] && STATE_READ="$OLD_DIR/install-state"; }
  ON_PATH=0; case ":$PATH:" in *":$BIN:"*) ON_PATH=1 ;; esac
  COLS=$(stty size 2>/dev/null <"${TTY:-/dev/tty}" | cut -d' ' -f2); COLS=${COLS:-${COLUMNS:-100}}
  SSH_HOSTS=0
  [ -r "$HOME/.ssh/config" ] && SSH_HOSTS=$(grep -ciE '^[[:space:]]*host[[:space:]]+[^*?]+$' "$HOME/.ssh/config")
  HAVE=""
  if [ -x "$HOME_DIR/nib" ]; then HAVE=$("$HOME_DIR/nib" --version 2>/dev/null | head -1)
  elif [ -x "$OLD_DIR/nib" ]; then HAVE=$("$OLD_DIR/nib" --version 2>/dev/null | head -1); fi
  HAD_DAEMONS=0; [ -d "$HOME_DIR/daemons" ] || [ -d "$OLD_DIR/daemons" ] && HAD_DAEMONS=1
  HARN=""; for h in claude codex gemini opencode ollama; do command -v $h >/dev/null 2>&1 && HARN="$HARN $h"; done
  HARN="${HARN# }"
  command -v git >/dev/null 2>&1 && GIT=1 || GIT=0
  command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || die "This needs curl or wget."
  command -v tar >/dev/null 2>&1 || die "This needs tar."
}

row() { printf '  %s%-9s%s %s\n' "$D" "$1" "$R" "$2"; }
show_machine() {
  row machine "$OSNAME · $ARCH$([ $WSL = 1 ] && echo " · ${A}WSL${R}")"
  row release "$TAG"
  row into    "$(tilde "$HOME_DIR")  ${D}·  commands in $(tilde "$BIN")${R}"
  [ -n "$HAVE" ] && row found "$HAVE  ${D}— replaced in place, your settings stay${R}"
  row agents  "${HARN:-${Y}none found${R}  ${D}nib works without; its agents tab needs one${R}}"
  [ $GIT = 0 ] && row git "${Y}not found${R}  ${D}diffs and agent branches need it${R}"
  if [ $WSL = 1 ]; then
    say ""
    say "  ${A}ℹ${R}  You're inside WSL, so this is ${B}Linux${R} nib, drawn on Windows through WSLg."
    say "     ${D}Prefer nib as a Windows app that opens your WSL projects? Run install.ps1 in${R}"
    say "     ${D}PowerShell instead; it reaches WSL directly, no SSH involved.${R}"
  fi
  say ""
}

defaults() {
  if [ $OPT_REMOTES = -1 ]; then
    if [ $HAD_DAEMONS = 1 ] || [ "$SSH_HOSTS" -gt 0 ]; then OPT_REMOTES=1; else OPT_REMOTES=0; fi
  fi
  [ $OPT_PATH = -1 ] && OPT_PATH=1
  if [ $OPT_UPSTREAM = -1 ]; then
    OPT_UPSTREAM=0
    [ -f "$STATE_READ" ] && grep -q '/nib-upstream ' "$STATE_READ" && OPT_UPSTREAM=1
  fi
}

# ── the checklist ───────────────────────────────────────────────────────────
# Parallel arrays (bash 3.2 has no associative arrays). VAR "" = always installed.
LABEL=(); HINT=(); VAR=()
add() { LABEL+=("$1"); HINT+=("$2"); VAR+=("$3"); }
items() {
  LABEL=(); HINT=(); VAR=()
  add "nib + nibd"     "the editor, its daemon, the app icon and agent skills" ""
  add "SSH into Linux" "the Linux daemon nib uploads to servers, x86_64 + aarch64$([ "$SSH_HOSTS" -gt 0 ] && echo "  ·  $SSH_HOSTS hosts in ~/.ssh/config")" OPT_REMOTES
  if [ $ON_PATH = 1 ]; then add "Add to PATH" "$(tilde "$BIN") is on PATH already; nothing to write" OPT_PATH
  else add "Add to PATH" "one line in $(tilde "$RC"), so \`nib\` works in any terminal" OPT_PATH; fi
  add "nib-upstream"   "agents may draft issues / PRs to $REPO, sent only with your OK" OPT_UPSTREAM
}
get()  { [ -z "$1" ] && echo 1 || eval "echo \$$1"; }
set_() { eval "$1=$2"; }
fit() {  # a hint, cut to the terminal's width
  local w=$(( COLS - 28 )) h="$1"
  [ $w -lt 10 ] && { echo ""; return; }
  [ ${#h} -gt $w ] && h="${h:0:$((w - 1))}…"
  echo "$h"
}

draw() {
  local i n=${#LABEL[@]} box h
  for ((i=0; i<n; i++)); do
    if [ -z "${VAR[$i]}" ]; then box="${D}[■]"
    elif [ "$(get "${VAR[$i]}")" = 1 ]; then box="${A}[✓]"; else box="${D}[ ]"; fi
    h=$(fit "${HINT[$i]}")
    if [ $i = "$CUR" ]; then
      printf '%s %s▸ %s %s%-17s%s%s%s\033[0m%s\n' "$SEL" "$A" "$box" "$WHITE" "${LABEL[$i]}" "$FG" "$h" "$EL" "$R"
    else
      printf '   %s %s%-17s%s%s%s%s\n' "$box" "$FG" "${LABEL[$i]}" "$D" "$h" "$R" "$EL"
    fi
  done
  printf '%s\n  %s↑↓%s move  %sspace%s toggle  %senter%s install  %sq%s quit%s\n' \
    "$EL" "$A" "$D" "$A" "$D" "$A" "$D" "$A" "$D" "$R$EL"
}

choose() {
  items; CUR=1
  local n=${#LABEL[@]} key rest var
  say "  ${B}${WHITE}What to install${R}"
  say ""
  printf '\033[?25l'
  draw
  while :; do
    IFS= read -rsn1 key <"$TTY"
    if [ "$key" = $'\033' ]; then IFS= read -rsn2 -t 1 rest <"$TTY"; key="$key$rest"; fi
    case "$key" in
      $'\033[A'|k) CUR=$(( (CUR - 1 + n) % n )) ;;
      $'\033[B'|j) CUR=$(( (CUR + 1) % n )) ;;
      ' '|x) var="${VAR[$CUR]}"
             if [ -n "$var" ]; then [ "$(get "$var")" = 1 ] && set_ "$var" 0 || set_ "$var" 1; fi ;;
      '') break ;;
      q|Q) echo; say "  ${D}nothing was changed.${R}"; exit 0 ;;
    esac
    printf '\033[%dA' $((n + 2)); draw
  done
  printf '\033[?25h'
  say ""
}

# ── steps ───────────────────────────────────────────────────────────────────
FRAMES=(⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏)
# step "title" "detail" function [args]: runs it with a spinner; stops the install if it fails.
step() {
  local t="$1" what="$2" i=0 pid rc
  shift 2
  what=$(COLS=$((COLS - ${#t} + 22)) fit "$what")
  ( "$@" ) >"$TMPD/log" 2>&1 &
  pid=$!
  if [ -t 1 ]; then
    printf '\033[?25l'
    while kill -0 $pid 2>/dev/null; do
      printf '\r\033[K  %s%s%s %s  %s%s%s' "$A" "${FRAMES[$((i % 10))]}" "$R" "$t" "$D" "$what" "$R"
      sleep 0.08; i=$((i + 1))
    done
    printf '\033[?25h'
  fi
  wait $pid; rc=$?
  if [ $rc -ne 0 ]; then
    printf '\r%s  %s✗%s %s  %s%s%s\n' "$EL" "$RED" "$R" "$t" "$D" "$what" "$R"
    sed 's/^/      /' "$TMPD/log" | tail -15
    say ""; say "  ${RED}Stopped.${R} $([ -n "$HAVE" ] && echo "Your installed nib is as it was." || echo "Nothing was installed.")"
    exit 1
  fi
  printf '\r%s  %s✓%s %s  %s%s%s\n' "$EL" "$G" "$R" "$t" "$D" "$what" "$R"
}

do_download() {
  local REL="https://github.com/$REPO/releases/download/$TAG"
  fetch "$REL/SHA256SUMS" "$TMPD/SHA256SUMS"
  fetch "$REL/nibble-$TAG-$TRIPLE.tar.gz" "$TMPD/nibble-$TAG-$TRIPLE.tar.gz"
  if [ $OPT_REMOTES = 1 ]; then
    for t in $DAEMON_TRIPLES; do fetch "$REL/nibd-$TAG-$t.tar.gz" "$TMPD/nibd-$TAG-$t.tar.gz"; done
  fi
}
do_verify() {
  local f
  for f in "$TMPD"/*.tar.gz; do verify "$f" || return 1; done
}
# Releases made before provenance was (v0.4): nothing to check them against.
unattested() { case "$TAG" in v0.[0-3]|v0.[0-3][.-]*|v0.4|v0.4-rc.*) return 0 ;; esac; return 1; }
# SHA256SUMS comes from the release the archives come from: it tells of a broken download, not
# of a release somebody else filled. Provenance does: GitHub's record (Sigstore) that this very
# file was built by the release workflow of $REPO. Checking it takes the GitHub CLI, signed in;
# without one it is not checked, and the summary says so.
can_attest() { command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; }
do_attest() {
  local f
  for f in "$TMPD"/*.tar.gz; do
    gh attestation verify "$f" --repo "$REPO" --signer-workflow "$REPO/.github/workflows/release.yml" >/dev/null 2>"$TMPD/attest.err" || {
      echo "$(basename "$f") was not built by $REPO's release workflow, or GitHub has no record that it was:"
      tail -5 "$TMPD/attest.err"
      return 1
    }
  done
}
do_unpack() {
  # Unpacked beside the installed one, then swapped in (do_swap): a failed step leaves it as it was.
  rm -rf "$HOME_DIR.new" "$TMPD/x" && mkdir -p "$TMPD/x" "$(dirname "$HOME_DIR")"
  tar -xzf "$TMPD/nibble-$TAG-$TRIPLE.tar.gz" -C "$TMPD/x"
  mv "$TMPD/x/nibble-$TAG-$TRIPLE" "$HOME_DIR.new"
  if [ $OPT_REMOTES = 1 ]; then
    for t in $DAEMON_TRIPLES; do
      tar -xzf "$TMPD/nibd-$TAG-$t.tar.gz" -C "$TMPD/x"
      mkdir -p "$HOME_DIR.new/daemons/$t"
      mv "$TMPD/x/nibd-$TAG-$t/nibd" "$HOME_DIR.new/daemons/$t/nibd"
      chmod +x "$HOME_DIR.new/daemons/$t/nibd"
    done
  fi
  [ -f "$STATE_READ" ] && cp "$STATE_READ" "$HOME_DIR.new/install-state"
  chmod +x "$HOME_DIR.new/nib" "$HOME_DIR.new/nibd"
  if [ "$OS" = Darwin ]; then xattr -dr com.apple.quarantine "$HOME_DIR.new" 2>/dev/null || true; fi
}
# What an install is made of (scripts/build/package.sh, the daemons, this script's state, and the
# nibble.app the window makes beside nib on macOS).
PROGRAM="nib nibd BUILD_INFO .skills LICENSE README.md icon.svg nibble.icns nibble.app daemons install-state"
# rm_program DIR [LIKE]: take the program's files out of DIR, by name (and whatever is named as
# in LIKE, a newer install), then DIR itself if nothing is left. Never the folder whole: what
# nib did not put there is somebody's, and before v0.4 that was the user's conversations and
# work trees.
rm_program() {
  local dir="$1" like="${2:-}" n left
  [ -d "$dir" ] || return 0
  for n in $PROGRAM; do rm -rf "${dir:?}/$n"; done
  if [ -n "$like" ] && [ -d "$like" ]; then
    for n in "$like"/* "$like"/.[!.]*; do
      [ -e "$n" ] && rm -rf "${dir:?}/$(basename "$n")"
    done
  fi
  rmdir "$dir" 2>/dev/null && return 0
  # nib's data folder: what is left there is the user's, as it should be.
  [ "$dir" = "$OLD_DIR" ] && return 0
  left=$(ls -A "$dir" | tr '\n' ' ')
  note "${D}left in $(tilde "$dir"), not the installer's: ${left% }${R}"
}
do_swap() {
  # An interrupted run's: its program files only, like any other.
  rm_program "$HOME_DIR.old" "$HOME_DIR.new"
  [ -d "$HOME_DIR.old" ] && { echo "$HOME_DIR.old is in the way: move it aside and run this again"; return 1; }
  [ -d "$HOME_DIR" ] && mv "$HOME_DIR" "$HOME_DIR.old"
  mv "$HOME_DIR.new" "$HOME_DIR"
  rm_program "$HOME_DIR.old" "$HOME_DIR"
  # An install from before v0.4, in nib's data folder: the program leaves it, the data stays.
  if [ $LEGACY = 1 ]; then
    rm_program "$OLD_DIR"
    note "${D}nib moved from $(tilde "$OLD_DIR") to $(tilde "$HOME_DIR"); your conversations and work trees stay in $(tilde "$OLD_DIR")${R}"
  fi
  return 0
}
# A two-line script rather than a symlink: nib finds its daemon and skills next to the real binary.
wrapper() {  # wrapper NAME
  local f="$BIN/$1"
  if [ -e "$f" ] && ! grep -q "$MARK" "$f" 2>/dev/null; then
    note "${Y}!${R} left $(tilde "$f") alone: it isn't the installer's. Remove it, or run $(tilde "$HOME_DIR/$1")."
    return 0
  fi
  printf '#!/bin/sh\n%s\nexec "%s" "$@"\n' "$MARK" "$HOME_DIR/$1" > "$f.tmp" && chmod +x "$f.tmp" && mv "$f.tmp" "$f"
}
do_link() {
  mkdir -p "$BIN"
  wrapper nib && wrapper nibd
}
do_path() {
  grep -q "$MARK" "$RC" 2>/dev/null && return 0
  mkdir -p "$(dirname "$RC")"
  if [ "$SH" = fish ]; then printf '\nfish_add_path "%s"  %s\n' "$BIN" "$MARK" >> "$RC"
  else printf '\nexport PATH="%s:$PATH"  %s\n' "$BIN" "$MARK" >> "$RC"; fi
}
# Skills go to both places agents look. One the user edited is kept, and they're told where the
# new one is; the state file holds the hash of what was installed.
do_skills() {
  local src="$HOME_DIR/.skills" dest name dir new had cur newstate="$TMPD/state"
  : > "$newstate"
  for dest in "$HOME/.claude/skills" "$HOME/.agents/skills"; do
    mkdir -p "$dest"
    for dir in "$src"/*/; do
      [ -d "$dir" ] || continue
      name=$(basename "$dir")
      new=$(dirhash "$dir")
      had=$(awk -v p="$dest/$name" '$1 == p { print $2 }' "$STATE" 2>/dev/null)
      if [ "$name" = nib-upstream ] && [ $OPT_UPSTREAM = 0 ]; then
        # unchecked: take back one we put there, unless it was edited
        if [ -n "$had" ] && [ -d "$dest/$name" ] && [ "$(dirhash "$dest/$name")" = "$had" ]; then rm -rf "${dest:?}/$name"; fi
        continue
      fi
      if [ -d "$dest/$name" ]; then
        cur=$(dirhash "$dest/$name")
        if [ "$cur" != "$new" ] && [ "$cur" != "$had" ]; then
          note "${Y}!${R} kept your edited $(tilde "$dest/$name"); the new one: diff -ru $(tilde "$dest/$name") $(tilde "$src/$name")"
          echo "$dest/$name $had" >> "$newstate"
          continue
        fi
        rm -rf "${dest:?}/$name"
      fi
      cp -R "$src/$name" "$dest/$name"
      echo "$dest/$name $new" >> "$newstate"
    done
  done
  mv "$newstate" "$STATE"
}
# macOS: nibble.app, for Finder, Launchpad and Spotlight. Its script starts the installed binary,
# which runs its window from a nibble.app of its own, beside it (crates/nib/src/app_bundle.rs).
# Linux: nibble.desktop, named for the window's app id ("nibble"), so docks match the two.
do_icon() {
  if [ "$OS" = Darwin ]; then
    local app="$HOME/Applications/nibble.app"
    rm -rf "$HOME/Applications/nib.app"   # an earlier installer's
    rm -rf "$app" && mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    local icon=""
    if [ -f "$HOME_DIR/nibble.icns" ]; then
      cp "$HOME_DIR/nibble.icns" "$app/Contents/Resources/nibble.icns"
      icon="  <key>CFBundleIconFile</key><string>nibble</string>"
    fi
    cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>nibble</string>
  <key>CFBundleDisplayName</key><string>nibble</string>
  <key>CFBundleIdentifier</key><string>com.github.talg2324.nibble</string>
  <key>CFBundleExecutable</key><string>nib</string>
$icon
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${TAG#v}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
    printf '#!/bin/sh\ncd "$HOME"\nexec "%s" "$@"\n' "$HOME_DIR/nib" > "$app/Contents/MacOS/nib"
    chmod +x "$app/Contents/MacOS/nib"
  else
    local apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    mkdir -p "$apps"
    rm -f "$apps/nib.desktop"   # an earlier installer's
    {
      echo "[Desktop Entry]"
      echo "Type=Application"
      echo "Name=nibble"
      echo "Comment=nibble, an editor for working with agents"
      echo "Exec=$HOME_DIR/nib %F"
      [ -f "$HOME_DIR/icon.svg" ] && echo "Icon=$HOME_DIR/icon.svg"
      echo "StartupWMClass=nibble"
      echo "Terminal=false"
      echo "Categories=Development;TextEditor;"
    } > "$apps/nibble.desktop"
    # WSLg makes Start-menu shortcuts (which Windows needs to pin an app) only from system
    # folders, never ~/.local/share/applications. It watches /usr/share/applications (on every
    # distro) while it runs, so the shortcut appears at once.
    if [ $WSL = 1 ]; then
      sudo -n install -m 644 "$apps/nibble.desktop" /usr/share/applications/nibble.desktop 2>/dev/null ||
        note "${Y}!${R} To pin nibble on Windows: ${A}sudo cp $(tilde "$apps/nibble.desktop") /usr/share/applications/${R}"
    fi
  fi
}
do_check() { "$HOME_DIR/nib" --version; }

install() {
  local H; H=$(tilde "$HOME_DIR")
  rule
  step "Download"     "nibble-$TAG-$TRIPLE.tar.gz$([ $OPT_REMOTES = 1 ] && echo " + the Linux daemons")" do_download
  step "Check sha256" "against the release's SHA256SUMS" do_verify
  if unattested; then note "${D}$TAG was released before nib's builds carried provenance: not checked${R}"
  elif can_attest; then step "Provenance" "built by $REPO's release workflow (gh attestation verify)" do_attest
  else note "${D}provenance not checked: that takes the GitHub CLI, signed in (gh auth login), then run this again${R}"; fi
  step "Unpack"       "into $H" do_unpack
  step "Swap in"      "$([ -n "$HAVE" ] && echo "replacing $HAVE" || echo "$H")" do_swap
  step "Commands"     "nib, nibd in $(tilde "$BIN")" do_link
  if [ $OPT_PATH = 1 ] && [ $ON_PATH = 0 ]; then
    step "Add to PATH" "one line in $(tilde "$RC")" do_path
  fi
  step "Agent skills" "into ~/.claude/skills, ~/.agents/skills$([ $OPT_UPSTREAM = 1 ] && echo ", with nib-upstream")" do_skills
  step "App icon"     "$([ "$OS" = Darwin ] && echo "~/Applications/nibble.app" || echo "nibble.desktop")" do_icon
  step "Check"        "nib --version" do_check
  rule
  [ -s "$TMPD/notes" ] && { say ""; sed 's/^/  /' "$TMPD/notes"; }
}

finish() {
  say ""
  say "  ${G}${B}nib $TAG is installed.${R}"
  say ""
  if [ $OPT_PATH = 1 ] && [ $ON_PATH = 0 ]; then
    say "  Open a new terminal, or   ${A}source $(tilde "$RC")${R}"
  elif [ $ON_PATH = 0 ]; then
    say "  Not on PATH; run it as    ${A}$(tilde "$BIN")/nib${R}"
  fi
  say "  Open a project            ${A}nib .${R}"
  [ $OPT_REMOTES = 1 ] && say "  Open one over SSH         ${A}nib @myserver${R}  ${D}nibd goes up by itself${R}"
  say "  Update                    ${D}run this installer again${R}"
  say "  Remove                    ${A}curl -fsSL https://github.com/$REPO/releases/latest/download/install.sh | bash -s -- --uninstall${R}"
  say ""
}

# ── uninstall ───────────────────────────────────────────────────────────────
un_skills() {
  [ -f "$STATE_READ" ] || return 0
  local p h
  while read -r p h; do
    [ -d "$p" ] || continue
    if [ "$(dirhash "$p")" = "$h" ]; then rm -rf "$p"; else note "${Y}!${R} kept your edited $(tilde "$p")"; fi
  done < "$STATE_READ"
}
un_links() {
  local n
  for n in nib nibd; do grep -q "$MARK" "$BIN/$n" 2>/dev/null && rm -f "$BIN/$n"; done
  return 0
}
un_path() {
  local f
  for f in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile" "$HOME/.config/fish/conf.d/nib.fish"; do
    grep -q "$MARK" "$f" 2>/dev/null || continue
    grep -v "$MARK" "$f" > "$f.nib-tmp" && cat "$f.nib-tmp" > "$f" && rm -f "$f.nib-tmp"
  done
}
un_icon() {
  rm -rf "$HOME/Applications/nibble.app" "$HOME/Applications/nib.app"
  rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/applications/nibble.desktop" "${XDG_DATA_HOME:-$HOME/.local/share}/applications/nib.desktop"
  # Only the copy do_icon put there: on WSL, and launching this install's nib (not a package's
  # entry, nor another user's install).
  if [ $WSL = 1 ] && grep -qxF "Exec=$HOME_DIR/nib %F" /usr/share/applications/nibble.desktop 2>/dev/null; then
    sudo -n rm -f /usr/share/applications/nibble.desktop 2>/dev/null ||
      note "${Y}!${R} Also remove the Start-menu entry: ${A}sudo rm /usr/share/applications/nibble.desktop${R}"
  fi
}
un_program() {
  rm_program "$HOME_DIR"
  [ $LEGACY = 1 ] && rm_program "$OLD_DIR"
  return 0
}

uninstall() {
  local ans
  say "  ${B}${WHITE}Remove nib${R}  ${D}your settings in ~/.config/nib and your projects stay${R}"
  say "  ${D}so do your conversations and agents' work trees, in $(tilde "${XDG_DATA_HOME:-$HOME/.local/share}/nib")${R}"
  say ""
  [ -d "$HOME_DIR" ] || [ -e "$BIN/nib" ] || [ $LEGACY = 1 ] || { say "  ${D}nib isn't installed in $(tilde "$PREFIX").${R}"; exit 0; }
  if [ $YES = 0 ]; then
    printf '  Remove it? %s[y/N]%s ' "$D" "$R"; IFS= read -r ans <"$TTY"
    case "$ans" in y|Y|yes) ;; *) say "  ${D}nothing was changed.${R}"; exit 0 ;; esac
  fi
  rule
  step "Agent skills" "the ones it installed and you didn't edit" un_skills
  step "Commands"     "nib, nibd in $(tilde "$BIN")" un_links
  step "PATH line"    "the line marked by the installer" un_path
  step "App icon"     "nibble.app / nibble.desktop" un_icon
  step "Program"      "$(tilde "$HOME_DIR")$([ $LEGACY = 1 ] && echo ", and nib's files in $(tilde "$OLD_DIR")")" un_program
  rule
  [ -s "$TMPD/notes" ] && { say ""; sed 's/^/  /' "$TMPD/notes"; }
  say ""; say "  ${G}${B}nib is removed.${R}"; say ""
}

main() {
  printf '%s' "$FG"
  banner
  detect
  if [ $UNINSTALL = 1 ]; then uninstall; return; fi
  if [ -n "$VERSION" ]; then
    case "$VERSION" in v*) TAG="$VERSION" ;; *) TAG="v$VERSION" ;; esac
  else
    TAG=$(latest_tag) || die "Couldn't reach GitHub to find the latest release (https://github.com/$REPO/releases)."
  fi
  defaults
  show_machine
  if [ $YES = 0 ]; then choose; else
    items; say "  ${D}--yes: installing${R}"; CUR=-1; draw | sed '$d' | sed '$d'; say ""; fi
  install
  finish
}
main
