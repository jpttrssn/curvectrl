# Exposure — Product Vision

*Last updated: 2026-08-24 · Basis: repository audit at commit `97b417c`*

## What Exposure Is

Exposure is a free-software (MPL-2.0) desktop application for film photographers who digitize their own negatives: a **film negative scanning and RAW image editor**, built natively for the COSMIC desktop environment. Point it at a folder of camera-scanned RAW frames and it renders them as proper photographic positives — not inverted-looking curiosities — using a pipeline designed around how film actually behaves: optical density, clear-film base, and per-stock tone curves.

Today Exposure is an early-stage, single-binary Rust app (~1,400 lines) whose first milestone is a high-fidelity contact sheet: it scans `~/Pictures/exposure`, decodes each RAW frame, reconstructs a monochrome negative at full sensor resolution, and shows inverted, calibrated positive thumbnails in a responsive grid. The longer arc is a complete darkroom: library browsing, non-destructive editing, and export for monochrome and color stocks alike.

## Who It Is For

- **Film photographers who scan by camera** — shooting negatives with a DSLR/mirrorless body (the dominant modern home-scanning workflow) and ending up with folders of CR2/NEF/ARW RAW files.
- **Linux users**, especially COSMIC users, who prefer fast native tools over Electron apps or heavyweight cross-platform suites.
- **Monochrome shooters first** — the current proof of concept targets B&W stocks such as Ilford HP5+ — with color stocks as the pipeline matures.
- Secondary: anyone wanting a fast, private, local RAW browser/editor with no accounts and no cloud.

## Core Value Proposition

Generic editors treat a scanned negative like an upside-down photo and leave you hand-wrestling curves. Exposure treats negatives as measurable physical objects:

1. **Density-space inversion anchored on the film itself.** Each frame's clear-film base is measured statistically (p95 estimator, dust/outlier-robust) so black-point calibration is automatic and capture casts neutralize themselves — no per-image fiddling.
2. **Grain-preserving reconstruction.** Bayer CFA classes are rescaled onto a common base at full sensor resolution instead of being demosaiced, keeping the grain texture and microcontrast that half-res interpolation smears away.
3. **Film-stock-aware tone mapping.** Inversion runs through per-stock profiles (`base`, `d_max`, `gamma`) in optical-density space — the same axes published film characteristic curves use.
4. **Local-first and bounded.** Everything happens on your machine; frames decode lazily and strictly sequentially so only one full-resolution RAW buffer is ever alive (~50–100 MB peak).
5. **Native COSMIC experience** — system theming, keyboard/touch support, translations, and desktop integration from day one.

## Main Features

Working today:

- **Thumbnail library grid** — every regular file in `~/Pictures/exposure`, listed once at startup, displayed as square adaptive tiles (≤384 px) with progressive, one-at-a-time decoding and loading/failed states.
- **Monochrome RAW pipeline** — per-CFA-position black/white-level normalization → masked-border cropping → full-resolution bayer flattening (or Rec.709 luminance collapse for ≥3-sample pixels) → statistical clear-film base measurement with plausibility guard → optical-density-space inversion against the active stock profile (Ilford HP5+, proof of concept) → linear-light block-average downscale → gentle separable unsharp mask → sRGB encode → EXIF orientation fix.
- **Film stock model** — `MonoStock` profile struct carrying calibrated parameters; per-channel inversion path retained and tested for future color stocks.
- **COSMIC integration** — nav-bar pages, header menu, About drawer, persistent settings via cosmic-config, single-instance launch, Fluent localization with English fallback.

Planned (tracked in NOTES.md):

- Stock picker UI; color stocks with per-channel base colors and camera input matrices.
- "Calibrate from a base-only frame": anchor scans lacking measurable margins; optional AsShot white-balance prior.
- Folder refresh/watching; surfaced decode errors instead of silent failure.
- Bounded thumbnail memory (LRU eviction).
- Optional denoise pass for very underexposed captures.
- Full viewer/editor/export workspace — nav pages 2–3 are scaffolds awaiting it.
- Polished distribution (the vendored, distro-packaging build path already exists).

## Current Tech Stack

