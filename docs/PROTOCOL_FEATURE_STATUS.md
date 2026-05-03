# Protocol Feature Status

This document records protocol features that are supported, validated,
experimental, or intentionally deferred. It is the source of truth when deciding
whether a missing test is a coverage gap or a deliberate non-goal.

## Status Key

| Status | Meaning |
| --- | --- |
| Supported | Public API exists and default CI or release-blocking tests cover the behavior. |
| Diagnostic | Code exists, but validation is manual, platform-sensitive, or release-runbook driven. |
| Experimental | API or configuration exists, but compatibility and production behavior are still being hardened. |
| Deferred | Not a supported behavior for the current release line. |

## Validated Features

| Feature | Status | Validation |
| --- | --- | --- |
| HTTP/3 request/response streams | Supported | TypeScript core/interop suites, Rust mock-pair and quiche interop tests, curl release lane. |
| HTTP/2 parity adapter | Supported | TypeScript core/parity coverage and browser smoke. |
| Raw QUIC bidirectional streams | Supported | TypeScript raw QUIC interop suites and Rust mock-pair tests. |
| Raw QUIC DATAGRAM frames | Supported | TypeScript core/interop datagram tests and Rust datagram pair tests. |
| Session resumption tickets | Supported | TypeScript interop coverage verifies ticket emission and resumed connections. |
| QUIC-LB plaintext connection IDs | Supported | Configuration and connection-map unit tests cover server ID parsing and CID routing. |
| qlog and keylog emission | Diagnostic | Exercised by runbooks and failure artifact workflows rather than default CI. |
| Linux `io_uring` runtime | Diagnostic | Covered by the privileged Docker runtime lane; environment support depends on kernel and seccomp policy. |

## Experimental Or Deferred Features

| Feature | Status | Current behavior |
| --- | --- | --- |
| 0-RTT data transfer | Experimental | Client/server configuration and safe-method guards exist. The current automated coverage proves API restrictions and resumption setup, not end-to-end early-data acceptance under replay-sensitive production conditions. Treat 0-RTT request data as opt-in until a dedicated replay/acceptance matrix exists. |
| NAT rebinding | Deferred | No dedicated test simulates a peer address change after handshake. The transport should not be advertised as NAT-rebinding validated. |
| Active connection migration | Deferred | `disableActiveMigration` is configurable, but the default is `true`, and CI does not validate path migration, preferred addresses, or mobile network handoff behavior. |
| QUIC version negotiation | Deferred | The current lanes exercise the configured quiche default version. Deliberate version-negotiation probes and downgrade/interoperability cases are not covered. |
| Connection migration across load-balanced origins | Deferred | QUIC-LB CID routing and shared session ticket keys are supported building blocks, but an established connection moving between backend origins is not validated. |
| WebTransport API parity | Deferred | Listed as a v1 non-goal in the support matrix. |
| HTTP/1 default serving path | Deferred | Disabled unless explicitly enabled; not part of the primary support target. |

## Adding Support

Promote a deferred or experimental feature only when the implementation,
configuration docs, and tests all move together. At minimum, add:

- a public API or explicit non-API contract
- Rust-level protocol coverage when JavaScript cannot observe the invariant
- TypeScript integration coverage for the public behavior
- release-runbook evidence for platform-sensitive cases
