#!/bin/sh

set -eu

PFFFT_REPOSITORY=https://github.com/marton78/pffft.git
PFFFT_COMMIT=e0bf595c98ded55cc457a371c1b29c8cab552628
LIBMYSOFA_REPOSITORY=https://github.com/hoene/libmysofa.git
LIBMYSOFA_COMMIT=7a0c07111a3d7230ec534e08925c24a7525f33c0
DEPLOYMENT_TARGET=15.0

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
output_dir="$script_dir/lib/ios"
build_root=${FIGHTBOX_IOS_THIRD_PARTY_BUILD_ROOT:-${TMPDIR:-/private/tmp}/fightbox-ios-third-party}

sdk_path=$(xcrun --sdk iphoneos --show-sdk-path)
clang_path=$(xcrun --sdk iphoneos --find clang)
clangxx_path=$(xcrun --sdk iphoneos --find clang++)

checkout_pinned_source() {
    repository=$1
    commit=$2
    destination=$3

    if [ ! -d "$destination/.git" ]; then
        mkdir -p "$destination"
        git -C "$destination" init
        git -C "$destination" remote add origin "$repository"
    fi

    if ! git -C "$destination" cat-file -e "$commit^{commit}" 2>/dev/null; then
        git -C "$destination" fetch --depth=1 origin "$commit"
    fi

    git -C "$destination" checkout --detach "$commit"
}

mkdir -p "$build_root/src" "$build_root/build" "$output_dir"

pffft_source=${PFFFT_SOURCE_DIR:-"$build_root/src/pffft"}
libmysofa_source=${LIBMYSOFA_SOURCE_DIR:-"$build_root/src/libmysofa"}

if [ -z "${PFFFT_SOURCE_DIR:-}" ]; then
    checkout_pinned_source "$PFFFT_REPOSITORY" "$PFFFT_COMMIT" "$pffft_source"
fi

if [ -z "${LIBMYSOFA_SOURCE_DIR:-}" ]; then
    checkout_pinned_source "$LIBMYSOFA_REPOSITORY" "$LIBMYSOFA_COMMIT" "$libmysofa_source"
fi

pffft_build="$build_root/build/pffft"
pffft_install="$build_root/install/pffft"
cmake -S "$pffft_source" -B "$pffft_build" -G "Unix Makefiles" \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DCMAKE_SYSTEM_NAME=iOS \
    -DCMAKE_C_COMPILER="$clang_path" \
    -DCMAKE_CXX_COMPILER="$clangxx_path" \
    -DCMAKE_OSX_SYSROOT="$sdk_path" \
    -DCMAKE_OSX_ARCHITECTURES=arm64 \
    -DCMAKE_OSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$pffft_install" \
    -DPFFFT_USE_TYPE_DOUBLE=OFF \
    -DPFFFT_USE_FFTPACK=OFF \
    -DPFFFT_USE_BENCH_GREEN=OFF \
    -DPFFFT_USE_BENCH_KISS=OFF \
    -DPFFFT_USE_BENCH_POCKET=OFF \
    -DTARGET_C_ARCH=armv8-a \
    -DTARGET_CXX_ARCH=armv8-a \
    -DTARGET_C_EXTRA=neon \
    -DTARGET_CXX_EXTRA=neon
cmake --build "$pffft_build" --target PFFFT --parallel
cmake --install "$pffft_build"
cmake -E copy "$pffft_install/lib/libpffft.a" "$output_dir/libpffft.a"

libmysofa_build="$build_root/build/libmysofa"
cmake -S "$libmysofa_source" -B "$libmysofa_build" -G "Unix Makefiles" \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DCMAKE_SYSTEM_NAME=iOS \
    -DCMAKE_C_COMPILER="$clang_path" \
    -DCMAKE_CXX_COMPILER="$clangxx_path" \
    -DCMAKE_OSX_SYSROOT="$sdk_path" \
    -DCMAKE_OSX_ARCHITECTURES=arm64 \
    -DCMAKE_OSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=OFF \
    -DBUILD_STATIC_LIBS=ON \
    -DBUILD_TESTS=OFF
cmake --build "$libmysofa_build" --target mysofa-static --parallel
cmake -E copy "$libmysofa_build/src/libmysofa.a" "$output_dir/libmysofa.a"

for archive in "$output_dir/libpffft.a" "$output_dir/libmysofa.a"; do
    lipo -info "$archive"
    otool -l "$archive" | awk -v archive="$archive" '
        /LC_BUILD_VERSION/ { seen = 1 }
        seen && /minos/ { print archive ": " $0; exit }
    '
done

echo "Built iPhoneOS arm64 archives for deployment target $DEPLOYMENT_TARGET:"
shasum -a 256 "$output_dir/libpffft.a" "$output_dir/libmysofa.a"
