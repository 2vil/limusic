#!/usr/bin/env bash
# Build Vibratunes in release mode and install it on this machine — one command per OS, and each
# OS only builds what it will actually install:
#
#   Arch/Manjaro    cargo tauri build --no-bundle → makepkg → vibratunes-…-x86_64.pkg.tar.zst → pacman -U
#   Fedora/RHEL     cargo tauri build --bundles rpm        → dnf/rpm install
#   Debian/Ubuntu   cargo tauri build --bundles deb        → apt install
#   other Linux     cargo tauri build --no-bundle          → manual install to /usr/local
#   macOS           cargo tauri build --bundles app        → copy to /Applications
#   Windows (MSYS)  cargo tauri build --bundles nsis       → run the -setup.exe silently
#
#   scripts/dist-install.sh            # build + install + launch
#   scripts/dist-install.sh --no-run   # build + install, don't start the app
#   scripts/dist-install.sh android    # cargo tauri android build --apk → adb install -r
set -euo pipefail

cd "$(dirname "$0")/.."
RUN_APP=1
TARGET=""

for arg in "$@"; do
  case "$arg" in
    --no-run) RUN_APP=0 ;;
    android)  TARGET="android" ;;
    *) echo "unknown argument: $arg (supported: --no-run, android)" >&2; exit 2 ;;
  esac
done

say()  { printf '\033[1;34m▸\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# Every cargo invocation goes through this so the rustup toolchain (with the Android std)
# wins over a distro rustc that may shadow it in PATH.
export PATH="$HOME/.cargo/bin:$PATH"

command -v cargo >/dev/null 2>&1 || fail "cargo not found — install rustup: https://rustup.rs"
command -v cargo-tauri >/dev/null 2>&1 || fail "cargo-tauri not found — cargo install tauri-cli"

VERSION="$(python3 -c "import json;print(json.load(open('src-tauri/tauri.conf.json'))['version'])" 2>/dev/null || echo 1.0.0)"

# Run sudo non-interactively when possible, prompt when we have a terminal, and otherwise
# (CI, background shells) hand the command to the user instead of hanging on a password.
sudo_maybe() {
  if sudo -n true 2>/dev/null; then
    sudo "$@"
  elif [ -t 0 ]; then
    sudo "$@"
  else
    say "sudo needs a password — run this to finish the install:"
    printf '    sudo %s\n' "$*"
    return 42
  fi
}

# ---------------------------------------------------------------- android -----------------
if [ "$TARGET" = "android" ]; then
  command -v adb >/dev/null 2>&1 || fail "adb not found — install android platform-tools"
  adb get-state >/dev/null 2>&1 || fail "no Android device connected (check USB debugging)"
  say "Building the release APK…"
  cargo tauri android build --apk
  APK="$(find target -name '*.apk' -path '*release*' 2>/dev/null | head -1 || true)"
  [ -z "$APK" ] && fail "no release APK found under target/ — check the build output above"
  say "Installing $(basename "$APK") on the device…"
  adb install -r "$APK"
  ok "Installed. Launch it from the app drawer (or: adb shell monkey -p com.vibratunes.app 1)"
  exit 0
fi

# ------------------------------------------------- per-OS build targets & install ----------
OS="$(uname -s)"
case "$OS" in
  Linux)
    DISTRO=unknown
    command -v pacman >/dev/null 2>&1 && DISTRO=arch
    command -v dnf    >/dev/null 2>&1 && DISTRO=fedora
    command -v apt    >/dev/null 2>&1 && DISTRO=debian
    case "$DISTRO" in
      arch)   BUILD_ARGS=(--no-bundle) ;;  # the pacman package is built by makepkg from the binary
      fedora) BUILD_ARGS=(--bundles rpm) ;;
      debian) BUILD_ARGS=(--bundles deb) ;;
      *)      BUILD_ARGS=(--no-bundle) ;;  # manual /usr/local install from the binary
    esac
    ;;
  Darwin)  BUILD_ARGS=(--bundles app) ;;
  MINGW*|MSYS*|CYGWIN*) BUILD_ARGS=(--bundles nsis) ;;
  *) fail "unsupported OS: $OS (this script covers Linux, macOS and Windows/MSYS)" ;;
esac

say "Building the release (${BUILD_ARGS[*]:-default bundles}) — this compiles the whole workspace…"
cargo tauri build "${BUILD_ARGS[@]}"

case "$OS" in
  # ============================================================== Linux ====================
  Linux)
    ARCH="$(uname -m)"
    BIN="target/release/limusic-app"
    [ -f "$BIN" ] || BIN="target/release/vibratunes-app"
    [ -f "$BIN" ] || fail "release binary not found under target/release/"

    case "$DISTRO" in
      arch)
        # The native package: makepkg wraps the binary, .desktop and hicolor icons into a
        # pacman-installable .pkg.tar.zst — same layout the deb carries.
        say "Building the pacman package (pkg.tar.zst)…"
        PKGROOT="target/packaging/vibratunes"
        rm -rf "$PKGROOT"
        mkdir -p "$PKGROOT/usr/bin" "$PKGROOT/usr/share/applications" \
                 "$PKGROOT/usr/share/licenses/vibratunes"
        for size in 32x32 64x64 128x128 256x256; do
          mkdir -p "$PKGROOT/usr/share/icons/hicolor/$size/apps"
        done
        install -Dm755 "$BIN" "$PKGROOT/usr/bin/vibratunes"
        install -Dm644 src-tauri/icons/32x32.png     "$PKGROOT/usr/share/icons/hicolor/32x32/apps/vibratunes.png"
        install -Dm644 src-tauri/icons/64x64.png     "$PKGROOT/usr/share/icons/hicolor/64x64/apps/vibratunes.png"
        install -Dm644 src-tauri/icons/128x128.png   "$PKGROOT/usr/share/icons/hicolor/128x128/apps/vibratunes.png"
        [ -f src-tauri/icons/128x128@2x.png ] && \
          install -Dm644 src-tauri/icons/128x128@2x.png "$PKGROOT/usr/share/icons/hicolor/256x256/apps/vibratunes.png"
        install -Dm644 LICENSE "$PKGROOT/usr/share/licenses/vibratunes/LICENSE"
        cat > "$PKGROOT/usr/share/applications/vibratunes.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Vibratunes
