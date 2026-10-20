#!/bin/sh
# Installs the latest Kuana release on macOS or Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/berkayakin/kuana-editor/main/install.sh | sh
#
# macOS: Kuana.app goes to /Applications (~/Applications when that isn't writable). Files curl
# downloads aren't quarantined, so the app opens without the warning a browser download of a
# build without a Developer ID gets. Linux: the `kuana` binary goes to ~/.local/bin, with a
# desktop entry and icons so app launchers list it.
#
# The archive is checked against the SHA-256 in the release's latest.json. Kuana updates itself
# from then on. KUANA_INSTALL_DIR picks another folder; KUANA_NO_LAUNCH=1 skips opening it.

set -eu

REPO="berkayakin/kuana-editor"
RELEASES="https://github.com/$REPO/releases/latest/download"

say() { printf '%s\n' "$*"; }
fail() { printf 'kuana install: %s\n' "$*" >&2; exit 1; }

# The archive name or SHA-256 of a platform in latest.json (written by release.yml with
# indent=2, so each field is on a line of its own after its platform's key).
field() {
    awk -v key="\"$2\": {" -v field="\"$3\":" '
        index($0, key) { inside = 1; next }
        inside && index($0, field) { sub(/^[^:]*: *"/, ""); sub(/".*$/, ""); print; exit }
        inside && /}/ { exit }
    ' "$1"
}

sha256_of() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

download() {
    curl --proto '=https' --tlsv1.2 -fL --progress-bar -o "$2" "$1" || fail "couldn't download $1"
}

install_macos() {
    archive="$1"
    ditto -x -k "$archive" "$tmp/unpacked" || fail "couldn't unpack $(basename "$archive")"
    [ -d "$tmp/unpacked/Kuana.app" ] || fail "the archive has no Kuana.app"

    if [ -n "${KUANA_INSTALL_DIR:-}" ]; then
        dir="$KUANA_INSTALL_DIR"
    elif [ -w /Applications ]; then
        dir=/Applications
    else
        dir="$HOME/Applications"
    fi
    mkdir -p "$dir"
    app="$dir/Kuana.app"

    # Swapping the bundle under a running copy would leave it reading half-replaced files.
    if pgrep -f "^$app/Contents/MacOS/" > /dev/null 2>&1; then
        fail "Kuana is running from $app; quit it (unsaved edits are kept) and run this again"
    fi

    if [ -e "$app" ]; then
        mv "$app" "$tmp/previous.app" || fail "couldn't replace $app"
    fi
    if ! mv "$tmp/unpacked/Kuana.app" "$app"; then
        [ -e "$tmp/previous.app" ] && mv "$tmp/previous.app" "$app"
        fail "couldn't move Kuana.app into $dir"
    fi

    say "Installed Kuana $version to $app"
    if [ -z "${KUANA_NO_LAUNCH:-}" ]; then
        open "$app"
    fi
}

install_linux() {
    archive="$1"
    mkdir -p "$tmp/unpacked"
    tar -xzf "$archive" -C "$tmp/unpacked" || fail "couldn't unpack $(basename "$archive")"
    [ -f "$tmp/unpacked/kuana" ] || fail "the archive has no kuana binary"

    dir="${KUANA_INSTALL_DIR:-$HOME/.local/bin}"
    mkdir -p "$dir"
    # A rename, so a running copy keeps its old file and the next start gets the new one.
    cp "$tmp/unpacked/kuana" "$dir/.kuana.new"
    chmod 755 "$dir/.kuana.new"
    mv -f "$dir/.kuana.new" "$dir/kuana"

    # A desktop entry and icons from the release's own commit, so launchers list Kuana. Optional:
    # without them the binary still works.
    data="${XDG_DATA_HOME:-$HOME/.local/share}"
    raw="https://raw.githubusercontent.com/$REPO/v$version/assets/app-icon/linux"
    if curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp/kuana.desktop" "$raw/dev.kuana.editor.desktop"; then
        for size in 16 24 32 48 64 128 256 512; do
            icons="$data/icons/hicolor/${size}x${size}/apps"
            mkdir -p "$icons"
            curl --proto '=https' --tlsv1.2 -fsSL -o "$icons/dev.kuana.editor.png" \
                "$raw/hicolor/${size}x${size}/apps/dev.kuana.editor.png" || true
        done
        mkdir -p "$data/applications"
        # Launchers don't always have ~/.local/bin on their PATH.
        sed "s|^Exec=kuana |Exec=$dir/kuana |" "$tmp/kuana.desktop" > "$data/applications/dev.kuana.editor.desktop"
        command -v update-desktop-database > /dev/null 2>&1 && update-desktop-database "$data/applications" 2> /dev/null || true
    fi

    say "Installed Kuana $version to $dir/kuana"
    case ":$PATH:" in
        *":$dir:"*) ;;
        *) say "Note: $dir isn't on your PATH; add it to run \`kuana\` from a terminal." ;;
    esac
    if [ -z "${KUANA_NO_LAUNCH:-}" ] && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; }; then
        nohup "$dir/kuana" > /dev/null 2>&1 &
    fi
}

main() {
    case "$(uname -s)" in
        Darwin) platform=macos ;;
        Linux)
            case "$(uname -m)" in
                x86_64 | amd64) platform=linux-x86_64 ;;
                aarch64 | arm64) platform=linux-aarch64 ;;
                *) fail "there's no Linux build for $(uname -m)" ;;
            esac
            ;;
        *) fail "this script is for macOS and Linux; Windows has an installer on https://github.com/$REPO/releases/latest" ;;
    esac
    command -v curl > /dev/null 2>&1 || fail "curl is needed"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    download "$RELEASES/latest.json" "$tmp/latest.json"
    version=$(sed -n 's/^ *"version": *"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
    name=$(field "$tmp/latest.json" "$platform" name)
    sha256=$(field "$tmp/latest.json" "$platform" sha256)
    [ -n "$version" ] && [ -n "$name" ] && [ -n "$sha256" ] ||
        fail "the latest release has no build for $platform yet"

    say "Downloading Kuana $version ($name)"
    download "$RELEASES/$name" "$tmp/$name"
    [ "$(sha256_of "$tmp/$name")" = "$sha256" ] ||
        fail "$name doesn't match the SHA-256 in latest.json; nothing was installed"

    case "$platform" in
        macos) install_macos "$tmp/$name" ;;
        *) install_linux "$tmp/$name" ;;
    esac
}

# Everything runs from here, so a download cut short can't run half the script.
main "$@"
