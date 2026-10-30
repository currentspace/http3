/** Shared IP listener arguments for the stream server and Node facade. */
export interface ListenOptions {
  port?: number;
  host?: string;
  path?: never;
}

export function parseListenArgs(
  portOrOptions: unknown,
  hostOrCallback?: string | (() => void),
  callback?: () => void,
): { port: number; host: string; callback?: () => void } {
  if (typeof portOrOptions !== 'number' &&
      (typeof portOrOptions !== 'object' || portOrOptions === null || Array.isArray(portOrOptions))) {
    throw new TypeError('listen() expects a port number or { port, host }');
  }
  const options = typeof portOrOptions === 'number' ? { port: portOrOptions } : portOrOptions as ListenOptions;
  if (hostOrCallback !== undefined && typeof hostOrCallback !== 'string' && typeof hostOrCallback !== 'function') {
    throw new TypeError('listen() expects a host string or callback as its second argument');
  }
  if ('path' in options) throw new TypeError('HTTP/3 requires an IP port, not a Unix socket');
  if (typeof portOrOptions === 'object' && typeof hostOrCallback === 'string') {
    throw new TypeError('Use listen({ port, host }, callback) or listen(port, host, callback)');
  }
  const host = options.host ?? (typeof hostOrCallback === 'string' ? hostOrCallback : '::');
  if (typeof host !== 'string' || !host) throw new TypeError('listen() host must be a nonempty string');
  const onListening = typeof hostOrCallback === 'function' ? hostOrCallback : callback;
  if (onListening !== undefined && typeof onListening !== 'function') throw new TypeError('listen() callback must be a function');
  return { port: options.port ?? 0, host, callback: onListening };
}

export function validateListenPort(port: number): void {
  if (typeof port !== 'number') throw new TypeError('listen() port must be a number');
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new RangeError('port must be an integer between 0 and 65535');
}
