import path from 'node:path';
export default {
  turbopack: { root: path.resolve(import.meta.dirname, '..') },
  serverExternalPackages: ['@currentspace/http3'],
};
