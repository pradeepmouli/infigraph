#!/usr/bin/env bash
set -euo pipefail

# Infigraph installer
# Usage: curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash
#
# Override for GitHub Enterprise:
#   INFIGRAPH_GH_HOST=github.example.com INFIGRAPH_GH_OWNER=myorg bash install.sh

GHE_HOST="${INFIGRAPH_GH_HOST:-github.com}"
GHE_OWNER="${INFIGRAPH_GH_OWNER:-intuit}"
GHE_REPO="infigraph"
INSTALL_DIR="${INFIGRAPH_INSTALL_DIR:-$HOME/.local/bin}"

echo "Infigraph installer"
echo "==================="

# Detect OS and arch
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) OS_TAG="apple-darwin" ;;
  Linux)  OS_TAG="unknown-linux-gnu" ;;
  MINGW*|MSYS*|CYGWIN*) OS_TAG="pc-windows-msvc" ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64)       ARCH_TAG="x86_64" ;;
  aarch64|arm64) ARCH_TAG="aarch64" ;;
  *)            echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

TARGET="${ARCH_TAG}-${OS_TAG}"
echo "Target: ${TARGET}"
echo "Install dir: ${INSTALL_DIR}"
echo ""

# Determine archive format
if [[ "$OS_TAG" == "pc-windows-msvc" ]]; then
  ARCHIVE_EXT="zip"
  BIN_SUFFIX=".exe"
else
  ARCHIVE_EXT="tar.gz"
  BIN_SUFFIX=""
fi

# For GHE: require gh CLI + auth. For public GitHub: prefer gh, fall back to curl.
if [[ "$GHE_HOST" != "github.com" ]]; then
  if ! command -v gh &>/dev/null; then
    echo "Error: gh CLI not found. Install it first:"
    echo "  brew install gh"
    echo "Then authenticate with GHE:"
    echo "  gh auth login --hostname ${GHE_HOST}"
    exit 1
  fi
  if ! gh auth status --hostname "${GHE_HOST}" &>/dev/null; then
    echo "Error: gh not authenticated with ${GHE_HOST}"
    echo "Run: gh auth login --hostname ${GHE_HOST}"
    exit 1
  fi
fi

# Rename a running binary out of the way before overwriting.
# On Unix cp usually works (old inode stays valid), but rename is safer
# on NFS/CIFS mounts and matches the Windows install path.
move_running_binary() {
  local bin="$1"
  if [ -f "$bin" ]; then
    rm -f "${bin}.old"
    mv "$bin" "${bin}.old" 2>/dev/null || true
  fi
}

cleanup_old_binaries() {
  rm -f "$INSTALL_DIR/infigraph${BIN_SUFFIX}.old" \
        "$INSTALL_DIR/infigraph-mcp${BIN_SUFFIX}.old" 2>/dev/null || true
}

# Try pre-built binary from GHE releases
try_prebuilt() {
  echo "Checking for pre-built binary..."
  local asset_name="infigraph-${TARGET}.${ARCHIVE_EXT}"
  local download_path="/tmp/${asset_name}"

  # Get latest release
  local release_tag
  release_tag=$(gh api --hostname "${GHE_HOST}" "repos/${GHE_OWNER}/${GHE_REPO}/releases/latest" --jq '.tag_name' 2>/dev/null || echo "")

  if [ -z "$release_tag" ]; then
    echo "No releases found."
    return 1
  fi

  echo "Latest release: ${release_tag}"
  echo "Looking for: ${asset_name}"

  # Download asset
  local dl_err
  dl_err=$(GH_HOST="${GHE_HOST}" gh release download "${release_tag}" \
    --repo "${GHE_OWNER}/${GHE_REPO}" \
    --pattern "${asset_name}" \
    --dir /tmp --clobber 2>&1)
  if [ $? -eq 0 ]; then

    mkdir -p "$INSTALL_DIR"

    # Rename running binaries before overwriting
    move_running_binary "$INSTALL_DIR/infigraph${BIN_SUFFIX}"
    move_running_binary "$INSTALL_DIR/infigraph-mcp${BIN_SUFFIX}"

    if [[ "$ARCHIVE_EXT" == "zip" ]]; then
      unzip -o "${download_path}" -d "$INSTALL_DIR"
    else
      tar -xzf "${download_path}" -C "$INSTALL_DIR"
    fi

    rm -f "${download_path}"
    cleanup_old_binaries
    # Strip macOS quarantine so Gatekeeper doesn't block unsigned binaries
    if [ "$OS" = "Darwin" ]; then
      xattr -dr com.apple.quarantine "$INSTALL_DIR/infigraph" "$INSTALL_DIR/infigraph-mcp" 2>/dev/null || true
    fi
    echo "Installed pre-built binary to ${INSTALL_DIR}/"
    return 0
  fi

  echo "Download failed for ${TARGET} in release ${release_tag}:"
  echo "  ${dl_err}"
  return 1
}

