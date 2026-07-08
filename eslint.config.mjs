import eslint from '@eslint/js';
import tseslint from 'typescript-eslint';

// docs/WASM_CLIENT_PLAN.md §6.6: lib/wasm/** must stay host-agnostic (Node
// today, workerd/browsers later) and must never pull in the native binding
// loader or the file-tailing keylog module. Two files are narrowly exempted
// for the one Node-specific thing each legitimately needs — see their own
// doc comments (lib/wasm/core-loader.ts, lib/wasm/node-udp-adapter.ts).
const wasmZoneRestrictedImports = [
  { name: '../event-loop.js', message: 'lib/wasm/** must not import the native binding loader/event-loop types (docs/WASM_CLIENT_PLAN.md §6.6). A wasm event loop is structurally, not nominally, typed against ClientEventLoopLike/QuicClientEventLoopLike.' },
  { name: '../keylog.js', message: 'lib/wasm/** must not import the file-tailing keylog module — the wasm runtime delivers keylog via take_keylog + events instead (§6.4, N5).' },
  { name: 'node:http2', message: 'lib/wasm/** must stay free of node:http2 — relevant only to the native H2/H3 fallback path.' },
  { name: 'node:dns', message: 'lib/wasm/** must not resolve hostnames itself — connect() already resolves the endpoint before constructing the wasm event loop.' },
  { name: 'node:fs', message: 'lib/wasm/** must stay host-agnostic. The sole exception is core-loader.ts\'s own file-read (see its override below).' },
  { name: 'node:dgram', message: 'lib/wasm/** must stay host-agnostic. The sole exception is node-udp-adapter.ts (see its override below).' },
];

export default tseslint.config(
  eslint.configs.recommended,
  ...tseslint.configs.strictTypeChecked,
  {
    languageOptions: {
      parserOptions: {
        project: ['./tsconfig.json', './tsconfig.test.json', './tsconfig.workerd.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      // Floating promises are critical — must always await or handle
      '@typescript-eslint/no-floating-promises': 'error',
      '@typescript-eslint/no-misused-promises': 'error',

      // Typed EventEmitter APIs rely on declaration merging and overloads.
      '@typescript-eslint/no-unsafe-declaration-merging': 'off',
      '@typescript-eslint/unified-signatures': 'off',

      // Enforce async/await over .then/.catch
      '@typescript-eslint/promise-function-async': 'error',

      // Require explicit return types for public API clarity
      '@typescript-eslint/explicit-function-return-type': ['error', {
        allowExpressions: true,
        allowTypedFunctionExpressions: true,
      }],

      // Allow void expressions for fire-and-forget (but only with void operator)
      '@typescript-eslint/no-confusing-void-expression': ['error', {
        ignoreArrowShorthand: true,
      }],

      // Relax some strict rules that conflict with napi interop or stub code
      '@typescript-eslint/no-extraneous-class': 'off',      // EventEmitter subclasses
      '@typescript-eslint/no-explicit-any': ['error', {
        ignoreRestArgs: true,
      }],
      '@typescript-eslint/no-unused-vars': ['error', {
        argsIgnorePattern: '^_',
        varsIgnorePattern: '^_',
      }],
      '@typescript-eslint/restrict-template-expressions': ['error', {
        allowNumber: true,
      }],

      // Allow `this: void` on interface method signatures (the blessed
      // fix `@typescript-eslint/unbound-method`'s own message suggests) —
      // used by lib/wasm/core-loader.ts's Http3WasmExports, whose members
      // are raw wasm exports passed around as bare function references.
      '@typescript-eslint/no-invalid-void-type': ['error', {
        allowAsThisParameter: true,
      }],
    },
  },
  {
    // Test files get relaxed rules. FFI tests legitimately drive native
    // bindings with any-typed shims, hold imports for incremental work, and
    // leave style nits (let/const) to be caught by reviewers, not lint.
    files: ['test/**/*.ts'],
    rules: {
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
      '@typescript-eslint/no-explicit-any': 'off',
      '@typescript-eslint/no-unused-vars': 'off',
      '@typescript-eslint/no-require-imports': 'off',
      // node:test describe/it/before handle async callbacks internally
      '@typescript-eslint/no-floating-promises': 'off',
      '@typescript-eslint/promise-function-async': 'off',
      '@typescript-eslint/require-await': 'off',
      '@typescript-eslint/no-misused-promises': 'off',
      '@typescript-eslint/no-non-null-assertion': 'off',
      '@typescript-eslint/no-unnecessary-condition': 'off',
      '@typescript-eslint/no-unnecessary-type-assertion': 'off',
      '@typescript-eslint/explicit-function-return-type': 'off',
      '@typescript-eslint/restrict-plus-operands': 'off',
      '@typescript-eslint/restrict-template-expressions': 'off',
      '@typescript-eslint/prefer-promise-reject-errors': 'off',
      '@typescript-eslint/use-unknown-in-catch-callback-variable': 'off',
      'prefer-const': 'off',
      'no-useless-assignment': 'off',
    },
  },
  {
    // docs/WASM_CLIENT_PLAN.md §6.6 — see wasmZoneRestrictedImports above.
    files: ['lib/wasm/**/*.ts'],
    rules: {
      'no-restricted-imports': ['error', { paths: wasmZoneRestrictedImports }],
    },
  },
  {
    // Sole exception: this file's own `readFileSync` for the Node artifact-load path.
    // (Phase 5: this used to be core-loader.ts itself; that file is now
    // 100% host-agnostic — the node:fs-touching convenience wrapper lives
    // in this separate file instead, precisely so core-loader.ts and
    // everything that imports it can compile under tsconfig.workerd.json.
    // See node-core-loader.ts's doc comment.)
    files: ['lib/wasm/node-core-loader.ts'],
    rules: {
      'no-restricted-imports': ['error', {
        paths: wasmZoneRestrictedImports.filter((p) => p.name !== 'node:fs'),
      }],
    },
  },
  {
    // Sole exception: this file's own `node:dgram` transport implementation.
    files: ['lib/wasm/node-udp-adapter.ts'],
    rules: {
      'no-restricted-imports': ['error', {
        paths: wasmZoneRestrictedImports.filter((p) => p.name !== 'node:dgram'),
      }],
    },
  },
  {
    ignores: [
      'dist/',
      'dist-test/',
      'index.js',
      'index.d.ts',
      '*.node',
      // Debug repros are .mjs scratch files outside the typed project.
      'test/debug/**',
    ],
  },
);
