import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const images = JSON.parse(readFileSync(new URL('../containers/node-images.json', import.meta.url), 'utf8'));

export function resolveNodeImage(version = process.env.NODE_MAJOR ?? '24') {
  const major = version.split('.')[0];
  const image = images[major];
  const exact = image?.match(/^node:([\d.]+)-/)?.[1];
  if (!image || (version !== major && version !== exact)) {
    throw new Error(`No pinned container image for Node ${version}; update containers/node-images.json`);
  }
  return image;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  console.log(resolveNodeImage(process.argv[2]));
}
