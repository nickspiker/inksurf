#!/usr/bin/env bash
# Build the Rust renderer for arm64 into jniLibs, wrap it in the debug APK, and with `install` push it to the adb-attached phone.
set -euo pipefail
cd "$(dirname "$0")"
cargo ndk -t arm64-v8a -p 26 -o android/app/src/main/jniLibs build --release
(cd android && ./gradlew assembleDebug)
if [[ "${1:-}" == "install" ]]; then
    adb install -r android/app/build/outputs/apk/debug/app-debug.apk
fi
