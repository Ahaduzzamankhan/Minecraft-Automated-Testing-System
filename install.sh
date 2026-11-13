#!/usr/bin/env sh
# MATS one-line installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/Ahaduzzamankhan/Minecraft-Automated-Testing-System/main/install.sh | sh
#
# Environment variables:
#   MATS_VERSION      release tag to install (default: latest), e.g. v1.0.0
#   MATS_INSTALL_DIR  install directory (default: $HOME/.local/bin)
set -eu

REPO="Ahaduzzamankhan/Minecraft-Automated-Testing-System"
BIN="mats"

info() { printf '%s\n' "$*"; }
fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"

# Platform detection.
os="$(uname -s)"
case "$os" in
    Linux) os="linux" ;;
    Darwin) os="macos" ;;
    *) fail "unsupported operating system: $os" ;;
esac

arch="$(uname -m)"
case "$arch" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) fail "unsupported architecture: $arch" ;;
esac

tag="${MATS_VERSION:-}"
if [ -z "$tag" ]; then
    tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1)"
    [ -n "$tag" ] || fail "could not determine the latest MATS release"
    case "$tag" in
        v*) ;;
        *) tag="v$tag" ;;
    esac
fi

asset="$BIN-$tag-$os-$arch"
base="https://github.com/$REPO/releases/download/$tag"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading MATS $tag for $os-$arch..."
curl -fsSL -o "$tmp/$asset" "$base/$asset" || fail "could not download $asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null || true

if [ -s "$tmp/SHA256SUMS" ]; then
    expected="$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/SHA256SUMS")"
    if [ -n "$expected" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
            actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
        else
            actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
        fi
        [ "$actual" = "$expected" ] || fail "checksum verification failed for $asset"
        info "Checksum verified."
    fi
else
    info "warning: no SHA256SUMS published; continuing without checksum verification"
fi

install_dir="${MATS_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
target="$install_dir/$BIN"
if [ -e "$target" ] && [ ! -w "$target" ]; then
    fail "$target is not writable; set MATS_INSTALL_DIR to a directory you own"
fi
mv "$tmp/$asset" "$target"
chmod +x "$target"
info "Installed $target"

# Add the install directory to PATH for future shells.
case ":$PATH:" in
    *":$install_dir:"*) added="already on PATH" ;;
    *)
        line="export PATH=\"$install_dir:\$PATH\""
        added="added to PATH"
        for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
            [ -e "$rc" ] || continue
            grep -F "$line" "$rc" >/dev/null 2>&1 || printf '\n# Added by the MATS installer\n%s\n' "$line" >>"$rc"
        done
        ;;
esac

info "$added. Open a new terminal, then run: mats --version"
"$target" --version >/dev/null 2>&1 && "$target" --version || true
