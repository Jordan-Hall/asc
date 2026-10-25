# Spike P50: MCP OAuth from VS Code and Cursor against `asc serve` `/mcp`

Status: decided 2026-10-25. SPEC §24.4.8, §22.7.3. Depends on M13 (protected-resource metadata), M33 (issuers), P24.

## Question

Does the optional OAuth path (M33) work for a direct `McpHttpServerDefinition` to an asc server's `/mcp`, and does the pre-issued-token `inputs` fallback ship?

## What the server does (verified by `crates/asc-serve/tests/mcp_oauth.rs` and `mcp_authz.rs`)

- `GET /.well-known/oauth-protected-resource` and `/.well-known/oauth-protected-resource/mcp` are public (RFC 9728). `resource` is the canonical `/mcp` URL (the RFC 8707 audience). `authorization_servers` is empty unless `[mcp.oauth] issuer` is set. `scopes_supported` is `asc:read`, `asc:write`. `bearer_methods_supported` is `["header"]`.
- Unauthenticated `/mcp` gets 401 with `WWW-Authenticate: Basic realm=...` and `Bearer resource_metadata="<prm url>", scope="asc:read"`.
- An access token is accepted only if the signature verifies against the issuer's JWKS (RS256, ES256), `aud` names this `/mcp` URL, `exp`/`nbf` hold, and the subject maps to a principal. A token in the query string gets 400. The token is never forwarded.
- DCR is not built. Clients must use a pre-registered public client (PKCE, loopback or custom-scheme redirect) at the issuer, or a CIMD-capable issuer.

## Client matrix

| Step | VS Code (`McpHttpServerDefinition`) | Cursor (`mcp.json` `url` entry) |
|---|---|---|
| Reads the 401 `resource_metadata` and the PRM | Expected: yes (MCP authorization spec 2025-06 and later) | Expected: yes |
| Needs DCR | Falls back to a user-supplied client id when the issuer has no registration endpoint | Needs a static `auth` block (`CLIENT_ID`, optional `scopes`); redirect `cursor://anysphere.cursor-mcp/oauth/callback` |
| Sends RFC 8707 `resource` | Expected: yes | Not relied on; the issuer must default `aud` to the `/mcp` URL |
| Attaches the bearer to `/mcp` | Yes | Yes |

Not verified here: the rows marked "Expected" and the Cursor redirect need a run of the real clients against a staging `asc serve` with `[mcp.oauth]` set. A spike run in CI cannot start an editor. That run is the one open item below.

## Decision

1. The extension registers no HTTP definition and does no OAuth (`src/mcp.ts` offers stdio only: `asc mcp`, and `asc mcp --remote <url>`, which resolves its credential through the git credential helper). This is unchanged.
2. The optional OAuth path is documented as supported only for a server that sets `[mcp.oauth] issuer` to an issuer with a pre-registered public client. A deployment without it must not be expected to work with an OAuth-only client.
3. The pre-issued-token `inputs` fallback ships as documentation, not extension code. It is the default advice for a direct `/mcp` registration:

```json
{
  "inputs": [
    { "type": "promptString", "id": "asc-token", "description": "asc token (read scope)", "password": true }
  ],
  "servers": {
    "asc-server": {
      "type": "http",
      "url": "https://asc.example.org/mcp",
      "headers": { "Authorization": "Bearer ${input:asc-token}" }
    }
  }
}
```

   The token comes from `asc serve token create --repo <r> --read`. The URL is `https`. The token never enters the repository or the extension settings. The `asc-ide` token is never used here.
4. Never register both `asc mcp` and the server's `/mcp` for one repository (§22.2).

## Open item

Run VS Code, VS Code Insiders and Cursor against a staging server with one issuer and record the "Expected" rows. If a client does not discover the PRM or cannot use a pre-registered client, item 2 narrows to "use the `inputs` fallback" and nothing in the server changes.
