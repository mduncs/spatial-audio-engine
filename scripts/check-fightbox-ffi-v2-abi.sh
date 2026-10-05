#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_dir="$repo_root/crates/fightbox-ffi/tests/abi/v2"
public_include_dir="$repo_root/crates/fightbox-ffi/include"
cc_bin="${CC:-cc}"

if ! command -v "$cc_bin" >/dev/null 2>&1; then
  echo "C compiler not found: $cc_bin" >&2
  exit 1
fi

probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/fightbox-v2-abi.XXXXXX")"
trap 'rm -rf "$probe_dir"' EXIT

common_flags=(-std=c11 -Wall -Wextra -Werror -Wpedantic)

"$cc_bin" "${common_flags[@]}" \
  -I"$public_include_dir" \
  -I"$fixture_dir" \
  "$fixture_dir/header_contract.c" \
  -o "$probe_dir/header-contract"
"$probe_dir/header-contract"

"$cc_bin" "${common_flags[@]}" \
  -DFB_V2_ADVERSARIAL_DIAGNOSTIC_SELF_TEST \
  -I"$public_include_dir" \
  -I"$fixture_dir" \
  "$fixture_dir/adversarial_client.c" \
  -o "$probe_dir/adversarial-diagnostic-self-test"
diagnostic_output=""
if diagnostic_output="$("$probe_dir/adversarial-diagnostic-self-test" 2>&1)"; then
  diagnostic_exit_code=0
else
  diagnostic_exit_code=$?
fi
expected_diagnostic='fightbox C ABI v2 contract: FAIL contract=diagnostic.self-test exit_code=125'
if [[ "$diagnostic_exit_code" -ne 125 ||
      "$diagnostic_output" != "$expected_diagnostic" ]]; then
  printf 'V2 diagnostic self-test mismatch: exit_code=%s output=%q\n' \
    "$diagnostic_exit_code" "$diagnostic_output" >&2
  exit 1
fi

"$cc_bin" "${common_flags[@]}" \
  -I"$public_include_dir" \
  -I"$fixture_dir" \
  -c "$fixture_dir/adversarial_client.c" \
  -o "$probe_dir/adversarial_client.o"

if [[ "${1:-}" == "--" ]]; then
  shift
  if [[ "$#" -eq 0 ]]; then
    echo 'Expected a fightbox-ffi library path after --' >&2
    exit 1
  fi
  bash "$repo_root/scripts/check-fightbox-ffi-v1-abi.sh" -- "$@"
  "$cc_bin" "$probe_dir/adversarial_client.o" "$@" \
    -o "$probe_dir/rust-library-probe"
  "$probe_dir/rust-library-probe" \
    "$repo_root/platforms/ios/FightboxApp/Resources/chicago-block-a.fightbox" \
    "$repo_root/platforms/ios/FightboxApp/Resources/chicago-block-baked"
elif [[ "$#" -eq 0 ]]; then
  bash "$repo_root/scripts/check-fightbox-ffi-v1-abi.sh"
else
  echo 'Usage: check-fightbox-ffi-v2-abi.sh [-- library [linker arguments...]]' >&2
  exit 1
fi

echo 'fightbox C ABI v2 contract: PASS'
