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
# target), a C compiler for the musl target (for ring: `musl-gcc` from
# Ubuntu's `musl-tools`, or one named by CC_x86_64_unknown_linux_musl, as
# scripts/ci-install-musl-cc.sh sets up), cargo-leptos, a wasm-bindgen CLI matching Cargo.lock, binaryen's
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

# The site (wasm, JS, CSS) from cargo-leptos as usual.
cargo leptos build --release --cargo-locked --frontend-only

# The server for the musl target, which links a fully static binary.
# cargo-leptos can't do this half: it only looks for the server binary in
# target/release/ (unless `bin-target-triple` is set in Cargo.toml, which
# would apply to dev builds too). So this is the `cargo build` it would run,
# with the environment it would set; the values are crates/gavmo's
# [package.metadata.leptos]. Keep them in step with it. (The "dropping
# unsupported crate type `cdylib`" warning is expected: the lib is also
# built for wasm, where it is a cdylib.)
LEPTOS_OUTPUT_NAME=gavmo \
  LEPTOS_SITE_ROOT=target/site \
  LEPTOS_SITE_PKG_DIR=pkg \
  LEPTOS_SITE_ADDR=127.0.0.1:3000 \
  LEPTOS_RELOAD_PORT=3001 \
  LEPTOS_LIB_DIR=crates/gavmo \
  LEPTOS_BIN_DIR=crates/gavmo \
  LEPTOS_HASH_FILES=false \
  LEPTOS_JS_MINIFY=true \
  cargo build --locked --release --target="$triple" \
  --package=gavmo --bin=gavmo --no-default-features --features=ssr

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
