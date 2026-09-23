# AGENTS.md

COSMIC desktop app (libcosmic, Rust edition 2024) for film roll library management and film negative RAW image editing. Single-binary crate; early stage.

- `NOTES.md` tracks current working state and next-step targets for in-progress feature work. Read it before continuing work; update it when the state changes.

## Commands

- Lint/verify: `just check` — runs `cargo clippy --all-features --locked -- -W clippy::pedantic`. Use this to validate changes.
- Tests: `cargo test --locked` — unit tests for pure pipeline helpers in `src/app.rs`.
- Run the app: `just run` — builds and runs in **release** profile with `RUST_BACKTRACE=full` (not debug).
- Build: `just` (= `build-release`) or `just build-debug`.
- Edition 2024 needs a recent stable toolchain (rustup).
- Just recipes use `--locked`; the vendored build recipe (`build-vendored`) uses `--frozen --offline` since it must not touch the network.

## Dependencies

- `libcosmic` is a **git dependency on pop-os master**, not crates.io. `Cargo.lock` pins the commit; always build/check with `--locked` (the just recipes already do).

## Codegen and i18n

- `build.rs` runs `xdgen` at compile time: generates `target/xdgen/app.desktop` and `app.metainfo.xml` from the templates in `resources/` combined with fluent strings from `i18n/`. Edit templates in `resources/`, never generated output (`target/` is gitignored).
- User-facing strings use the `fl!` macro with message IDs from `i18n/en/curvectrl.ftl`. Add new messages there; missing translations fall back to English.

## Conventions

- Every `.rs` file starts with `// SPDX-License-Identifier: GPL-3.0-or-later` (repo license is GPL-3.0-or-later).
- Distro packaging/vendoring flow is documented in README (`just vendor` → `just build-vendored`; `install` honors `rootdir`/`prefix`).
