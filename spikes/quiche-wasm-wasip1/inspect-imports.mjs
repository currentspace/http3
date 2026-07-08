import { readFileSync } from 'node:fs';

const path = process.argv[2];
const bytes = readFileSync(path);
const mod = new WebAssembly.Module(bytes);
const imports = WebAssembly.Module.imports(mod);
const exports = WebAssembly.Module.exports(mod);
console.log('=== IMPORTS (' + imports.length + ') ===');
for (const i of imports) console.log(`${i.kind}  ${i.module}::${i.name}`);
console.log('=== EXPORTS (' + exports.length + ') ===');
for (const e of exports) console.log(`${e.kind}  ${e.name}`);