# Build from source via GHE clone
build_from_source() {
  echo "Building from source..."

  # Check for cmake (required by lbug/kuzu graph DB)
  if ! command -v cmake &>/dev/null; then
    echo "cmake not found — required to build the graph database."
    if [ "$OS" = "Darwin" ]; then
      if command -v brew &>/dev/null; then
        echo "Installing cmake via Homebrew..."
        brew install cmake
      else
        echo "Error: Install cmake first: brew install cmake"; exit 1
      fi
    else
      echo "Installing cmake..."
      if command -v apt-get &>/dev/null; then
        sudo apt-get install -y cmake
      elif command -v yum &>/dev/null; then
        sudo yum install -y cmake
      elif command -v dnf &>/dev/null; then
        sudo dnf install -y cmake
      else
        echo "Error: Install cmake manually: https://cmake.org/download/"; exit 1
      fi
    fi
  fi

  # Check for cargo
  if ! command -v cargo &>/dev/null; then
    echo "cargo not found. Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi

  local src_dir="/tmp/infigraph-build"

  if [ -d "$src_dir/.git" ]; then
    echo "Updating existing clone..."
    cd "$src_dir" && git pull
  else
    echo "Cloning from ${GHE_HOST}/${GHE_OWNER}/${GHE_REPO}..."
    rm -rf "$src_dir"
    gh repo clone "${GHE_OWNER}/${GHE_REPO}" "$src_dir" -- --hostname "${GHE_HOST}" 2>/dev/null \
      || git clone "https://${GHE_HOST}/${GHE_OWNER}/${GHE_REPO}.git" "$src_dir"
    cd "$src_dir"
  fi

  echo "Building release (this may take a few minutes)..."
  cargo build --release -p infigraph-cli -p infigraph-mcp

  mkdir -p "$INSTALL_DIR"

  # Rename running binaries before overwriting
  move_running_binary "$INSTALL_DIR/infigraph${BIN_SUFFIX}"
  move_running_binary "$INSTALL_DIR/infigraph-mcp${BIN_SUFFIX}"

  cp "target/release/infigraph${BIN_SUFFIX}" "$INSTALL_DIR/"
  cp "target/release/infigraph-mcp${BIN_SUFFIX}" "$INSTALL_DIR/"
  cleanup_old_binaries
  echo "Built and installed to ${INSTALL_DIR}/"
}

# Main flow
if ! try_prebuilt; then
  build_from_source
fi

# Ensure install dir is on PATH (Unix only)
if [[ "$OS_TAG" != "pc-windows-msvc" ]]; then
  if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
    echo ""
    for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile"; do
      if [ -f "$rc" ] && ! grep -q "INFIGRAPH" "$rc"; then
        echo "# Infigraph" >> "$rc"
        echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$rc"
        echo "Added PATH entry to $rc"
        break
      fi
    done
    echo ""
    echo "Restart your shell or run:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
  fi
fi

# Auto-run infigraph install to register MCP + primary search
echo ""
if [ -x "$INSTALL_DIR/infigraph${BIN_SUFFIX}" ]; then
  echo "Registering as primary search for AI agents..."
  "$INSTALL_DIR/infigraph${BIN_SUFFIX}" install
fi

echo ""
echo "=============================="
echo "Infigraph installed!"
echo "=============================="
echo ""
echo "Next steps:"
echo "  cd /your/project"
echo "  infigraph index              # Index a project"
echo "  infigraph index --full       # Full reindex from scratch"
echo "  infigraph search 'query'     # Search indexed code"
echo ""
echo "Manage installation:"
echo "  infigraph update             # Refresh after rebuild"
echo "  infigraph uninstall          # Remove all configs"
