# AGENTS.md

## Project

- Rust 2021 workspace; the primary crate is `apps/core`, and `spikes/` is intentionally excluded from the workspace.
- Check changes with `cargo fmt --all --check` and `cargo clippy --all-targets --all-features --locked -- -D warnings`.
- Run `cargo test --locked` after logic changes.
- Keep implementations minimal and Windows/JVM-instrumentation behavior explicit.
- The default Fabric correctness path is documented in `docs/architecture.md`: prepare and mount mods before JVM creation; JVMTI redefine is only an explicit post-load fallback.
- Do not commit generated build output, downloaded fixtures, runtime logs, or spike projects. Version-control boundaries are defined in `.gitignore`.

## Codex

- Repository skills live in `.agents/skills`.
- Project MCP servers are configured in `.codex/config.toml`; trust this project when Codex prompts.
