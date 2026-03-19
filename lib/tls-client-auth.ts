import { ERR_HTTP3_TLS_CONFIG_ERROR, Http3Error } from './errors.js';

export type ClientAuthMode = 'none' | 'request' | 'require';

type ServerCaOption = string | Buffer | Array<string | Buffer> | undefined;

function hasServerCa(ca: ServerCaOption): boolean {
  if (typeof ca === 'undefined') return false;
  if (Array.isArray(ca)) return ca.length > 0;
  return true;
}

export function resolveServerClientAuthMode(options: {
  ca?: ServerCaOption;
  clientAuth?: ClientAuthMode;
}): ClientAuthMode {
  const explicitMode = options.clientAuth;
  const caPresent = hasServerCa(options.ca);

  if (typeof explicitMode === 'undefined') {
    return caPresent ? 'require' : 'none';
  }

  switch (explicitMode) {
    case 'none':
      if (caPresent) {
        throw new Http3Error(
          'invalid TLS server options: `clientAuth: "none"` cannot be combined with `ca`',
          ERR_HTTP3_TLS_CONFIG_ERROR,
        );
      }
      return 'none';
    case 'request':
    case 'require':
      if (!caPresent) {
        throw new Http3Error(
          `invalid TLS server options: \`clientAuth: "${explicitMode}"\` requires \`ca\``,
          ERR_HTTP3_TLS_CONFIG_ERROR,
        );
      }
      return explicitMode;
    default:
      throw new Http3Error(
        `invalid TLS server options: unknown clientAuth mode "${String(explicitMode)}"`,
        ERR_HTTP3_TLS_CONFIG_ERROR,
      );
  }
}
