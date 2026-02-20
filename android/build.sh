#!/usr/bin/env bash
# android/build.sh — Build the Rust FFI library and generate Kotlin bindings.
#
# Prerequisites (see docs/clients/android.md Build Guide):
#   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   cargo install cargo-ndk uniffi-bindgen
#   ANDROID_NDK_HOME must point to the NDK root (e.g. ~/Android/Sdk/ndk/25.2.9519653)
#
# Usage:
#   android/build.sh            # release build (all three ABIs)
#   android/build.sh --debug    # debug build (faster, larger binaries)

set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFI_CRATE="$WORKSPACE_ROOT/android/tenet-ffi"
JNI_OUT="$WORKSPACE_ROOT/android/app/app/src/main/jniLibs"
KOTLIN_OUT="$WORKSPACE_ROOT/android/app/app/src/main/java/com/example/tenet/uniffi"

BUILD_MODE="--release"
if [[ "${1:-}" == "--debug" ]]; then
    BUILD_MODE=""
fi

echo "==> Building Rust FFI library (${BUILD_MODE:---release})..."
cargo ndk \
    --manifest-path "$FFI_CRATE/Cargo.toml" \
    -t aarch64-linux-android \
    -t armv7-linux-androideabi \
    -t x86_64-linux-android \
    -o "$JNI_OUT" \
    build $BUILD_MODE

echo "==> Generating Kotlin bindings..."
uniffi-bindgen generate \
    "$FFI_CRATE/src/tenet_ffi.udl" \
    --language kotlin \
    --out-dir "$KOTLIN_OUT"

echo
echo "==> Done."
echo "    .so files:       $JNI_OUT"
echo "    Kotlin bindings: $KOTLIN_OUT"
echo
echo "    Next: cd android/app && ./gradlew assembleDebug"
