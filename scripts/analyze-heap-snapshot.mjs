#!/usr/bin/env node
/**
 * Parse a V8 heap snapshot and report the top retained object types.
 * Usage: node scripts/analyze-heap-snapshot.mjs <snapshot.heapsnapshot>
 */

import { readFileSync } from 'node:fs';

const file = process.argv[2];
if (!file) {
  console.error('Usage: node analyze-heap-snapshot.mjs <file.heapsnapshot>');
  process.exit(1);
}

console.log(`Parsing ${file}...`);
const raw = readFileSync(file, 'utf8');
const snapshot = JSON.parse(raw);

const { nodes, edges, strings } = snapshot;
const meta = snapshot.snapshot.meta;
const nodeFields = meta.node_fields;
const edgeFields = meta.edge_fields;
const nodeFieldCount = nodeFields.length;
const edgeFieldCount = edgeFields.length;

// Node field indices
const NODE_TYPE = nodeFields.indexOf('type');
const NODE_NAME = nodeFields.indexOf('name');
const NODE_ID = nodeFields.indexOf('id');
const NODE_SELF_SIZE = nodeFields.indexOf('self_size');
const NODE_EDGE_COUNT = nodeFields.indexOf('edge_count');
const NODE_RETAINED_SIZE = nodeFields.indexOf('retained_size'); // may not exist

const nodeTypes = meta.node_types[0]; // array of type strings

const nodeCount = nodes.length / nodeFieldCount;
console.log(`  ${nodeCount.toLocaleString()} nodes, ${strings.length.toLocaleString()} strings`);

// Count by constructor name
const byConstructor = new Map();
// Count by type
const byType = new Map();

for (let i = 0; i < nodeCount; i++) {
  const base = i * nodeFieldCount;
  const typeIdx = nodes[base + NODE_TYPE];
  const nameIdx = nodes[base + NODE_NAME];
  const selfSize = nodes[base + NODE_SELF_SIZE];
  const typeName = nodeTypes[typeIdx];
  const name = strings[nameIdx];

  // By constructor
  const key = `${typeName}::${name}`;
  const prev = byConstructor.get(key) || { count: 0, totalSize: 0 };
  prev.count++;
  prev.totalSize += selfSize;
  byConstructor.set(key, prev);

  // By type
  const prev2 = byType.get(typeName) || { count: 0, totalSize: 0 };
  prev2.count++;
  prev2.totalSize += selfSize;
  byType.set(typeName, prev2);
}

console.log('\n=== Top 30 by count ===');
const sorted = [...byConstructor.entries()]
  .sort((a, b) => b[1].count - a[1].count)
  .slice(0, 30);

console.log('Count'.padStart(10), 'TotalMB'.padStart(10), 'Name');
for (const [name, { count, totalSize }] of sorted) {
  console.log(
    String(count).padStart(10),
    (totalSize / 1024 / 1024).toFixed(1).padStart(10),
    name,
  );
}

console.log('\n=== Top 30 by total size ===');
const sortedBySize = [...byConstructor.entries()]
  .sort((a, b) => b[1].totalSize - a[1].totalSize)
  .slice(0, 30);

console.log('Count'.padStart(10), 'TotalMB'.padStart(10), 'Name');
for (const [name, { count, totalSize }] of sortedBySize) {
  console.log(
    String(count).padStart(10),
    (totalSize / 1024 / 1024).toFixed(1).padStart(10),
    name,
  );
}

console.log('\n=== By node type ===');
const sortedTypes = [...byType.entries()]
  .sort((a, b) => b[1].totalSize - a[1].totalSize);

console.log('Count'.padStart(10), 'TotalMB'.padStart(10), 'Type');
for (const [name, { count, totalSize }] of sortedTypes) {
  console.log(
    String(count).padStart(10),
    (totalSize / 1024 / 1024).toFixed(1).padStart(10),
    name,
  );
}
