#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# Select the host sidecar from manifests, including libc. For release musl builds
# use Dockerfile.musl so BoringSSL and the addon share a pinned Alpine sysroot.
TARGET=$(node --input-type=module -e '
import { hostPlatform } from "./scripts/native-packages.mjs";
const { platform, arch, libc } = hostPlatform();
const cpu = {x64: "x86_64", arm64: "aarch64"}[arch];
if (!cpu || !["linux", "darwin"].includes(platform)) throw Error(`Unsupported native host: ${platform}/${arch}`);
console.log(`${cpu}-${platform === "darwin" ? "apple-darwin" : `unknown-linux-${libc === "musl" ? "musl" : "gnu"}`}`);
')
if [[ "$TARGET" == *-musl ]]; then
  # Omit --target: bindgen's host build script also needs dynamic musl/dlopen.
  cargo --config 'build.rustflags=["-C", "target-feature=-crt-static"]' build --release --locked --lib
  node --input-type=module -e '
import { copyFileSync } from "node:fs";
import { nativePackages, selectNativePackage } from "./scripts/native-packages.mjs";
copyFileSync("target/release/libhttp3.so", selectNativePackage(nativePackages()).pkg.main);
'
else
  pnpm exec napi build --release --platform --target "$TARGET"
fi
if [[ "$TARGET" == *-apple-darwin ]]; then
  if [[ "$TARGET" == aarch64-* ]]; then
    OTHER_TARGET=x86_64-apple-darwin
  else
    OTHER_TARGET=aarch64-apple-darwin
  fi
  if rustup target list --installed | grep -qx "$OTHER_TARGET"; then
    pnpm exec napi build --release --platform --target "$OTHER_TARGET"
  fi
fi
pnpm run patch:native-loader
pnpm run build:dist
node --input-type=module -e '
import { copyFileSync, existsSync } from "node:fs";
import { nativePackages } from "./scripts/native-packages.mjs";
for (const { dir, pkg } of nativePackages()) {
  if (existsSync(pkg.main)) {
    copyFileSync(pkg.main, `npm/${dir}/${pkg.main}`);
    console.log(`Prepared npm/${dir}/${pkg.main}`);
  }
}
'
