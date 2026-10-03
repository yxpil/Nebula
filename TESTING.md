# Testing Nebula

Nebula is a workspace of crates (a memory database with a hand-written SQL
subset, page-based encrypted storage, a TCP server and a CLI). Tests are split
between inline `#[cfg(test)]` unit tests inside each crate and `tests/`
integration tests. All run under `cargo test` from the repo root.

## Layout

| Crate | Unit tests (`#[cfg(test)]`) | Integration tests (`tests/`) |
|-------|------------------------------|------------------------------|
| `nebula-core` | binary codec, shared errors | — |
| `nebula-crypto` | KDF, AEAD, HMAC/HKDF round-trips | — |
| `nebula-sql` | lexer/parser/AST for every supported statement | `database.rs`-style coverage in src; **`injection.rs` (new)** |
| `nebula-tokenizer` | CN/EN tokenizer, stopwords | — |
| `nebula-storage` | page/pager/records internals | `memory_file.rs` |
| `nebula-engine` | records, inverted index, executor, auth, cache | `database.rs` |
| `nebula-config` / `nebula-cluster` | TOML config, users & grants | — |
| `nebula-server` | protocol, challenge-response | `e2e.rs` |
| `nebula-cli` | render/repl helpers | — |

## Run

```sh
# everything
cargo test --workspace

# one crate, e.g. the SQL parser
cargo test -p nebula-sql
```

Integration tests use ephemeral temp directories and loopback ports; no real
network or persistent data directory is required.

## What the injection tests assert (`crates/nebula-sql/tests/injection.rs`)

Nebula builds a strongly-typed AST and the engine only compares bound values —
it never concatenates user input into SQL. The parser tests prove the front
line of that defense:

- a string with `''` escaping keeps an injected boolean tautology
  (`'x'' OR ''1''=''1'`) as **opaque data** in one `KeywordEq` literal — never as
  an `OR` node;
- `--` line comments and `/* */` block comments are lexer errors (there is no
  comment syntax to hide injected text);
- stacked dangerous statements (`DROP TABLE`, `EXECUTE IMMEDIATE`) and trailing
  tokens after a statement are rejected;
- `LIKE` patterns with inner wildcards (`%a% OR 1=1 --%`) are rejected;
- a string where an integer `id` is required is a type error, never coerced;
- an XSS payload stored via `INSERT` stays a plain `Literal::Str`;
- a legitimate `OR` between two typed predicates still parses (control) — OR is
  not blanket-rejected, only kept out of string literals.

## Hook / plugin / event / callback tests

Nebula has **no runtime hook, plugin or event-bus mechanism**. Its cross-cutting
concern is an authorization model: users and per-database `GRANT`/`REVOKE`
privileges (`Privilege` = Read/Write/Admin) are parsed by `nebula-sql` and
enforced by `nebula-engine`/`nebula-cluster`. Privilege enforcement and
challenge-response auth are exercised by the existing `nebula-engine/tests/
database.rs` and `nebula-server/tests/e2e.rs`; there are no user-registerable
callbacks to test, so the hook-test count is **0**.

## Expected result

`cargo test -p nebula-sql` should finish green: unit 24 + `injection.rs` 7
(passing:failing = 31:0). The workspace as a whole (`cargo test --workspace`)
retains its pre-existing unit + integration suites; nothing in this change
touches any production source.
