#!/bin/sh
set -eu

ZCCACHE_INSTALL_MODE="${ZCCACHE_INSTALL_MODE:-user}"
ZCCACHE_INSTALL_REPO="${ZCCACHE_INSTALL_REPO:-zackees/zccache}"
ZCCACHE_INSTALL_BASE_URL="${ZCCACHE_INSTALL_BASE_URL:-}"
ZCCACHE_INSTALL_VERSION="${ZCCACHE_INSTALL_VERSION:-latest}"
ZCCACHE_NO_MODIFY_PATH="${ZCCACHE_NO_MODIFY_PATH:-0}"

usage() {
    cat <<'EOF'
Usage: install.sh [--user|--global] [--bin-dir PATH] [--version VERSION]

Environment:
  ZCCACHE_INSTALL_MODE      user or global
  ZCCACHE_INSTALL_DIR       explicit install directory
  ZCCACHE_INSTALL_VERSION   latest or a specific version/tag
  ZCCACHE_INSTALL_REPO      GitHub repo owner/name
  ZCCACHE_INSTALL_BASE_URL  Override release base URL (for testing/mirrors)
  ZCCACHE_NO_MODIFY_PATH    1 to skip shell profile updates
EOF
}

log() {
    printf '[zccache-install] %s\n' "$*"
}

die() {
    printf '[zccache-install] ERROR: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

append_path_line() {
    profile="$1"
    line="$2"
    [ -f "$profile" ] || : >"$profile"
    grep -F "$line" "$profile" >/dev/null 2>&1 || printf '\n%s\n' "$line" >>"$profile"
}

modify_path() {
    install_dir="$1"
    case ":${PATH:-}:" in
        *:"$install_dir":*) return 0 ;;
    esac
    if [ "$ZCCACHE_NO_MODIFY_PATH" = "1" ]; then
        return 0
    fi
    export_line="export PATH=\"$install_dir:\$PATH\""
    append_path_line "$HOME/.profile" "$export_line"
    if [ -n "${SHELL:-}" ] && [ "$(basename "$SHELL")" = "zsh" ]; then
        append_path_line "$HOME/.zprofile" "$export_line"
    fi
    log "Added $install_dir to shell startup PATH configuration."
}

normalize_arch() {
    case "$1" in
        x86_64|amd64) printf 'x86_64' ;;
        arm64|aarch64) printf 'aarch64' ;;
        *) die "unsupported architecture: $1" ;;
    esac
}

detect_target() {
    os="$(uname -s)"
    arch="$(normalize_arch "$(uname -m)")"
    case "$os" in
        Linux) printf '%s-unknown-linux-musl' "$arch" ;;
        Darwin) printf '%s-apple-darwin' "$arch" ;;
        *) die "unsupported operating system: $os" ;;
    esac
}

resolve_tag() {
    version="$1"
    case "$version" in
        latest) resolve_latest_tag ;;
        *) printf '%s' "$version" ;;
    esac
}

asset_tag() {
    tag="$1"
    case "$tag" in
        v*) printf '%s' "$tag" ;;
        *) printf 'v%s' "$tag" ;;
    esac
}

release_base_url() {
    if [ -n "$ZCCACHE_INSTALL_BASE_URL" ]; then
        printf '%s' "${ZCCACHE_INSTALL_BASE_URL%/}"
    else
        printf 'https://github.com/%s/releases' "$ZCCACHE_INSTALL_REPO"
    fi
}

resolve_latest_tag() {
    latest_url="$(release_base_url)/latest"
    resolved=""
    if command -v curl >/dev/null 2>&1; then
        resolved="$(curl -LsS -o /dev/null -w '%{url_effective}' "$latest_url" 2>/dev/null || true)"
    elif command -v wget >/dev/null 2>&1; then
        resolved="$(wget -O /dev/null --max-redirect=10 --server-response "$latest_url" 2>&1 \
            | awk 'tolower($1) == "location:" { loc = $2 } END { gsub(/\r/, "", loc); print loc }')"
    else
        die "either curl or wget is required"
    fi
    tag="${resolved%/}"
    tag="${tag##*/}"
    [ -n "$tag" ] && [ "$tag" != "latest" ] || die "could not resolve latest release tag from $latest_url"
    printf '%s' "$tag"
}

asset_url() {
    tag="$1"
    asset="$2"
    base="$(release_base_url)"
    printf '%s/download/%s/%s' "$base" "$tag" "$asset"
}

download() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$dest" "$url"
    else
        die "either curl or wget is required"
    fi
}

extract_archive() {
    archive="$1"
    dest="$2"
    mkdir -p "$dest"
    tar -xzf "$archive" -C "$dest"
}

main() {
    install_dir="${ZCCACHE_INSTALL_DIR:-}"
    version="$ZCCACHE_INSTALL_VERSION"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --user) ZCCACHE_INSTALL_MODE="user" ;;
            --global) ZCCACHE_INSTALL_MODE="global" ;;
            --bin-dir)
                shift
                [ "$#" -gt 0 ] || die "--bin-dir requires a value"
                install_dir="$1"
                ;;
            --version)
                shift
                [ "$#" -gt 0 ] || die "--version requires a value"
                version="$1"
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                die "unknown argument: $1"
                ;;
        esac
        shift
    done

    need_cmd tar
    need_cmd mktemp

    if [ -z "$install_dir" ]; then
        if [ "$ZCCACHE_INSTALL_MODE" = "global" ]; then
            install_dir="/usr/local/bin"
        else
            install_dir="$HOME/.local/bin"
        fi
    fi

    target="$(detect_target)"
    tag="$(resolve_tag "$version")"
    archive_tag="$(asset_tag "$tag")"
    asset="zccache-${archive_tag}-${target}.tar.gz"
    url="$(asset_url "$tag" "$asset")"

    tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t zccache-install)"
    trap 'rm -rf "$tmpdir"' EXIT INT TERM

    archive="$tmpdir/$asset"
    log "Downloading $url"
    download "$url" "$archive"
    extract_archive "$archive" "$tmpdir"

    archive_root="$tmpdir/zccache-${archive_tag}-${target}"
    [ -d "$archive_root" ] || die "archive layout was not recognized"

    mkdir -p "$install_dir"
    cp "$archive_root"/zccache "$install_dir"/
    cp "$archive_root"/zccache-daemon "$install_dir"/
    if [ -f "$archive_root/zccache-fp" ]; then
        cp "$archive_root"/zccache-fp "$install_dir"/
    fi
    chmod 755 "$install_dir"/zccache "$install_dir"/zccache-daemon 2>/dev/null || true
    [ -f "$install_dir/zccache-fp" ] && chmod 755 "$install_dir"/zccache-fp 2>/dev/null || true

    if [ "$ZCCACHE_INSTALL_MODE" = "user" ]; then
        modify_path "$install_dir"
    fi

    log "Installed to $install_dir"
    if ! command -v zccache >/dev/null 2>&1; then
        log "Open a new shell or export PATH=\"$install_dir:\$PATH\" before running zccache."
    fi
}

main "$@"
