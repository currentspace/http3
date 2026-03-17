/**
 * Error subclass for HTTP/3 and QUIC protocol errors.
 *
 * Every instance carries a string {@link code} that identifies the error
 * category, and optionally a numeric {@link quicCode} or {@link h3Code}
 * taken from the QUIC/HTTP/3 wire protocol.
 */
export class Http3Error extends Error {
  /** Machine-readable error code, e.g. `ERR_HTTP3_STREAM_ERROR`. */
  readonly code: string;
  /** QUIC transport-level error code (RFC 9000 Section 20.1), if applicable. */
  readonly quicCode?: number;
  /** HTTP/3 application error code (RFC 9114 Section 8.1), if applicable. */
  readonly h3Code?: number;

  /**
   * @param message - Human-readable description.
   * @param code - One of the `ERR_HTTP3_*` string constants.
   * @param options - Optional QUIC/H3 numeric error codes.
   */
  constructor(message: string, code: string, options?: { quicCode?: number; h3Code?: number }) {
    super(message);
    this.name = 'Http3Error';
    this.code = code;
    this.quicCode = options?.quicCode;
    this.h3Code = options?.h3Code;
  }
}

/** A stream-level error occurred (reset, flow-control violation, etc.). */
export const ERR_HTTP3_STREAM_ERROR = 'ERR_HTTP3_STREAM_ERROR';
/** A session-level error occurred (connection close, transport error). */
export const ERR_HTTP3_SESSION_ERROR = 'ERR_HTTP3_SESSION_ERROR';
/** Response headers have already been sent on this stream. */
export const ERR_HTTP3_HEADERS_SENT = 'ERR_HTTP3_HEADERS_SENT';
/** Operation attempted in an invalid state (e.g. not connected, already closed). */
export const ERR_HTTP3_INVALID_STATE = 'ERR_HTTP3_INVALID_STATE';
/** The peer sent a GOAWAY frame. */
export const ERR_HTTP3_GOAWAY = 'ERR_HTTP3_GOAWAY';
/** TLS certificate or key configuration is invalid or missing. */
export const ERR_HTTP3_TLS_CONFIG_ERROR = 'ERR_HTTP3_TLS_CONFIG_ERROR';
/** An error occurred while setting up or writing the SSLKEYLOGFILE. */
export const ERR_HTTP3_KEYLOG_ERROR = 'ERR_HTTP3_KEYLOG_ERROR';