| Area | Choice |
|---|---|
| Language | Rust, edition 2024 |
| UI toolkit | libcosmic (git dependency on pop-os master, pinned by `Cargo.lock`) over iced; wgpu GPU-accelerated rendering |
| RAW decoding | rawloader 0.37 |
| Async runtime | tokio (`spawn_blocking` for CPU-heavy decode) |
| Localization | i18n-embed + Fluent (`fl!` macro; `i18n/en/exposure.ftl`) |
| Asset embedding | rust-embed |
| Codegen | xdgen at build time → `app.desktop` + `app.metainfo.xml` from `resources/` templates |
| Build/dev tooling | Cargo (`--locked` everywhere), just recipes (release, debug, vendored, install, check), rust-analyzer; mold/sccache recommended for iteration speed |

No databases; persistence is the filesystem plus cosmic-config-backed app settings.

## Architecture Overview

Single process, single binary, Elm-style **Model–View–Update** through `cosmic::Application`:

- **State** — `AppModel` holds the COSMIC core, nav model, key binds, persisted config, and the tile list; each `Tile` carries a `Thumb::{Loading, Ready(Handle), Failed}` state.
- **Messages and tasks** — widgets and async jobs speak via a `Message` enum; init batches window-title setup with the directory scan, and each completed decode chains the next through `decode_next()`.
- **Background work** — RAW decoding runs one frame at a time on `tokio::task::spawn_blocking`; the strict chain keeps peak memory predictable.
- **Image pipeline** — pure, side-effect-free functions split between `src/app.rs` (normalization, cropping, bayer flattening, luminance, resizing, unsharp mask, sRGB, orientation) and `src/film.rs` (stock profiles, base measurement, density-space inversion). The split is deliberate: the math is unit-testable headlessly, and the UI consumes finished handles.
- **UI layer** — iced `Grid::fluid` layout with aspect-ratio-constrained square cells; decoded handles rendered directly; pages switched via the nav model.
- **Platform glue** — cosmic-config for settings, rust-embedded Fluent catalogs, and build.rs/xdgen generating desktop + metainfo files from templates (never edited by hand; `target/` output is gitignored).

Known architectural debts, consciously tracked: no folder re-scan after startup, unbounded thumbnail cache, silent decode errors, one hardcoded stock, and a placeholder in-code `APP_ID`.

## Quality and Testing Approach

- **Lint gate**: `just check` runs `cargo clippy --all-features --locked -- -W clippy::pedantic`; warnings pointing into `src/` are treated as defects (known framework noise from compiling libcosmic master is ignored).
- **Reproducibility**: every build/test runs `--locked` against the pinned dependency graph; the vendored recipe additionally runs frozen/offline for distro packaging.
- **Unit tests** (`cargo test --locked`): property-style tests around the color math — inversion maps base→black and d_max→white, stays monotonic between endpoints, clamps out-of-range input, neutralizes casts via measured channel bases, falls back on implausible bases, respects CFA phase shifts and fourth-color sites, keeps gray/RGB paths equivalent on neutrals; resize produces exact block averages and is identity without shrink; unsharp mask leaves flat buffers untouched, raises edge contrast, clamps overshoot; crop rejects degenerate regions; sRGB matches reference values.
- **Manual visual QA**: perceptual constants (gamma 0.7, unsharp amount 0.4) are tuned against real HP5+ scans; the stock's base constant was calibrated from measured plateau data, not guessed. Decisions and gotchas are logged in NOTES.md.
- No formal coverage percentage is enforced yet; the covered surface is the entire pure-pipeline core.

## Where Exposure Is Heading

1. **From inversion POC to a stock-aware darkroom** — stock picker, per-channel color bases, calibration frames, and input transforms for color accuracy.
2. **A trustworthy library** — refreshable/watched folders, visible error states, bounded caches, comfort at thousand-file scale.
3. **An editing workspace** — crop, exposure, and tone controls operating on the same density-space pipeline, then JPEG/TIFF export, turning the contact sheet into a full scan workbench.
4. **Robustness** — denoise for thin negatives, graceful handling of margin-less frames, diagnosable failures.
5. **Distribution** — keep the vendored, distro-friendly build path healthy and publish packaged builds.

Fixed principles throughout: local-first, grain-honest processing (decisions at full resolution before display scaling), parameters that are measured rather than eyeballed wherever possible, and COSMIC-native UX.
