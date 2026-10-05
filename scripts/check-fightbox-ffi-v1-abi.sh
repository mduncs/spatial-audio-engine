#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_dir="$repo_root/crates/fightbox-ffi/tests/abi/v1"
public_include_dir="$repo_root/crates/fightbox-ffi/include"
cc_bin="${CC:-cc}"
expected_v1_header_sha256='94207735eddbfb6b74cdf0d4fcda67f9b72c9141ec8bbe317aa3da07bfe6749a'

if ! command -v "$cc_bin" >/dev/null 2>&1; then
  echo "C compiler not found: $cc_bin" >&2
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  actual_v1_header_sha256="$(shasum -a 256 "$fixture_dir/fightbox_v1.h" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  actual_v1_header_sha256="$(sha256sum "$fixture_dir/fightbox_v1.h" | awk '{print $1}')"
else
  echo 'Need shasum or sha256sum to verify the frozen v1 header.' >&2
  exit 1
fi
if [[ "$actual_v1_header_sha256" != "$expected_v1_header_sha256" ]]; then
  echo 'Frozen fightbox_v1.h changed; the v1 ABI fixture is immutable.' >&2
  exit 1
fi

probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/fightbox-v1-abi.XXXXXX")"
trap 'rm -rf "$probe_dir"' EXIT

common_flags=(-std=c11 -Wall -Wextra -Werror -Wpedantic)

"$cc_bin" "${common_flags[@]}" \
  -I"$fixture_dir" \
  -c "$fixture_dir/legacy_client.c" \
  -o "$probe_dir/legacy_client.o"

"$cc_bin" "${common_flags[@]}" \
  -I"$public_include_dir" \
  -I"$fixture_dir" \
  -c "$fixture_dir/current_header_stub.c" \
  -o "$probe_dir/current_header_stub.o"

"$cc_bin" \
  "$probe_dir/legacy_client.o" \
  "$probe_dir/current_header_stub.o" \
  -o "$probe_dir/current-header-probe"
"$probe_dir/current-header-probe"

if [[ "${1:-}" == "--" ]]; then
  shift
  if [[ "$#" -eq 0 ]]; then
    echo 'Expected a fightbox-ffi library path after --' >&2
    exit 1
  fi
  "$cc_bin" "$probe_dir/legacy_client.o" "$@" -o "$probe_dir/rust-library-probe"
  "$probe_dir/rust-library-probe" \
    "$repo_root/platforms/ios/FightboxApp/Resources/chicago-block-a.fightbox" \
    "$repo_root/platforms/ios/FightboxApp/Resources/chicago-block-baked"
elif [[ "$#" -ne 0 ]]; then
  echo 'Usage: check-fightbox-ffi-v1-abi.sh [-- library [linker arguments...]]' >&2
  exit 1
fi

echo 'fightbox C ABI v1 freeze: PASS'
