# 0001 gRPC-Web and Connect unary on the RPC listener

Status: accepted, 2026-10-24. Deviates from SPEC section 24 ("no `tonic-web`; the browser reaches asc through the Forgejo proxy P04"). SPEC.md and PLAN.md are unchanged; this record and `docs/WEB-UI.md` (section 8, "Browser RPC") carry the change.

## Context

The web UI (`web/`, `@asc/asc-web`) must work against a real `asc serve` on any host, with or without a forge in front. The SPEC route sent every browser call through the Forgejo proxy, which ties the UI to Forgejo, adds a hop, and keeps a second allowlist. A browser cannot speak native gRPC.

## Decision

`asc serve` answers gRPC-Web (`application/grpc-web[+proto]`) and Connect unary (`application/proto`) on the existing RPC paths, behind the existing listener, TLS or trusted-proxy rules and HTTP/3. It is on when `[web] enabled` (or `[web] grpc_web = true`) and off otherwise.

- It is a small tower layer in `crates/asc-serve/src/web/grpc_web.rs`, not the `tonic-web` crate: no new dependency to audit, and CORS, a request cap, cookie stripping and Connect unary are part of the same code instead of being left to the caller. Streaming works because trailers are moved into a final frame while data frames pass through as produced.
- It wraps `limits::guarded_routes`, so a browser call is a native gRPC call after translation: the same admission, `Authorizer` and service limits decide it.
- Credentials are the bearer token in `Authorization`. Cookies are dropped; no credentialed CORS.
- CORS is closed by default (same origin only). `[web] cors_origins` lists exact extra origins; never a wildcard. Other origins get 403.
- Request bodies are capped (`[web] max_request`, default 1 MiB).

## Consequences

- The UI works against any asc server; the Forgejo proxy remains an option, not a requirement.
- There is no per-method browser allowlist: a browser token can call whatever that token may call natively. Narrow a token's scope in the token store, not in this layer.
- gRPC-Web text, Connect JSON and Connect streaming are not served; they can be added in the same file.
- If a future SPEC revision re-adopts `tonic-web`, the layer can be swapped without changing the wire behaviour or the config.