GenericName=Music Player
Comment=YouTube Music client with offline downloads
Exec=vibratunes %u
Icon=vibratunes
Terminal=false
Categories=Audio;Music;AudioVideo;
MimeType=x-scheme-handler/vibratunes;
StartupWMClass=vibratunes
DESKTOP
        # makepkg forbids dashes (and colons) in pkgver: 1.4.0-rc.9 -> 1.4.0_rc.9
        PKGVER="${VERSION//-/_}"
        cat > "$PKGROOT/PKGBUILD" <<EOF
pkgname=vibratunes
pkgver=${PKGVER}
pkgrel=1
pkgdesc="Vibratunes — YouTube Music client with offline downloads"
arch=('${ARCH}')
license=('GPL-3.0-or-later')
depends=('mpv' 'webkit2gtk-4.1' 'gtk3')
options=(!debug)
package() { cp -a "\$startdir/usr" "\$pkgdir"/; }
EOF
        (cd "$PKGROOT" && makepkg -f >/dev/null)
        PKG="$(ls "$PKGROOT"/vibratunes-[0-9]*.pkg.tar.zst 2>/dev/null | head -1 || true)"
        [ -n "$PKG" ] || fail "makepkg produced no package"
        say "Installing $(basename "$PKG")…"
        if sudo_maybe pacman -U --noconfirm "$PKG"; then
          ok "Installed — launch with 'vibratunes' or from your app launcher."
        else
          rc=$?
          [ "$rc" -eq 42 ] && RUN_APP=0 || exit "$rc"
        fi
        ;;

      fedora)
        RPM="$(ls target/release/bundle/rpm/*.rpm 2>/dev/null | head -1 || true)"
        [ -n "$RPM" ] || fail "no .rpm under target/release/bundle/rpm — check the build output"
        say "Installing $(basename "$RPM")…"
        sudo_maybe dnf install -y "$RPM" || [ $? -eq 42 ]
        ok "Installed — launch with 'vibratunes' or from your app launcher."
        ;;

      debian)
        DEB="$(ls target/release/bundle/deb/*.deb 2>/dev/null | head -1 || true)"
        [ -n "$DEB" ] || fail "no .deb under target/release/bundle/deb — check the build output"
        say "Installing $(basename "$DEB")…"
        sudo_maybe apt install -y "$DEB" || [ $? -eq 42 ]
        ok "Installed — launch with 'vibratunes' or from your app launcher."
        ;;

      *)
        say "No known package manager — installing manually to /usr/local…"
        sudo_maybe install -Dm755 "$BIN" /usr/local/bin/vibratunes || [ $? -eq 42 ]
        sudo_maybe install -Dm644 src-tauri/icons/128x128.png \
          /usr/local/share/icons/hicolor/128x128/apps/vibratunes.png || [ $? -eq 42 ]
        sudo_maybe install -Dm644 /dev/stdin \
          /usr/local/share/applications/vibratunes.desktop <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Vibratunes
Exec=/usr/local/bin/vibratunes %u
Icon=vibratunes
Terminal=false
Categories=Audio;Music;AudioVideo;
DESKTOP
        ok "Installed to /usr/local — launch with 'vibratunes'."
        ;;
    esac
    [ "$RUN_APP" = "1" ] && { say "Launching…"; vibratunes & }
    ;;

  # ============================================================== macOS ====================
  Darwin)
    APP="$(ls -d target/release/bundle/macos/*.app 2>/dev/null | head -1 || true)"
    [ -n "$APP" ] || fail "no .app under target/release/bundle/macos — check the build output"
    say "Installing $(basename "$APP") to /Applications…"
    # Kill a running copy first: replacing a bundle in use leaves the old binary live.
    pkill -x Vibratunes 2>/dev/null || true
    rm -rf "/Applications/$(basename "$APP")"
    cp -R "$APP" /Applications/
    # Gatekeeper: locally built unsigned builds sometimes get quarantined; strip the attribute
    # so the first launch doesn't bounce.
    xattr -dr com.apple.quarantine "/Applications/$(basename "$APP")" 2>/dev/null || true
    ok "Installed to /Applications."
    [ "$RUN_APP" = "1" ] && { say "Launching…"; open "/Applications/$(basename "$APP")"; }
    ;;

  # ============================================================== Windows ==================
  MINGW*|MSYS*|CYGWIN*)
    NSIS="$(ls target/release/bundle/nsis/*-setup.exe 2>/dev/null | head -1 || true)"
    [ -n "$NSIS" ] || fail "no NSIS setup under target/release/bundle/nsis — check the build output"
    say "Running the installer $(basename "$NSIS")…"
    # /S = silent, matching the app's configured passive install mode.
    cmd //c "$(cygpath -w "$NSIS")" /S
    ok "Installed — launch from the Start Menu."
    [ "$RUN_APP" = "1" ] && { say "Launching…"; cmd //c start "" "%ProgramFiles%\\Vibratunes\\vibratunes.exe"; }
    ;;
esac

ok "Done."
