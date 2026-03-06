# curl / External Client Interop

## Status: verified

The unified server entrypoint now supports both:

- HTTP/3 over UDP (`curl --http3-only`)
- HTTP/2 over TLS/TCP (`curl --http2`)

Both protocols run on the same host+port in a single server lifecycle.

## Verified configurations

| Path | Retry | curl mode | Result |
|------|-------|-----------|--------|
| H3 worker transport | disabled | `--http3-only` | OK |
| H3 worker transport | enabled | `--http3-only` | OK |
| H2 unified listener | n/a | `--http2` | OK |
| SSE endpoint | n/a | `--http3-only --no-buffer` | OK |
| SSE endpoint | n/a | `--http2 --no-buffer` | OK |

## Notes

- The no-retry QUIC acceptance path continues using `odcid = None`, which preserves curl/ngtcp2 compatibility for HTTP/3 handshakes.
- SSE interoperability is standards-based (`text/event-stream`) and does not require protocol extensions.

## Environment reference

- **quiche**: 0.24.9 (Cloudflare, BoringSSL)
- **curl**: 8.18.0 with ngtcp2/nghttp3 and OpenSSL
- **Node**: 24.x
