#!/usr/bin/env bash
# Build Tachyon for Windows from WSL.
#
# Rust's MSVC toolchain cannot link from inside WSL, and cmd.exe refuses to run
# with a \\wsl.localhost UNC path as its working directory. So we mirror the
# source onto the Windows filesystem and drive the Windows-side cargo through
# WSL interop, then copy the binary back.
#
#   ./build-windows.sh                 release build
#   ./build-windows.sh debug           debug build
#   ./build-windows.sh release run     build, then launch it
#   ./build-windows.sh test            run the portable test suite on Windows

set -euo pipefail

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WIN_USER_DIR="${TACHYON_WIN_DIR:-/mnt/c/Users/$(cmd.exe /c "echo %USERNAME%" 2>/dev/null | tr -d '\r\n')}"
BUILD_DIR="$WIN_USER_DIR/.tachyon-build"
CMD=/mnt/c/Windows/System32/cmd.exe

PROFILE="${1:-release}"
ACTION="${2:-}"

if [[ ! -d "$WIN_USER_DIR" ]]; then
    echo "error: cannot find the Windows user directory ($WIN_USER_DIR)" >&2
    echo "       set TACHYON_WIN_DIR to override" >&2
    exit 1
fi

echo ">> syncing source to $BUILD_DIR"
mkdir -p "$BUILD_DIR"
# Only the inputs; target/ stays on the Windows side so rebuilds are incremental.
if command -v rsync >/dev/null 2>&1; then
    rsync -a --delete \
        --exclude 'target/' --exclude '.git/' \
        "$SRC_DIR"/src "$SRC_DIR"/assets "$SRC_DIR"/tests "$BUILD_DIR"/
    rsync -a "$SRC_DIR"/Cargo.toml "$SRC_DIR"/build.rs "$BUILD_DIR"/
    [[ -f "$SRC_DIR/Cargo.lock" ]] && rsync -a "$SRC_DIR"/Cargo.lock "$BUILD_DIR"/
else
    rm -rf "$BUILD_DIR/src" "$BUILD_DIR/assets" "$BUILD_DIR/tests"
    cp -r "$SRC_DIR"/src "$SRC_DIR"/assets "$SRC_DIR"/tests "$BUILD_DIR"/
    cp "$SRC_DIR"/Cargo.toml "$SRC_DIR"/build.rs "$BUILD_DIR"/
    [[ -f "$SRC_DIR/Cargo.lock" ]] && cp "$SRC_DIR"/Cargo.lock "$BUILD_DIR"/
fi

# Translate the build directory back into a Windows path.
WIN_PATH="C:${BUILD_DIR#/mnt/c}"
WIN_PATH="${WIN_PATH//\//\\}"

case "$PROFILE" in
    test)
        echo ">> cargo test in $WIN_PATH"
        "$CMD" /c "cd /d $WIN_PATH && cargo test" 2>&1 | tr -d '\r'
        exit ${PIPESTATUS[0]}
        ;;
    debug)  CARGO_ARGS="build";           OUT_SUB="debug"   ;;
    *)      CARGO_ARGS="build --release"; OUT_SUB="release" ;;
esac

echo ">> cargo $CARGO_ARGS in $WIN_PATH"
"$CMD" /c "cd /d $WIN_PATH && cargo $CARGO_ARGS" 2>&1 | tr -d '\r'

EXE="$BUILD_DIR/target/$OUT_SUB/tachyon.exe"
if [[ ! -f "$EXE" ]]; then
    echo "error: build produced no binary" >&2
    exit 1
fi

mkdir -p "$SRC_DIR/dist"
cp "$EXE" "$SRC_DIR/dist/tachyon.exe"
cp "$BUILD_DIR/assets/tachyon.ico" "$SRC_DIR/dist/tachyon.ico" 2>/dev/null || true
SIZE=$(stat -c%s "$SRC_DIR/dist/tachyon.exe")
echo ">> dist/tachyon.exe ($((SIZE / 1024)) KiB)"

if [[ "$ACTION" == "run" ]]; then
    WIN_EXE="C:${EXE#/mnt/c}"
    echo ">> launching"
    "$CMD" /c "start \"\" \"${WIN_EXE//\//\\}\"" >/dev/null 2>&1
fi
