#!/bin/sh
set -eu

REPO="branchwork/branchwork"
INSTALL_DIR="${BRANCHWORK_INSTALL_DIR:-/usr/local/bin}"
BINARY_NAME="branchwork-server"

# ── Helpers ──────────────────────────────────────────────────────────────────

die() { printf "Error: %s\n" "$1" >&2; exit 1; }

info() { printf "  %s\n" "$1"; }

# ── Detect platform ─────────────────────────────────────────────────────────

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)  OS_TAG="linux" ;;
        Darwin) OS_TAG="macos" ;;
        *)      die "Unsupported OS: $OS (expected Linux or macOS)" ;;
    esac

    case "$ARCH" in
        x86_64|amd64)   ARCH_TAG="x64" ;;
        aarch64|arm64)  ARCH_TAG="arm64" ;;
        *)              die "Unsupported architecture: $ARCH (expected x86_64 or arm64)" ;;
    esac

    ARTIFACT="${BINARY_NAME}-${OS_TAG}-${ARCH_TAG}"
}

# ── Resolve version ─────────────────────────────────────────────────────────

resolve_version() {
    if [ -n "${BRANCHWORK_VERSION:-}" ]; then
        VERSION="$BRANCHWORK_VERSION"
        return
    fi

    info "Fetching latest release..."
    VERSION="$(
        curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
    )" || die "Could not determine latest release. Set BRANCHWORK_VERSION to install a specific version."

    [ -n "$VERSION" ] || die "Could not parse latest release tag."
}

# ── Download & install ───────────────────────────────────────────────────────

download_and_install() {
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}.tar.gz"

    info "Downloading ${ARTIFACT} ${VERSION}..."
    TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR"' EXIT

    curl -fsSL "$URL" -o "${TMPDIR}/${ARTIFACT}.tar.gz" \
        || die "Download failed. Check that release ${VERSION} exists and has an artifact for your platform.\n       URL: ${URL}"

    tar -xzf "${TMPDIR}/${ARTIFACT}.tar.gz" -C "$TMPDIR"

    # Install to target directory
    if [ -w "$INSTALL_DIR" ]; then
        mv "${TMPDIR}/${ARTIFACT}" "${INSTALL_DIR}/${BINARY_NAME}"
        chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
    else
        info "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${TMPDIR}/${ARTIFACT}" "${INSTALL_DIR}/${BINARY_NAME}"
        sudo chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
    fi
}

# ── Verify ───────────────────────────────────────────────────────────────────

verify() {
    if command -v "$BINARY_NAME" >/dev/null 2>&1; then
        info "Installed ${BINARY_NAME} ${VERSION} to ${INSTALL_DIR}/${BINARY_NAME}"
    else
        printf "\n"
        info "Binary installed to ${INSTALL_DIR}/${BINARY_NAME}"
        info "but ${INSTALL_DIR} is not in your PATH."
        info ""
        info "Add it with:"
        info "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    printf "Branchwork installer\n\n"

    detect_platform
    info "Platform: ${OS_TAG}-${ARCH_TAG}"

    resolve_version
    download_and_install
    verify

    printf "\nDone. Run '%s' to start.\n" "$BINARY_NAME"
}

main
