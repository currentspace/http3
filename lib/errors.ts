export class Http3Error extends Error {
  readonly code: string;
  readonly quicCode?: number;
  readonly h3Code?: number;

  constructor(message: string, code: string, options?: { quicCode?: number; h3Code?: number }) {
    super(message);
    this.name = 'Http3Error';
    this.code = code;
    this.quicCode = options?.quicCode;
    this.h3Code = options?.h3Code;
  }
}

export const ERR_HTTP3_STREAM_ERROR = 'ERR_HTTP3_STREAM_ERROR';
export const ERR_HTTP3_SESSION_ERROR = 'ERR_HTTP3_SESSION_ERROR';
export const ERR_HTTP3_HEADERS_SENT = 'ERR_HTTP3_HEADERS_SENT';
export const ERR_HTTP3_INVALID_STATE = 'ERR_HTTP3_INVALID_STATE';
export const ERR_HTTP3_GOAWAY = 'ERR_HTTP3_GOAWAY';
export const ERR_HTTP3_TLS_CONFIG_ERROR = 'ERR_HTTP3_TLS_CONFIG_ERROR';
export const ERR_HTTP3_KEYLOG_ERROR = 'ERR_HTTP3_KEYLOG_ERROR';
