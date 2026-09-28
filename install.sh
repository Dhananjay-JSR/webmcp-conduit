#!/bin/sh
# Install conduit — turn any WebMCP-enabled website into an MCP server.
#
#   curl -fsSL https://raw.githubusercontent.com/Dhananjay-JSR/webmcp-conduit/main/install.sh | sh
#
# Environment:
#   CONDUIT_VERSION   install a specific tag (default: latest release)
#   CONDUIT_BIN_DIR   where to put the binary (default: ~/.local/bin)

set -eu

REPO="Dhananjay-JSR/webmcp-conduit"
BINARY="conduit"
BIN_DIR="${CONDUIT_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" > /dev/null 2>&1 || die "this installer needs $1"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$os" in
        Linux)  os_part="unknown-linux-gnu" ;;
        Darwin) os_part="apple-darwin" ;;
        *) die "unsupported OS: $os. Try: cargo install webmcp-conduit" ;;
    esac

    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        arm64 | aarch64) arch_part="aarch64" ;;
        *) die "unsupported architecture: $arch. Try: cargo install webmcp-conduit" ;;
    esac

    printf '%s-%s' "$arch_part" "$os_part"
}

latest_version() {
    # The API returns the tag without needing a token for a public repo.
    curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' \
        | cut -d'"' -f4
}

main() {
    need curl
    need tar

    target=$(detect_target)
    tag="${CONDUIT_VERSION:-$(latest_version)}"
    [ -n "$tag" ] || die "could not determine the latest release; set CONDUIT_VERSION"
    version="${tag#v}"

    archive="$BINARY-$version-$target.tar.gz"
    url="https://github.com/$REPO/releases/download/$tag/$archive"

    say "installing $BINARY $version ($target)"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    curl -fsSL "$url" -o "$tmp/$archive" \
        || die "no build for $target in $tag. Try: cargo install webmcp-conduit"

    # Verify against the published checksums when they are available. A
    # failed download should not silently become an installed binary.
    if curl -fsSL "https://github.com/$REPO/releases/download/$tag/SHA256SUMS" -o "$tmp/SHA256SUMS" 2>/dev/null; then
        expected=$(grep " $archive\$" "$tmp/SHA256SUMS" | cut -d' ' -f1 || true)
        if [ -n "$expected" ]; then
            if command -v sha256sum > /dev/null 2>&1; then
                actual=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
            elif command -v shasum > /dev/null 2>&1; then
                actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
            else
                actual=""
            fi
            if [ -n "$actual" ] && [ "$actual" != "$expected" ]; then
                die "checksum mismatch for $archive"
            fi
            [ -n "$actual" ] && say "checksum ok"
        fi
    fi

    tar xzf "$tmp/$archive" -C "$tmp"
    mkdir -p "$BIN_DIR"
    install -m 755 "$tmp/$BINARY-$version-$target/$BINARY" "$BIN_DIR/$BINARY" 2>/dev/null \
        || { cp "$tmp/$BINARY-$version-$target/$BINARY" "$BIN_DIR/$BINARY"; chmod 755 "$BIN_DIR/$BINARY"; }

    say "installed $BIN_DIR/$BINARY"

    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *)
            say ""
            say "$BIN_DIR is not on your PATH. Add it:"
            say "    export PATH=\"$BIN_DIR:\$PATH\""
            ;;
    esac

    say ""
    say "try it:"
    say "    $BINARY probe https://example.com"
}

main "$@"
