/** @internal Diagnostic events omit request headers, bodies and TLS secrets. */
export function debugHttp3(event: string, fields: Record<string, unknown>): void {
  if (process.env.HTTP3_DEBUG === '1') console.error('[http3] ' + JSON.stringify({ event, ...fields }));
}

/** A close code describes the transport observation, not why a browser chose H2. */
export function debugHandshakeClose(connection: number, errorCode: number): void {
  const tlsAlert = errorCode >= 0x100 && errorCode <= 0x1ff ? errorCode - 0x100 : undefined;
  debugHttp3('handshake-closed', { connection, errorCode, tlsAlert,
    detail: 'QUIC connection closed before handshake completion. The server cannot determine the browser fallback decision.',
    hint: 'For locally trusted localhost certificates, Chromium may need --origin-to-force-quic-on=localhost:1. Use http3 doctor and chrome://net-export to distinguish certificate, address-family and cached alternative-service failures.',
  });
}
