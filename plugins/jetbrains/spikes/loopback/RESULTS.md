# Spike P55: OAuth loopback (SPEC 24.6)

Question: does the JetBrains sign-in work when the built-in server port moves, under Remote Dev, with `state`, PKCE and a
variable-port redirect that P06 must accept?

## Verdict: asc half designed and unit-tested (tests not yet executed here); platform half PENDING a live IDE.

## Decisions this spike fixes for P42

- Redirect is `http://127.0.0.1:<live port>/api/asc/oauth/callback`, built per sign-in from
  `BuiltInServerManager.getInstance().port` (read at call time). Never `localhost`, never a wildcard bind.
- P06 must accept `http`, host `127.0.0.1` (and `[::1]`), any port 1..65535, exact path, no userinfo/query/fragment
  (`Redirect.p06Accepts` models the rule; it should become a P06 test case).
- `state`: 128 random bits, single use, valid 5 minutes (inclusive), removed on first lookup even if expired. Unknown,
  replayed, expired or missing state is ignored and leaves other pending flows untouched. First duplicate parameter wins.
- PKCE: S256 only, 43-char verifier held in the flow and never placed in the authorize URL; the code is exchanged with that
  flow's verifier, so a callback is accepted only when state and verifier belong to the same flow.
- Authorize endpoint must be https. Callback accepted only from a loopback peer on the exact path.
- Fallback: with no built-in server (null, exception, port 0) an own `ServerSocket` on 127.0.0.1, OS-chosen port.

## Needs a live IDE (2026.1, 2026.2), not proven here

1. Registering the callback with the built-in server (`HttpRequestHandler`), and that the port the browser sees equals
   `BuiltInServerManager.port` on the machine running the browser.
2. Remote Dev: the backend's port is not reachable from the client's browser. Expected: the own-socket fallback must run on
   the client (thin client) side, or the PAT path is used. Verify and record.
3. A real P06 round trip once the Forgejo client `asc-ide` exists.

## Verification status

`gradle` and `kotlinc` are not installed on the authoring machine (JDK 17 only), so `LoopbackTest` was written but not
executed. Run `gradle test` in this directory, or add `include(":spikes:loopback")` to the root settings (owned by P36).
Security tests to prove by revert once runnable: drop the single-use removal in `StateStore.consume`, the expiry check, or the
loopback/path check, and `validCallbackAcceptedOnce`, `expiredStateIgnored`, `nonLoopbackPeerAndWrongPathIgnored` must fail.
