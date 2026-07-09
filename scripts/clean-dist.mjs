#!/usr/bin/env node
// Cleans dist/ before a tsc build (invoked by `pnpm run build:dist`), while
// preserving dist/wasm/http3_client.wasm across the wipe.
//
// Why: that file is produced by `pnpm run build:wasm` (a separate cargo +
// wasm-opt pipeline, docs/WASM_CLIENT_PLAN.md D1a/A5) and copied into
// dist/wasm/. tsc has no way to regenerate it. A plain
// `rmSync('dist', { recursive: true })` — which is what this script replaces
// — silently deletes it whenever build:wasm ran before build:dist/build, and
// nothing puts it back (see docs/WASM_CLIENT_PLAN.md D3 decision log). tsc
// does not require an empty outDir; it only (re)writes the .js/.d.ts files it
// manages, so leaving one pre-existing binary file under dist/ during the
// clean is safe and every other file still gets a fully clean rebuild.
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';

const DIST_DIR = 'dist';
const WASM_ARTIFACT = join(DIST_DIR, 'wasm', 'http3_client.wasm');

let stashedArtifact = null;
if (existsSync(WASM_ARTIFACT)) {
  // Copy (not rename) so this is safe even if the OS temp dir lives on a
  // different filesystem/volume than the repo (cross-device rename fails).
  const stashDir = mkdtempSync(join(tmpdir(), 'http3-wasm-artifact-'));
  stashedArtifact = join(stashDir, 'http3_client.wasm');
  copyFileSync(WASM_ARTIFACT, stashedArtifact);
}

rmSync(DIST_DIR, { recursive: true, force: true });

if (stashedArtifact) {
  mkdirSync(dirname(WASM_ARTIFACT), { recursive: true });
  copyFileSync(stashedArtifact, WASM_ARTIFACT);
  rmSync(dirname(stashedArtifact), { recursive: true, force: true });
}
