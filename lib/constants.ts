/**
 * HTTP/3 application error codes (RFC 9114 Section 8.1) and QUIC transport
 * error codes (RFC 9000 Section 20.1).
 *
 * These constants match the wire values and can be passed to
 * `stream.close(code)` or `session.close(code)`.
 * @module
 */

// HTTP/3 error codes (RFC 9114 §8.1)
export const H3_NO_ERROR = 0x0100;
export const H3_GENERAL_PROTOCOL_ERROR = 0x0101;
export const H3_INTERNAL_ERROR = 0x0102;
export const H3_STREAM_CREATION_ERROR = 0x0103;
export const H3_CLOSED_CRITICAL_STREAM = 0x0104;
export const H3_FRAME_UNEXPECTED = 0x0105;
export const H3_FRAME_ERROR = 0x0106;
export const H3_EXCESSIVE_LOAD = 0x0107;
export const H3_ID_ERROR = 0x0108;
export const H3_SETTINGS_ERROR = 0x0109;
export const H3_MISSING_SETTINGS = 0x010a;
export const H3_REQUEST_REJECTED = 0x010b;
export const H3_REQUEST_CANCELLED = 0x010c;
export const H3_REQUEST_INCOMPLETE = 0x010d;
export const H3_MESSAGE_ERROR = 0x010e;
export const H3_CONNECT_ERROR = 0x010f;
export const H3_VERSION_FALLBACK = 0x0110;

// QUIC transport error codes (RFC 9000 §20.1)
export const QUIC_NO_ERROR = 0x00;
export const QUIC_INTERNAL_ERROR = 0x01;
export const QUIC_CONNECTION_REFUSED = 0x02;
export const QUIC_FLOW_CONTROL_ERROR = 0x03;
export const QUIC_STREAM_LIMIT_ERROR = 0x04;
export const QUIC_STREAM_STATE_ERROR = 0x05;
export const QUIC_FINAL_SIZE_ERROR = 0x06;
export const QUIC_FRAME_ENCODING_ERROR = 0x07;
export const QUIC_TRANSPORT_PARAMETER_ERROR = 0x08;
export const QUIC_CONNECTION_ID_LIMIT_ERROR = 0x09;
export const QUIC_PROTOCOL_VIOLATION = 0x0a;
export const QUIC_INVALID_TOKEN = 0x0b;
export const QUIC_APPLICATION_ERROR = 0x0c;
export const QUIC_CRYPTO_BUFFER_EXCEEDED = 0x0d;
export const QUIC_KEY_UPDATE_ERROR = 0x0e;
export const QUIC_AEAD_LIMIT_REACHED = 0x0f;
export const QUIC_NO_VIABLE_PATH = 0x10;
export const QUIC_CRYPTO_ERROR = 0x0100; // Range: 0x0100-0x01ff
