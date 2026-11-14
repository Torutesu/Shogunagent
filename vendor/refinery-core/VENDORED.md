# Vendored refinery-core 0.10.1

This is `refinery-core` 0.10.1 exactly as published on crates.io (`src/` is byte-identical),
with **one** change in `Cargo.toml`: the optional `rusqlite` dependency bound is widened from
`>= 0.23, <= 0.41` to `>= 0.23, <= 0.42`.

Why: RUSTSEC-2026-0459 / RUSTSEC-2026-0502 (a C SQLite CVE) are fixed only in
`libsqlite3-sys` 0.40 / `sqlcipher-src` 420 (SQLCipher 4.20.0, SQLite 3.54.1), which rusqlite 0.42
pulls in. No published `refinery` release accepts rusqlite 0.42 yet, including upstream `main` as
of 2026-11-14. refinery-core only uses rusqlite APIs that 0.42 keeps (the workspace builds and the
upgrade-lineage tests pass against it).

It is wired in through `[patch.crates-io]` in the workspace `Cargo.toml`.

**Remove this directory and the patch entry** as soon as a refinery release accepts rusqlite 0.42:
bump `refinery` in `crates/shogun-memory/Cargo.toml`, delete the `[patch.crates-io]` entry, and run
`cargo update -p refinery-core`.

License: MIT (upstream `LICENSE`, copied alongside). Upstream: https://github.com/rust-db/refinery
