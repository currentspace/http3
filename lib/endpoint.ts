import { lookup } from 'node:dns/promises';
import { isIP } from 'node:net';
import { Http3Error, ERR_HTTP3_ENDPOINT_INVALID, ERR_HTTP3_ENDPOINT_RESOLUTION } from './errors.js';

export interface HostEndpoint {
  host: string;
  port: number;
  servername?: string;
}

export interface AddressEndpoint {
  address: string;
  port: number;
  servername?: string;
}

export type ConnectionEndpoint = string | HostEndpoint | AddressEndpoint;

export interface ParsedConnectionEndpoint {
  host: string;
  port: number;
  servername: string;
  authority: string;
}

export interface ResolvedConnectionEndpoint extends ParsedConnectionEndpoint {
  address: string;
  family: 4 | 6;
  socketAddress: string;
}

function assertPort(port: number, input: ConnectionEndpoint): void {
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Http3Error(
      `invalid endpoint port: ${String(port)}`,
      ERR_HTTP3_ENDPOINT_INVALID,
      { endpoint: stringifyConnectionEndpoint(input) },
    );
  }
}

function defaultServername(hostOrAddress: string, override?: string): string {
  return override ?? hostOrAddress;
}

export function formatSocketAddress(address: string, port: number): string {
  return isIP(address) === 6 ? `[${address}]:${String(port)}` : `${address}:${String(port)}`;
}

export function stringifyConnectionEndpoint(endpoint: ConnectionEndpoint): string {
  if (typeof endpoint === 'string') {
    return endpoint;
  }

  if ('host' in endpoint) {
    return isIP(endpoint.host) === 6
      ? formatSocketAddress(endpoint.host, endpoint.port)
      : `${endpoint.host}:${String(endpoint.port)}`;
  }

  return formatSocketAddress(endpoint.address, endpoint.port);
}

export function parseConnectionEndpoint(
  endpoint: ConnectionEndpoint,
  options: { defaultScheme: string; defaultPort: number },
): ParsedConnectionEndpoint {
  if (typeof endpoint !== 'string') {
    if ('host' in endpoint) {
      assertPort(endpoint.port, endpoint);
      if (!endpoint.host) {
        throw new Http3Error(
          'endpoint host must not be empty',
          ERR_HTTP3_ENDPOINT_INVALID,
          { endpoint: stringifyConnectionEndpoint(endpoint) },
        );
      }
      return {
        host: endpoint.host,
        port: endpoint.port,
        servername: defaultServername(endpoint.host, endpoint.servername),
        authority: stringifyConnectionEndpoint(endpoint),
      };
    }

    assertPort(endpoint.port, endpoint);
    if (!endpoint.address || isIP(endpoint.address) === 0) {
      throw new Http3Error(
        `endpoint address must be an IP literal, got ${endpoint.address}`,
        ERR_HTTP3_ENDPOINT_INVALID,
        { endpoint: stringifyConnectionEndpoint(endpoint) },
      );
    }
    return {
      host: endpoint.address,
      port: endpoint.port,
      servername: defaultServername(endpoint.address, endpoint.servername),
      authority: formatSocketAddress(endpoint.address, endpoint.port),
    };
  }

  try {
    const url = new URL(endpoint.includes('://') ? endpoint : `${options.defaultScheme}://${endpoint}`);
    const port = Number.parseInt(url.port || String(options.defaultPort), 10);
    assertPort(port, endpoint);
    if (!url.hostname) {
      throw new Http3Error(
        `endpoint is missing a hostname: ${endpoint}`,
        ERR_HTTP3_ENDPOINT_INVALID,
        { endpoint },
      );
    }
    return {
      host: url.hostname,
      port,
      servername: url.hostname,
      authority: endpoint,
    };
  } catch (err: unknown) {
    if (err instanceof Http3Error) {
      throw err;
    }
    throw new Http3Error(
      `invalid endpoint: ${endpoint}`,
      ERR_HTTP3_ENDPOINT_INVALID,
      {
        endpoint,
        cause: err instanceof Error ? err : undefined,
      },
    );
  }
}

export async function resolveConnectionEndpoint(
  endpoint: ConnectionEndpoint,
  options: { defaultScheme: string; defaultPort: number },
): Promise<ResolvedConnectionEndpoint> {
  const parsed = parseConnectionEndpoint(endpoint, options);
  const family = isIP(parsed.host);

  if (family === 4 || family === 6) {
    return {
      ...parsed,
      address: parsed.host,
      family,
      socketAddress: formatSocketAddress(parsed.host, parsed.port),
    };
  }

  try {
    const resolved = await lookup(parsed.host, { all: true, verbatim: true });
    const [firstResolved] = resolved;
    const selected = resolved.find((entry) => entry.family === 4) ?? firstResolved;
    return {
      ...parsed,
      address: selected.address,
      family: selected.family as 4 | 6,
      socketAddress: formatSocketAddress(selected.address, parsed.port),
    };
  } catch (err: unknown) {
    throw new Http3Error(
      `failed to resolve endpoint host ${parsed.host}`,
      ERR_HTTP3_ENDPOINT_RESOLUTION,
      {
        endpoint: parsed.authority,
        host: parsed.host,
        port: parsed.port,
        servername: parsed.servername,
        cause: err instanceof Error ? err : undefined,
      },
    );
  }
}
