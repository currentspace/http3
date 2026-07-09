import { lookup } from 'node:dns/promises';
import { isIP } from 'node:net';
import { Http3Error, ERR_HTTP3_ENDPOINT_INVALID, ERR_HTTP3_ENDPOINT_RESOLUTION } from './errors.js';

/**
 * DNS resolver function shape, matching `node:dns/promises`'s
 * `lookup(hostname, { all: true })` overload. Injectable so a future host
 * adapter (e.g. a non-Node runtime) can supply a different hostname
 * resolution strategy without changing default Node behavior — when not
 * supplied, {@link resolveConnectionEndpoint} defaults to the real
 * `node:dns/promises` `lookup`. IP-literal endpoints never call this at all
 * (they bypass DNS entirely).
 */
export type DnsLookupFn = (
  hostname: string,
  options: { all: true; verbatim?: boolean },
) => Promise<Array<{ address: string; family: number }>>;

// Typed once here (rather than inline at the call site) so `?? lookup`
// resolves against a single non-overloaded signature — combining the raw,
// overloaded `node:dns/promises` `lookup` with `DnsLookupFn` via `??`
// directly at the call site produces a union across overloads that
// TypeScript cannot re-collapse to `DnsLookupFn`'s single return shape.
const defaultDnsLookup: DnsLookupFn = lookup;

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

export function abortSignalError(signal: AbortSignal | undefined): Error {
  const reason: unknown = signal?.reason;
  if (reason instanceof Error && reason.name === 'AbortError') {
    return reason;
  }

  const error = new Error(reason instanceof Error
    ? reason.message
    : typeof reason === 'string' ? reason : 'The operation was aborted');
  error.name = 'AbortError';
  if (reason instanceof Error) {
    (error as Error & { cause?: unknown }).cause = reason;
  }
  return error;
}

/** Throws synchronously if the signal is already aborted. */
function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw abortSignalError(signal);
  }
}

/** Race a promise against the abort signal so a long-running lookup can
 *  be cut short. Audit finding #15. */
async function raceAbort<T>(promise: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
  if (!signal) return promise;
  throwIfAborted(signal);
  let rejectAbort: ((error: Error) => void) | undefined;
  const handler = (): void => {
    rejectAbort?.(abortSignalError(signal));
  };
  const aborted = new Promise<never>((_, reject) => {
    rejectAbort = reject;
    signal.addEventListener('abort', handler, { once: true });
  });
  try {
    return await Promise.race([promise, aborted]);
  } finally {
    signal.removeEventListener('abort', handler);
  }
}

export async function resolveConnectionEndpoint(
  endpoint: ConnectionEndpoint,
  options: {
    defaultScheme: string;
    defaultPort: number;
    signal?: AbortSignal;
    /** Override DNS resolution. Defaults to `node:dns/promises`'s `lookup`. */
    dnsLookup?: DnsLookupFn;
  },
): Promise<ResolvedConnectionEndpoint> {
  throwIfAborted(options.signal);
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

  const resolveHost = options.dnsLookup ?? defaultDnsLookup;
  try {
    const resolved = await raceAbort(
      resolveHost(parsed.host, { all: true, verbatim: true }),
      options.signal,
    );
    throwIfAborted(options.signal);
    const [firstResolved] = resolved;
    const selected = resolved.find((entry) => entry.family === 4) ?? firstResolved;
    return {
      ...parsed,
      address: selected.address,
      family: selected.family as 4 | 6,
      socketAddress: formatSocketAddress(selected.address, parsed.port),
    };
  } catch (err: unknown) {
    // Pass through abort errors verbatim — caller distinguishes by name.
    if (err instanceof Error && (err.name === 'AbortError' || options.signal?.aborted)) {
      throw err;
    }
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
