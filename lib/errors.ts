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
  /** Requested runtime mode when the error was raised. */
  readonly requestedMode?: import('./runtime.js').RuntimeMode;
  /** Selected runtime mode, if any, when the error was raised. */
  readonly selectedMode?: import('./runtime.js').SelectedRuntimeMode;
  /** Native runtime driver associated with the error. */
  readonly driver?: import('./runtime.js').RuntimeDriver;
  /** Structured reason code for endpoint/runtime failures. */
  readonly reasonCode?: string;
  /** Native errno when available. */
  readonly errno?: number;
  /** Native syscall or operation name when available. */
  readonly syscall?: string;
  /** Endpoint string associated with the error when available. */
  readonly endpoint?: string;
  /** Endpoint hostname associated with the error when available. */
  readonly host?: string;
  /** Endpoint port associated with the error when available. */
  readonly port?: number;
  /** TLS servername/SNI associated with the error when available. */
  readonly servername?: string;
  /** Indicates that the runtime selection fell back from the fast path. */
  readonly fallbackOccurred?: boolean;
  /** Structured runtime snapshot associated with the error when available. */
  readonly runtimeInfo?: import('./runtime.js').RuntimeInfo;

  /**
   * @param message - Human-readable description.
   * @param code - One of the `ERR_HTTP3_*` string constants.
   * @param options - Optional QUIC/H3 numeric error codes.
   */
  constructor(
    message: string,
    code: string,
    options?: {
      quicCode?: number;
      h3Code?: number;
      requestedMode?: import('./runtime.js').RuntimeMode;
      selectedMode?: import('./runtime.js').SelectedRuntimeMode;
      driver?: import('./runtime.js').RuntimeDriver;
      reasonCode?: string;
      errno?: number;
      syscall?: string;
      endpoint?: string;
      host?: string;
      port?: number;
      servername?: string;
      fallbackOccurred?: boolean;
      runtimeInfo?: import('./runtime.js').RuntimeInfo;
      cause?: Error;
    },
  ) {
    super(message, options?.cause ? { cause: options.cause } : undefined);
    this.name = 'Http3Error';
    this.code = code;
    this.quicCode = options?.quicCode;
    this.h3Code = options?.h3Code;
    this.requestedMode = options?.requestedMode;
    this.selectedMode = options?.selectedMode;
    this.driver = options?.driver;
    this.reasonCode = options?.reasonCode;
    this.errno = options?.errno;
    this.syscall = options?.syscall;
    this.endpoint = options?.endpoint;
    this.host = options?.host;
    this.port = options?.port;
    this.servername = options?.servername;
    this.fallbackOccurred = options?.fallbackOccurred;
    this.runtimeInfo = options?.runtimeInfo;
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
/** A connection endpoint is malformed or unsupported. */
export const ERR_HTTP3_ENDPOINT_INVALID = 'ERR_HTTP3_ENDPOINT_INVALID';
/** A hostname endpoint could not be resolved to a socket address. */
export const ERR_HTTP3_ENDPOINT_RESOLUTION = 'ERR_HTTP3_ENDPOINT_RESOLUTION';
/** The requested fast runtime path is unavailable in the current environment. */
export const ERR_HTTP3_FAST_PATH_UNAVAILABLE = 'ERR_HTTP3_FAST_PATH_UNAVAILABLE';
/** The requested runtime configuration is unsupported in the current environment. */
export const ERR_HTTP3_RUNTIME_UNSUPPORTED = 'ERR_HTTP3_RUNTIME_UNSUPPORTED';
/** The runtime selector fell back from the fast path to a portable path. */
export const ERR_HTTP3_RUNTIME_FALLBACK = 'ERR_HTTP3_RUNTIME_FALLBACK';
/** Warning code emitted when runtime auto-selection falls back. */
export const WARN_HTTP3_RUNTIME_FALLBACK = 'WARN_HTTP3_RUNTIME_FALLBACK';
