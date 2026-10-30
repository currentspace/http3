# Local HTTPS and HTTP/3

The `http3 dev-cert` CLI and listener defaults below are unreleased changes after
0.9.6. Use a built source checkout or a package containing these changes.

## Generate and trust a localhost certificate

With the package installed, run:

```sh
npx http3 dev-cert --out .certs --trust
```

From a built source checkout, replace `npx http3` with `node bin/http3.cjs`.
Without `--trust`, generation only writes local files. Trust can also be installed
or removed separately:

```sh
npx http3 dev-cert trust --out .certs
npx http3 dev-cert untrust --out .certs
```

OpenSSL must be installed. macOS trust uses `sudo security` and the System
keychain. Linux uses `sudo update-ca-certificates` or `sudo update-ca-trust`, plus
NSS `certutil` (`libnss3-tools` on Debian/Ubuntu, `nss-tools` on Fedora) for Chrome.
Windows uses the current user's Root store via `certutil.exe`; that command path
has unit coverage but no Windows browser qualification. Native Windows binaries
are not published.

The generated CA expires after 365 days, has a critical name constraint for
`localhost`, its subdomains, IPv4 loopback (127/8), and IPv6 loopback (::1), and
cannot issue intermediate CAs. Leaves last at most 30 days and cover the DNS hostname `localhost`.
They work over both IPv4 and IPv6, but do not cover IP-literal URLs. BoringSSL's
legacy verifier rejects IP SANs under IP name constraints; the CLI keeps the CA
constraints intact and issues a DNS-only leaf. Native/WASM clients connecting
to an IP should set `servername: 'localhost'` alongside the explicit `ca`. The CLI reuses matching unexpired keys and certificates.
To renew an expiring leaf, remove only `key.pem` and `cert.pem` and run generation
again. To replace the CA, untrust it first and use a new directory. Keep the CA
files until untrust succeeds; they identify the exact certificate to remove.

Keep `.certs` out of version control. Its CA private key can impersonate local
services on machines that trust it. The CLI restricts directory/key permissions
on Unix; it does not distribute the CA or change browser launch settings.

## Configure the server and Node clients

For the examples that accept file paths:

```sh
export TLS_KEY_FILE="$PWD/.certs/key.pem"
export TLS_CERT_FILE="$PWD/.certs/cert.pem"
export TLS_CA_FILE="$PWD/.certs/ca.pem"
export NODE_EXTRA_CA_CERTS="$PWD/.certs/ca.pem"
npm run dev
```

`NODE_EXTRA_CA_CERTS` must be set **before Node starts**. It supplies this CA to
Node TLS clients, including ordinary HTTPS/fetch. It does not turn fetch into
HTTP/3, and it does not configure this library's native or WASM BoringSSL engine.
Pass `ca: readFileSync(caPath)` to `connectAsync`/`connectQuicAsync` explicitly.
`TLS_CA_FILE` does that for the combined Next/Nest example's internal H3 client.
Neither `rejectUnauthorized: false` nor `NODE_TLS_REJECT_UNAUTHORIZED=0` is needed.

## Chrome and Alt-Svc

Restart Chrome after installing trust, using a separate development profile:

```sh
# macOS; use the chrome/google-chrome executable on Linux.
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --user-data-dir="$TMPDIR/http3-dev-chrome" \
  --origin-to-force-quic-on=localhost:1 \
  https://localhost:3443
```

Chromium's local-root QUIC exception needs this flag even when the CA is trusted.
Port **1 is unused**; the application port still comes from the server's `Alt-Svc`
advertisement. The first visit may use H2. Reload and check DevTools' Network
Protocol column for `h3`. This is a local development launch configuration, not
something an application can enable for its users. See Chromium's
[QUIC testing guidance](https://www.chromium.org/quic/playing-with-quic/). The
[session pool](https://github.com/chromium/chromium/blob/main/net/quic/quic_session_pool.cc)
extracts hostnames from forced origins for its local-root allowlist, while
[forced protocol selection](https://github.com/chromium/chromium/blob/main/net/http/http_network_session.cc)
still matches the specific origin and port.

The example browser scripts pin their generated leaf certificate in an isolated
Chromium process. They do not install OS trust. The separate Distribution CI test
installs/removes the CLI's CA on disposable Linux/macOS runners, checks a Node
HTTPS client using `NODE_EXTRA_CA_CERTS`, and checks H2-to-H3 negotiation without
SPKI bypass, ignored HTTPS errors, or a hostname resolver override.

## IPv4, IPv6 and listener scope

Both `createSecureServer()` and the Node facade accept these forms:

```js
server.listen(3443, '::', onListening);
server.listen({ port: 3443, host: '::' }, onListening);
server.listen(3443, onListening);
```

Omitting the host binds `::` with IPv4 also enabled. **This listens on all
interfaces**, as do the updated Next/Nest examples; `HTTP3_HOST` overrides their
bind address. Restrict access with a firewall when using development credentials.
For a strictly local IPv4 listener, set `HTTP3_HOST=127.0.0.1`. Using an
IP-literal browser URL also requires a different certificate with that IP SAN;
the CLI certificate is for `localhost`. An explicit `::1` listener is IPv6 loopback
only. IPv6 must be available for the dual-stack default; use an explicit IPv4
host on an IPv4-only OS.

A `127.0.0.1`-only server cannot receive a browser's `localhost` QUIC attempt to
`::1`. TCP can still succeed over IPv4, which makes the failure look like H2
working while H3 does not. Keep both TCP and UDP reachable on the same port.
The stream server takes IP literals; the Node facade also resolves hostnames.
Unix sockets and string-valued ports are rejected before native startup.
