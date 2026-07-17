# Contributing

Thanks for your interest in improving `claude-sidecar`!

## Development

Requires a recent stable Rust toolchain.

```bash
cargo build
cargo run -- --verbose        # run locally on 127.0.0.1:8765
```

## Before opening a PR

Make sure the full check pipeline is green — CI runs exactly these:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo fmt --all` fixes formatting for you.

## Guidelines

- Keep it simple — this is a small, focused tool. Prefer the least code that
  solves the problem at hand.
- No `.unwrap()` / `.expect()` on request paths; return a typed `SidecarError`.
- Add tests for new behavior. Unit tests live at the bottom of each module;
  HTTP-level tests are in `src/routes/mod.rs`.
- Never hold a lock across an `.await`; do blocking I/O on `spawn_blocking`.
- The command allowlist lives in `src/config.rs` (`ALLOWED_COMMANDS`).

## Reporting issues

Please include the command you ran, the request and response, and anything in
the server's stderr log (run with `-v` for per-line output).

## License

By contributing, you agree that your contributions will be dual licensed under
the MIT and Apache-2.0 licenses (see the README), without any additional terms
or conditions.

