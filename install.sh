#!/bin/sh
# replaykit installer.
#
#   curl -fsSL https://raw.githubusercontent.com/aryxnsdfs/replaykit/main/install.sh | sh
#
# Downloads the right prebuilt binary for your OS/arch from the latest GitHub
# release and installs it to ~/.local/bin (or $REPLAYKIT_INSTALL_DIR).
set -eu

REPO="aryxnsdfs/replaykit"
BIN="replaykit"
INSTALL_DIR="${REPLAYKIT_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$1"; }

# ---- detect platform -----------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) err "unsupported OS: $os (Windows users: download the .zip from the releases page)" ;;
esac

case "$arch" in
  x86_64|amd64) arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) err "unsupported architecture: $arch" ;;
esac

target="${arch_part}-${os_part}"
asset="${BIN}-${target}.tar.gz"

# ---- resolve version -----------------------------------------------------
version="${REPLAYKIT_VERSION:-latest}"
if [ "$version" = "latest" ]; then
  info "resolving latest release"
  version="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name":' | head -n1 | sed -E 's/.*"([^"]+)".*/\1/')"
  [ -n "$version" ] || err "could not determine latest version; set REPLAYKIT_VERSION"
fi

url="https://github.com/${REPO}/releases/download/${version}/${asset}"
info "downloading ${asset} (${version})"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if ! curl -fSL "$url" -o "$tmp/$asset"; then
  err "download failed: $url
The release may not have a prebuilt binary for ${target}.
You can instead build from source:  cargo install --git https://github.com/${REPO}"
fi

# ---- extract & install ---------------------------------------------------
tar -xzf "$tmp/$asset" -C "$tmp"
binpath="$(find "$tmp" -name "$BIN" -type f | head -n1)"
[ -n "$binpath" ] || err "binary '$BIN' not found in archive"

mkdir -p "$INSTALL_DIR"
install -m 755 "$binpath" "$INSTALL_DIR/$BIN" 2>/dev/null || {
  cp "$binpath" "$INSTALL_DIR/$BIN"
  chmod 755 "$INSTALL_DIR/$BIN"
}

info "installed $BIN to $INSTALL_DIR/$BIN"

# ---- PATH hint -----------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf '\033[33mnote:\033[0m %s is not on your PATH. Add it:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR" "$INSTALL_DIR" ;;
esac

"$INSTALL_DIR/$BIN" --version || true
printf '\nRun \033[36mreplaykit setup\033[0m to get started.\n'
