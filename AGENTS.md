# AGENTS.md

COSMIC desktop app (libcosmic, Rust edition 2024) for film negative scanning and RAW image editing. Single-binary crate; early stage. Unit tests cover the thumbnail color pipeline.

- `NOTES.md` tracks current working state and next-step targets for in-progress feature work (thumbnails/RAW rendering). Read it before continuing that work; update it when the state changes.

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

- Every `.rs` file starts with `// SPDX-License-Identifier: MPL-2.0` (repo license is MPL-2.0).
- Distro packaging/vendoring flow is documented in README (`just vendor` → `just build-vendored`; `install` honors `rootdir`/`prefix`).
- Expect ~130 clippy warnings from compiling libcosmic master from source; only warnings pointing into `src/` are actionable.

<!-- GILJOAI_MCP_PRIMER_START -->
## Giljo HQ -- what it is
A project-management and agent-coordination platform driven over MCP.
- Product -- top-level container holding baseline context (tech stack,
  architecture, conventions). Work happens under an active product.
- Project -- an actionable, multi-step body of work under a product. Agents
  execute it by receiving work-order assignments from an orchestrator.
- Orchestrator workflow -- the user activates a project; the orchestrator
  plans it, assigns jobs/work orders, and STOPS at staging (the human gate).
  The user triggers implementation; agents execute; the project closes out
  with a 360 memory entry.
- Chain execution -- the user links multiple projects to run back-to-back.
  The orchestrator is promoted to a conductor (master orchestrator) that
  spawns sub-orchestrators; each does its own staging, work-order writing,
  and team assembly for its project in the chain. A chain is multi-project,
  single-user (not Teams).
- Tasks -- a deferral list of smaller items the user can review and promote
  into projects.
- 360 memory -- the durable, detailed history layer; works alongside Git as
  the cross-session record.
- Commands -- the /giljo skill (or $giljo, or calling get_giljo_guide) loads
  the full command/routing instructions on demand.
<!-- GILJOAI_MCP_PRIMER_END -->
