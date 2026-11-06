# Porch

Porch is an in-progress Rust implementation of named local development URLs, inspired by Portless.

The CLI implementation has now landed and is evolving. Build with `cargo build --release`; run `cargo test --all-targets` for validation. Platform support and full Portless compatibility remain unverified.

```sh
porch proxy start --foreground --port 1355
# In another terminal:
porch demo -- your-server-command
```

The child receives PORT. Open http://demo.localhost:1355. The proxy must be started separately. Static routes use `porch alias demo http://127.0.0.1:3000`; `porch get demo` currently prints the upstream target, not a browser-facing URL.

See [feature comparison](docs/feature-comparison.md) for the frozen upstream baseline and [reference](docs/reference.md) for the command, configuration, and environment inventory.

Development ownership: this documentation work is limited to README.md, docs/feature-comparison.md, docs/reference.md, .github/workflows/, and tests/. Cargo.toml and implementation files belong to their implementation owners.

CI runs Cargo checks when a manifest exists and reports its absence explicitly during bootstrap. Release artifacts require an actual build and tests; a release workflow does not establish runtime feature parity.
