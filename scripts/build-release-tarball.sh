#!/usr/bin/env bash
# Builds the release tarball that flake.nix's `gavmo-bin` installs: a static
# (musl) `gavmo` daemon plus the control panel's site assets. Used by both
# ci.yml (to catch breakage early) and release.yml.
#
#   scripts/build-release-tarball.sh <version> <out-dir>
#
# Writes <out-dir>/gavmo-<version>-x86_64-linux.tar.gz, laid out as
# bin/gavmo, share/gavmo/site/ and share/doc/gavmo/. <version> must match
# Cargo.toml's workspace version.
#
# Needs: the toolchain from rust-toolchain.toml (including its musl
# target), a C compiler for the musl target (Ubuntu: `musl-tools`, for
# ring), cargo-leptos, a wasm-bindgen CLI matching Cargo.lock, binaryen's
# wasm-opt, and readelf.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <version> <out-dir>" >&2
  exit 2
fi
version=$1
out_dir=$2
triple=x86_64-unknown-linux-musl

cd "$(dirname "$0")/.."

cargo_version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
if [ "$version" != "$cargo_version" ]; then
  echo "error: version $version does not match Cargo.toml's $cargo_version" >&2
  exit 1
fi

# The musl target links a fully static binary. The site assets don't
# depend on the server's target.
cargo leptos build --release --cargo-locked --bin-cargo-args="--target=$triple"

bin="${CARGO_TARGET_DIR:-target}/$triple/release/gavmo"
# cargo-leptos puts the site under the workspace's target/, whatever
# CARGO_TARGET_DIR says.
site=target/site

# It must run on any Linux, with no dynamic loader or libc from the host.
if readelf --program-headers "$bin" | grep -q INTERP; then
  echo "error: $bin is dynamically linked; expected a static musl binary" >&2
  exit 1
fi
reported=$("$bin" --version)
if [ "$reported" != "gavmo $version" ]; then
  echo "error: $bin --version printed '$reported', expected 'gavmo $version'" >&2
  exit 1
fi
if [ ! -f "$site/pkg/gavmo.wasm" ]; then
  echo "error: $site/pkg/gavmo.wasm is missing; the site build failed?" >&2
  exit 1
fi

name="gavmo-$version-x86_64-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
install -Dm755 "$bin" "$stage/$name/bin/gavmo"
mkdir -p "$stage/$name/share/gavmo"
cp -r "$site" "$stage/$name/share/gavmo/site"
install -Dm644 LICENSE "$stage/$name/share/doc/gavmo/LICENSE"
install -Dm644 README.md "$stage/$name/share/doc/gavmo/README.md"

# Reproducible archive: fixed order, owners and timestamps (the commit's).
mkdir -p "$out_dir"
mtime=$(git log -1 --format=%ct 2>/dev/null || echo 0)
tarball="$out_dir/$name.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" \
  -C "$stage" -cf - "$name" | gzip -n -9 > "$tarball"

echo "built $tarball ($(du -h "$tarball" | cut -f1))"
