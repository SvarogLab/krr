# Code style

## Formatting

`rustfmt` with the repository's `rustfmt.toml`; `cargo fmt --all -- --check` passes. Markdown passes
`markdownlint-cli2` with `.markdownlint-cli2.jsonc`.

## Lints

`cargo clippy --all-targets --all-features -- -D warnings` passes.

Lints live in `[lints]` in `Cargo.toml`. Pedantic lints are not enabled as a group. One is added
there by name when it enforces an idiom of current Rust, such as `let ... else` or inline format
arguments. A threshold goes into `clippy.toml` only with a comment naming the code it is for.

`cargo deny check` passes with `deny.toml`. It covers advisories, licenses, bans and sources.

## Layout

- A module with submodules lives in `name.rs` beside a `name/` directory, never in `name/mod.rs`
  (`clippy::mod_module_files` enforces it).
- A module maps to one part of upstream. The README's architecture table names its counterpart.
- Unit tests sit in a `tests` module at the bottom of the file they test. Tests that run the binary
  or a whole scan against mock servers live in `tests/`, and their fixtures in `tests/fixtures/`.

## Naming

Types and functions use upstream's terms: a scan, an allocation, a strategy, a recommendation. That
way, the port can be read side by side with the code it comes from. Test names are sentences stating
the behaviour they hold.

## Errors

- Fallible code returns `anyhow::Result`, and `context` or `with_context` names what was being
  attempted, such as the query, the URL or the file.
- `unwrap` belongs in tests. Elsewhere, an `expect` states the invariant that makes it unreachable.

## Behaviour parity

Where output is observable, a test pins it to upstream's, using values produced by upstream's own
code wherever possible. Behaviour that deviates on purpose is recorded in
[`PORTING.md`](PORTING.md).

## Comments

Code says what it does. A comment says why, when the reason lives outside the code: upstream
behaviour being kept, a limit of Prometheus or of the Kubernetes API, or a library's quirk.

## Dependencies

Requirements in `Cargo.toml` name the current release, and `Cargo.lock` is tracked. Renovate
proposes updates through the shared preset in `renovate.json`.
