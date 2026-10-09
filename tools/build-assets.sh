#!/usr/bin/env bash
# Build guest inputs explicitly. Seedbox itself never starts a build environment.
set -euo pipefail
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  arm64|aarch64) architecture=aarch64; kernel_name=Image ;;
  x86_64) architecture=x86_64; kernel_name=bzImage ;;
  *) echo 'error: guest asset builds support ARM64 and x86-64' >&2; exit 2 ;;
esac
cache="${SEEDBOX_ASSET_DIR:-/tmp/seedbox-$(id -u)}"
if [[ -L "$cache" || ( -e "$cache" && ( ! -d "$cache" || ! -O "$cache" ) ) ]]; then
  echo "error: $cache must be a directory owned by your user" >&2; exit 2
fi
mkdir -p "$cache"
chmod 700 "$cache"
work=$(mktemp -d "$cache/build.XXXXXX")
container=
client=
cleanup() {
  if [[ -n "$container" ]]; then "$client" rm --force "$container" >/dev/null; fi
  rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/output"
if [[ $(uname -s) == Linux ]]; then
  echo 'assets: building Linux kernel and guest helpers with Nix' >&2
  kernel=$(nix build --no-link --print-out-paths .#guestKernel)
  runtime=$(nix build --no-link --print-out-paths .#runtimeArchive)
  cp "$kernel/$kernel_name" "$work/output/kernel"
  cp "$runtime" "$work/output/runtime.tar.gz"
else
  client=${SEEDBOX_ASSET_BUILDER:-}
  if [[ -z "$client" ]]; then
    for candidate in podman docker; do
      if command -v "$candidate" >/dev/null && "$candidate" info >/dev/null 2>&1; then client=$candidate; break; fi
    done
  fi
  if [[ -z "$client" ]]; then
    echo 'error: asset builds on Mac need a running Docker-compatible Linux engine; start Podman or Docker Desktop, then retry' >&2
    exit 2
  fi
  echo "assets: building Linux kernel and guest helpers with $client; this may take several minutes" >&2
  # Send only source files: never mount/copy Cargo targets or the Nix store.
  git ls-files --cached --others --exclude-standard -z | while IFS= read -r -d '' source; do
    if [[ -e "$source" || -L "$source" ]]; then printf '%s\0' "$source"; fi
  done > "$work/files"
  tar --null -T "$work/files" -cf "$work/source.tar"
  container=$("$client" create -i --platform linux/arm64 docker.io/nixos/nix:latest \
    sh -eu -c '
      mkdir /source /output
      tar -xf - -C /source
      cd /source
      export NIX_CONFIG="experimental-features = nix-command flakes
sandbox = false"
      kernel=$(nix build --no-link --print-out-paths .#guestKernel)
      runtime=$(nix build --no-link --print-out-paths .#runtimeArchive)
      cp "$kernel/Image" /output/kernel
      cp "$runtime" /output/runtime.tar.gz
    ')
  "$client" start --attach --interactive "$container" < "$work/source.tar"
  "$client" cp "$container:/output/." "$work/output/"
fi
# One slot per architecture. Replace files atomically; keep no version history.
mkdir -p "$cache/$architecture"
if [[ -L "$cache/$architecture" || ! -O "$cache/$architecture" ]]; then
  echo "error: invalid asset directory $cache/$architecture" >&2; exit 2
fi
mv "$work/output/kernel" "$cache/$architecture/kernel"
mv "$work/output/runtime.tar.gz" "$cache/$architecture/runtime.tar.gz"
echo "assets: ready in $cache/$architecture" >&2
