#!/usr/bin/env bash
# CI only: puts a musl C compiler and binaryen's wasm-opt on a GitHub Actions
# runner for scripts/build-release-tarball.sh. Needs Nix. Both come prebuilt
# from cache.nixos.org, from the flake's nixpkgs pin.
#
# The compiler is for crates with C code (ring); Rust links the binary
# itself with its bundled musl. The cross toolchain's tools are all prefixed
# (x86_64-unknown-linux-musl-gcc, ...), so it doesn't shadow the runner's cc.
set -euo pipefail

cd "$(dirname "$0")/.."

: "${GITHUB_PATH:?run this on GitHub Actions}" "${GITHUB_ENV:?}"

out=${RUNNER_TEMP:-/tmp}/ci-tools
mkdir -p "$out"
nix build --inputs-from . --out-link "$out/musl-cc" 'nixpkgs#pkgsCross.musl64.stdenv.cc^out'
nix build --inputs-from . --out-link "$out/binaryen" nixpkgs#binaryen

cc=x86_64-unknown-linux-musl-gcc
ar=x86_64-unknown-linux-musl-ar
for tool in "$cc" "$ar"; do
  test -x "$out/musl-cc/bin/$tool" || { echo "error: $tool not found" >&2; exit 1; }
done
"$out/musl-cc/bin/$cc" --version | head -1
"$out/binaryen/bin/wasm-opt" --version

echo "$out/musl-cc/bin" >>"$GITHUB_PATH"
echo "$out/binaryen/bin" >>"$GITHUB_PATH"
# Read by the cc crate when building C code for the musl target.
echo "CC_x86_64_unknown_linux_musl=$cc" >>"$GITHUB_ENV"
echo "AR_x86_64_unknown_linux_musl=$ar" >>"$GITHUB_ENV"
