#!/usr/bin/env bash
# Build Pulse from source and install it for the current user, with an
# entry in the apps menu. No root needed: everything goes into your home
# folder.
#
#   ./install.sh              build and install (run it again to update)
#   ./install.sh --uninstall  remove Pulse (your settings are kept)
#
# Installs pulse-ui, pulse-engine and pulse-cli into ~/.local/bin and
# pulse.desktop into ~/.local/share/applications. Set PREFIX to use another
# location (default: ~/.local).
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APP_DIR="$PREFIX/share/applications"
DESKTOP_FILE="$APP_DIR/pulse.desktop"
BINARIES=(pulse-ui pulse-engine pulse-cli)
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILT="$ROOT/target/release"

say() { printf '%s\n' "$*"; }
die() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}
have() { command -v "$1" > /dev/null 2>&1; }

# Yes/no question, default yes. Without a terminal the answer is no.
ask() {
    local reply
    [[ -t 0 ]] || return 1
    read -r -p "$1 [Y/n] " reply || return 1
    [[ -z "$reply" || "$reply" == [Yy]* ]]
}

# Whether a Pulse engine answers on this user's socket.
engine_running() { [[ -x "$1" ]] && "$1" status > /dev/null 2>&1; }

usage() {
    sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

uninstall() {
    local cli="$BIN_DIR/pulse-cli"
    if engine_running "$cli"; then
        say "Stopping Pulse…"
        "$cli" shutdown > /dev/null 2>&1 || true
    fi
    local removed=0
    for f in "${BINARIES[@]/#/$BIN_DIR/}" "$DESKTOP_FILE"; do
        if [[ -e "$f" ]]; then
            rm -f "$f"
            removed=1
        fi
    done
    if [[ $removed -eq 0 ]]; then
        say "Pulse is not installed in $PREFIX."
        return
    fi
    say "Pulse was removed."
    say "Your settings (~/.config/pulse-macro) and logs (~/.local/state/pulse-macro) were kept;"
    say "delete those folders to remove them too."
}

check_build_tools() {
    # rustup installs cargo here without always adding it to PATH.
    [[ -x "$HOME/.cargo/bin/cargo" ]] && PATH="$HOME/.cargo/bin:$PATH"
    local missing=()
    have cargo || missing+=(rust)
    have cc || missing+=(gcc)
    if have pkg-config; then
        pkg-config --exists fontconfig || missing+=(fontconfig)
        pkg-config --exists freetype2 || missing+=(freetype2)
    else
        missing+=(pkgconf fontconfig freetype2)
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        say "Pulse needs these to build: ${missing[*]}"
        if have pacman && ask "Install them now with sudo pacman (asks for your password)?"; then
            sudo pacman -S --needed "${missing[@]}"
        elif have pacman; then
            die "install them with: sudo pacman -S --needed ${missing[*]}"
        else
            die "install Rust (https://rustup.rs), a C compiler, pkg-config and the fontconfig and freetype2 development files, then run ./install.sh again"
        fi
    fi
    local version major minor
    version="$(cargo --version | awk '{print $2}')"
    IFS=. read -r major minor _ <<< "$version"
    if ((major < 1 || (major == 1 && minor < 85))); then
        die "Rust $version is too old; Pulse needs 1.85 or newer (sudo pacman -Syu rust, or rustup update)"
    fi
}

install_files() {
    mkdir -p "$BIN_DIR" "$APP_DIR"
    # Copy, then rename over the old file: safe while Pulse is running.
    for b in "${BINARIES[@]}"; do
        install -m 755 "$BUILT/$b" "$BIN_DIR/.$b.new"
        mv -f "$BIN_DIR/.$b.new" "$BIN_DIR/$b"
    done
    # The menu entry starts the installed window by its full path, so it
    # works even when ~/.local/bin is not on PATH.
    local exec_path="$BIN_DIR/pulse-ui"
    [[ "$exec_path" == *[[:space:]]* ]] && exec_path="\"$exec_path\""
    awk -v exec="Exec=$exec_path" '/^Exec=/ { print exec; next } { print }' \
        "$ROOT/packaging/desktop/pulse.desktop" > "$DESKTOP_FILE.new"
    chmod 644 "$DESKTOP_FILE.new"
    mv -f "$DESKTOP_FILE.new" "$DESKTOP_FILE"
}

# An engine started before this install keeps running the old copy.
offer_restart() {
    local cli="$BIN_DIR/pulse-cli"
    engine_running "$cli" || return 0
    if ask "Pulse is running. Restart it now to use the version you just installed? Clicking stops while it restarts."; then
        "$cli" shutdown > /dev/null 2>&1 || true
        for _ in $(seq 50); do
            engine_running "$cli" || break
            sleep 0.1
        done
        if [[ -n "${DISPLAY:-}" ]]; then
            setsid -f "$BIN_DIR/pulse-ui" > /dev/null 2>&1 < /dev/null
            say "Pulse restarted."
        else
            say "Pulse stopped. Open it again from your apps menu."
        fi
    else
        say "Pulse keeps running the old copy until you quit it (Quit Pulse) and open it again."
    fi
}

main() {
    case "${1:-}" in
        "") ;;
        --uninstall)
            uninstall
            return
            ;;
        -h | --help)
            usage
            return
            ;;
        *)
            usage >&2
            exit 64
            ;;
    esac
    [[ $EUID -ne 0 ]] || die "run this as your normal user, without sudo: Pulse installs into your home folder"
    [[ -f "$ROOT/Cargo.toml" ]] || die "run install.sh from the Pulse source folder"
    if [[ "${XDG_SESSION_TYPE:-}" == wayland ]]; then
        say "Note: Pulse needs an X11 session; it does not work on Wayland."
    fi

    check_build_tools
    say "Building Pulse (the first build downloads its dependencies and takes a few minutes)…"
    (cd "$ROOT" && cargo build --release --locked -p pulse-engine -p pulse-cli -p pulse-ui) ||
        die "the build failed (see above)"

    install_files
    say "Installed Pulse into $BIN_DIR and added it to your apps menu."
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) say "Note: $BIN_DIR is not on your PATH; use its full path to run pulse-cli in a terminal." ;;
    esac
    offer_restart
    say "Open Pulse from your apps menu. To remove it later: ./install.sh --uninstall"
}

main "$@"
