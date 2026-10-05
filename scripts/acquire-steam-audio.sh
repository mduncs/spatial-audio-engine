#!/usr/bin/env bash
# Download the official Steam Audio 4.8.1 SDK into an ignored repo-local cache.
# It intentionally has no source-checkout or alternate-URL fallback.
set -euo pipefail

readonly version='4.8.1'
readonly url='https://github.com/ValveSoftware/steam-audio/releases/download/v4.8.1/steamaudio_4.8.1.zip'
readonly expected_size='181171027'
readonly expected_sha256='4a0aa5ec1176f38f0b0993a37c2259d9e86f27e22d5e24f83ec4c3cb9a1d5449'

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache_dir="$repo_root/.cache/steam-audio"
archive="$cache_dir/steamaudio_${version}.zip"
sdk_dir="$cache_dir/steamaudio-${version}"
marker="$sdk_dir/.fightbox-sdk-marker"

verify_archive() {
  local actual_size actual_sha256
  actual_size="$(wc -c < "$archive" | tr -d '[:space:]')"
  if [[ "$actual_size" != "$expected_size" ]]; then
    echo "Steam Audio archive has $actual_size bytes; expected $expected_size" >&2
    return 1
  fi
  if command -v shasum >/dev/null 2>&1; then
    actual_sha256="$(shasum -a 256 "$archive" | awk '{print $1}')"
  elif command -v sha256sum >/dev/null 2>&1; then
    actual_sha256="$(sha256sum "$archive" | awk '{print $1}')"
  else
    echo 'Need shasum or sha256sum to verify the Steam Audio archive.' >&2
    return 1
  fi
  if [[ "$actual_sha256" != "$expected_sha256" ]]; then
    echo "Steam Audio archive SHA-256 mismatch: $actual_sha256" >&2
    return 1
  fi
}

validate_extracted_sdk() {
  [[ -f "$marker" ]] || return 1
  grep -Fqx "version=$version" "$marker" && grep -Fqx "sha256=$expected_sha256" "$marker"
}

mkdir -p "$cache_dir"
if validate_extracted_sdk; then
  printf 'STEAM_AUDIO_SDK_DIR=%s\n' "$sdk_dir"
  exit 0
fi

if [[ -f "$archive" ]]; then
  verify_archive || { echo "Refusing to extract an unverified cached archive." >&2; exit 1; }
else
  tmp_archive="$archive.partial"
  rm -f "$tmp_archive"
  curl --fail --location --retry 3 --output "$tmp_archive" "$url"
  archive="$tmp_archive"
  verify_archive
  mv "$tmp_archive" "$cache_dir/steamaudio_${version}.zip"
  archive="$cache_dir/steamaudio_${version}.zip"
fi

command -v unzip >/dev/null 2>&1 || { echo 'Need unzip to extract the verified Steam Audio archive.' >&2; exit 1; }
stage_dir="$cache_dir/.steamaudio-${version}.extracting"
rm -rf "$stage_dir"
mkdir -p "$stage_dir"
unzip -q "$archive" -d "$stage_dir"

# The archive has a stable root today, but discover it rather than encoding a second assumption.
header="$(find "$stage_dir" -type f -name phonon.h -print -quit)"
if [[ -z "$header" ]]; then
  echo 'Verified archive did not contain phonon.h; refusing to install it.' >&2
  exit 1
fi
rm -rf "$sdk_dir"
mv "$stage_dir" "$sdk_dir"
printf 'version=%s\nsha256=%s\nurl=%s\n' "$version" "$expected_sha256" "$url" > "$marker"
printf 'STEAM_AUDIO_SDK_DIR=%s\n' "$sdk_dir"
