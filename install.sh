#!/bin/sh
# koto installer. Builds from source, installs the CLI, and links the agent
# skill where agents look for it.
#
#   curl -fsSL https://raw.githubusercontent.com/kowo-co/koto/main/install.sh | sh
#
# What it does, exactly:
#   1. Downloads the source tarball for one ref (default: main).
#   2. cargo build --release -p koto.
#   3. Installs the binary to ~/.local/bin/koto.
#   4. Copies the koto skill to ~/.local/share/koto/skills/koto (canonical),
#      then symlinks it into ~/.claude/skills/koto and ~/.agent/skills/koto.
# Overrides: KOTO_REPO (owner/name), KOTO_REF, KOTO_BIN_DIR.
set -eu

say()  { printf '%s\n' "$*"; }
die()  { printf 'koto install: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "koto drives Hyprland; this is Linux-only"
command -v curl  >/dev/null 2>&1 || die "curl is required"
command -v tar   >/dev/null 2>&1 || die "tar is required"
command -v cargo >/dev/null 2>&1 || die "cargo is required — https://rustup.rs"

REPO=${KOTO_REPO:-kowo-co/koto}
REF=${KOTO_REF:-main}
BIN_DIR=${KOTO_BIN_DIR:-$HOME/.local/bin}
SHARE=$HOME/.local/share/koto
SKILL=$SHARE/skills/koto

WORK=$(mktemp -d "${TMPDIR:-/tmp}/koto-install.XXXXXX")
trap 'rm -rf "$WORK"' EXIT INT TERM

say "fetching $REPO@$REF"
curl -fsSL "https://github.com/$REPO/archive/refs/heads/$REF.tar.gz" \
  | tar -xz -C "$WORK" --strip-components=1 \
  || die "download failed: https://github.com/$REPO @$REF"

say "building (release; the first build takes a few minutes)"
cargo build --release -p koto --manifest-path "$WORK/Cargo.toml" \
  || die "build failed"

mkdir -p "$BIN_DIR"
install -m 755 "$WORK/target/release/koto" "$BIN_DIR/koto"
"$BIN_DIR/koto" --explain nop >/dev/null || die "installed binary failed a parse smoke test"

# The skill lives canonically under ~/.local/share/koto; agent homes get
# symlinks, so one install serves every agent and upgrades touch one place.
rm -rf "$SKILL"
mkdir -p "$SHARE/skills"
cp -R "$WORK/skills/koto" "$SKILL"

link_skill() { # $1 = skills dir of an agent home
  mkdir -p "$1"
  if [ -e "$1/koto" ] && [ ! -L "$1/koto" ]; then
    say "  skip: $1/koto exists and is not a symlink; leaving it alone"
    return 0
  fi
  ln -sfn "$SKILL" "$1/koto"
  say "  linked $1/koto"
}

say "installing the agent skill"
link_skill "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills"
link_skill "$HOME/.agent/skills"

say ""
say "koto installed: $BIN_DIR/koto"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "note: $BIN_DIR is not on PATH; add it to your shell profile" ;;
esac
say "optional: npm i -g betterwright && betterwright setup  # enables web attach bw"
say "try: koto --explain super enter end"
