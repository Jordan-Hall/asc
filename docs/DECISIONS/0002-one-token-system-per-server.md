# 0002 One token system per server

Status: accepted, 2026-11-10. Settles STATUS row 27: the roadmap rule "never a second token system" against the standalone token store (`TokenStoreAuthz`, M30).

## Context

With a forge in front, asc asks the forge about every credential (`decide_scoped` in `crates/asc-serve/src/forgejo.rs`): the forge's personal access tokens, OAuth tokens and Actions tokens are the only tokens, and their scopes are honoured. A server with no forge still needs credentials, so `asc serve token create` mints `asc p_<id>_<secret>` tokens into the server database.

The rule exists so that a person never holds two kinds of token for one server, and so that a revoke in one place is a revoke everywhere.

## Decision

`[auth] provider` selects exactly one token system for a server, and nothing else issues tokens on it.

- `provider = "forgejo"`: the forge issues every token and SSH key. `auth.token_key_file` is refused at config load, and `asc serve token` and `asc serve ssh-key` refuse to run, so no store token can be minted beside the forge's.
- `provider = "token_store"`: the store is the only token system, for servers with no forge.
- No provider: every credential is refused; a token key is refused at config load.

The token store stays: it is the one token system of a standalone server, not a second one.

## Consequences

- Moving a standalone server behind a forge means switching the provider; the old store tokens stop working at once, and the server cannot accept both during a migration.
- The checks are in `ServeConfig::validate` (`crates/asc-serve/src/config.rs`) and `token_cli::open`; the tests are `config_validates_the_auth_keys` and `the_cli_mints_nothing_on_a_forge_fronted_server` in `crates/asc-serve/tests/token_store.rs`.
