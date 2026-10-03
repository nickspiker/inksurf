#!/usr/bin/env bash
# Build the Rust renderer for arm64 into jniLibs and wrap it in an APK.
#   ./build.sh                 debug APK
#   ./build.sh install         debug APK, installed on the adb-attached phone
#   ./build.sh release         signed release APK (TOKEN keystore from the keys dir)
#   ./build.sh release install signed release APK, installed
set -euo pipefail
cd "$(dirname "$0")"
MODE=debug
DO_INSTALL=
for arg in "$@"; do
    case "$arg" in
        release) MODE=release ;;
        install) DO_INSTALL=1 ;;
        *) echo "unknown argument: $arg" >&2; exit 1 ;;
    esac
done

# Release signs with the canonical TOKEN key, resolved the same way as the rest of the app family: TOKEN.p12 plus a plaintext .pass beside it in the keys dir.
if [ "$MODE" = release ]; then
    KEYS_DIR=
    for d in /mnt/Harbor/Code/keys /mnt/Chiton/MEGA/Code/keys "$HOME/MEGA/code/keys" "$HOME/Code/keys"; do
        [ -f "$d/TOKEN.p12" ] && KEYS_DIR="$d" && break
    done
    [ -n "$KEYS_DIR" ] || { echo "ERROR: TOKEN.p12 not found in any known keys dir — cannot sign the release APK." >&2; exit 1; }
    export TOKEN_KEYSTORE_PATH="$KEYS_DIR/TOKEN.p12"
    export TOKEN_KEY_ALIAS=token
    if [ -z "${TOKEN_KEYSTORE_PASSWORD:-}" ] && [ -f "$KEYS_DIR/TOKEN.p12.pass" ]; then
        export TOKEN_KEYSTORE_PASSWORD="$(cat "$KEYS_DIR/TOKEN.p12.pass")"
    fi
    [ -n "${TOKEN_KEYSTORE_PASSWORD:-}" ] || { echo "ERROR: no TOKEN keystore password (expected $KEYS_DIR/TOKEN.p12.pass)." >&2; exit 1; }
fi

cargo ndk -t arm64-v8a -p 26 -o android/app/src/main/jniLibs build --release
if [ "$MODE" = release ]; then
    (cd android && ./gradlew assembleRelease)
    APK=android/app/build/outputs/apk/release/app-release.apk
else
    (cd android && ./gradlew assembleDebug)
    APK=android/app/build/outputs/apk/debug/app-debug.apk
fi
echo "APK: $APK"
if [ -n "$DO_INSTALL" ]; then
    adb install -r "$APK"
fi
