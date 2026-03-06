# Cloudflare-Restricted Origin Mode

This mode is for deployments where direct origin access should be blocked and
only Cloudflare should reach your service.

## Control Layers

1. Cloudflare Authenticated Origin Pull (mTLS)
2. Cloudflare IP range allowlist at SG/NACL
3. Optional app-level verification of client cert metadata

## mTLS Setup

- Configure origin server trust to require Cloudflare client certificates.
- Reject requests without valid client cert chain.
- Keep fallback/public mode disabled in locked environments.

## IP Allowlisting

- Allow only Cloudflare IPv4/IPv6 ranges on TCP/443 and UDP/443.
- Automate SG updates from Cloudflare published IP lists.
- Alert on drift between configured and expected ranges.

## Operational Modes

- **Public mode:** direct origin access allowed.
- **Locked mode:** only Cloudflare reaches origin.

Switch with environment-specific infrastructure config, not runtime code paths.

## Verification

- External non-Cloudflare source should fail at network or TLS layer.
- Cloudflare proxied H2/H3 traffic should succeed.
- Health checks should be sourced from allowed ranges or internal path.
