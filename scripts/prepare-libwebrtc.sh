#!/usr/bin/env bash
set -euo pipefail

# Fetch the exact native artifact used by the vendored LiveKit bindings.
# No system files or existing browser profiles are changed.
cache_base="${XDG_CACHE_HOME:-${HOME}/.cache}/teams-linux/build-deps"
target_dir="${cache_base}/libwebrtc-89d790b-x64"
archive_sha="b167adad5291cea0e4d66a0454d9d52d2ad714e6b0ed70f4410317d3ebde70c5"
library_sha="daa499c9769ac884380dff248a533b8d1adb9a6c33e1a04f8e605dc0012bcb21"
if [[ "$(uname -s)-$(uname -m)" != "Linux-x86_64" ]]; then
    printf '%s\n' 'Este artefato pré-compilado é para Linux x86_64.' >&2
    exit 1
fi
mkdir -p "$cache_base"
if [[ -f "$target_dir/lib/libwebrtc.a" ]] && \
   [[ "$(sha256sum "$target_dir/lib/libwebrtc.a" | cut -d ' ' -f 1)" == "$library_sha" ]] && \
   [[ -f "$target_dir/include/third_party/libc++/src/include/span" ]]; then
    printf '%s\n' "$target_dir"
    exit 0
fi
if [[ -e "$target_dir" ]]; then
    printf '%s\n' "O cache existente está incompleto ou foi alterado: $target_dir" >&2
    exit 1
fi
staging_dir="$(mktemp -d "$cache_base/libwebrtc-download.XXXXXXXX")"
trap 'rm -rf -- "$staging_dir"' EXIT
curl --fail --location --retry 2 --silent --show-error \
    'https://github.com/livekit/rust-sdks/releases/download/webrtc-89d790b/webrtc-linux-x64-release.zip' \
    --output "$staging_dir/webrtc.zip"
printf '%s  %s\n' "$archive_sha" "$staging_dir/webrtc.zip" | sha256sum --check --status
unzip -q "$staging_dir/webrtc.zip" -d "$staging_dir"
printf '%s  %s\n' "$library_sha" "$staging_dir/linux-x64-release/lib/libwebrtc.a" | sha256sum --check --status
[[ -f "$staging_dir/linux-x64-release/include/third_party/libc++/src/include/span" ]]
mv "$staging_dir/linux-x64-release" "$target_dir"
printf '%s\n' "$target_dir"
