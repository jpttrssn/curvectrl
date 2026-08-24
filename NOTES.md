# NOTES — thumbnail listing & RAW rendering (working state)

Reference for continuing work on Page 1 thumbnails. All code in `src/app.rs`.

## Current behavior

- Page 1 lists every regular file in `$HOME/Pictures/exposure` as tiles in a responsive grid, sorted by name. Scanned once at startup.
- Columns adapt to window width; cells never exceed `THUMB_SIZE` px wide and stay square (`Grid::fluid` + `Sizing::AspectRatio(1.0)`). Previously a fixed 5-per-row manual layout.
- Each tile: image area (fills its square cell, `ContentFit::Contain`) + file name below. `image-loading-symbolic` / `image-missing-symbolic` icons for pending/failed.
- Thumbs are rendered as inverted positives via the film pipeline: per-channel level normalize → crop masked borders → half-res true-color demosaic (gray box-average fallback) → `measure_base_channels` (per-channel p95 of the linear buffer = self-calibrating WB anchored on clear film; kills light-table/sensor casts) → `film::invert_mono` (density-space B&W inversion, always on, hardcoded stock) → linear-light area-average downscale to `THUMB_SIZE` → sRGB transfer curve. cpp≥3 sources pass through before the same anchoring+inversion.
- `src/film.rs`: `MonoStock { name, base, d_max, gamma }`; `ACTIVE_STOCK` = Ilford HP5+ (`base=0.82` measured from real scans' brightest band plateau, p95 estimator via `measure_base`; `d_max=2.4`, `gamma=0.7` starting values pending visual tuning). Inversion: per-channel density `log10(base_c/v)` → normalized position `clamp(density/d_max, 0..1)` → tone curve `position^gamma`. Clear film (each channel's measured base) prints neutral black, the film's usable density limit prints white; channel bases below `MIN_PLAUSIBLE_BASE` (0.1 — no measurable clear film) fall back to `stock.base`.
- Thumbs decode progressively, one at a time, in file order.

## Architecture (all in src/app.rs)

- State: `tiles: Vec<Tile>`; `Tile { name: String, thumb: Thumb }`; `Thumb::{Loading, Ready(Handle), Failed}`.
- Messages: `FilesLoaded(Vec<String>)` builds tiles then starts chain; `ThumbReady(String, Result<Handle, ()>)` stores result and chains next.
- `decode_next()` picks first `Loading` tile → one `tokio::task::spawn_blocking` per file (`decode_thumbnail`). Strictly sequential so only one full-res RAW buffer is alive at a time (~50–100 MB peak).
- Pipeline: `rawloader::decode_file` → `normalize_samples` (per-CFA-position black/white levels for Integer data; max-gain for Float) → `crop_samples` (slices off `crops`-flagged masked borders; skipped if degenerate) → bayer: `demosaic_half` on the CFA shifted by the crop origin (`cfa.shift(left, top)`; 2×2 blocks accumulated via `color_at`; `None` → legacy gray box-average); cpp≥3: passthrough → `measure_base_channels` + `film::invert_mono(rgb, &ACTIVE_STOCK, bases)` → `resize_area` (block-average downscale to `THUMB_SIZE`, in linear light, before tone encoding — suppresses noise/grain aliasing) → `srgb_encode` on all values → u8 quantize → orientation fix (`orient`, all rawloader variants) → `Handle::from_rgba`. White balance was removed when inversion landed (mono negatives are neutral); per-image per-channel base anchoring replaced it.
- Constants: `THUMB_SIZE = 384.0` (f32; decode cap + grid cell width target), `TILE_ASPECT = 1.0`.
- View: Page 1 builds `cosmic::iced::widget::Grid::with_children(tiles.map(tile_view))` with `.fluid(THUMB_SIZE)` + `.height(grid::Sizing::AspectRatio(TILE_ASPECT))`, wrapped in a scrollable. `tile_view()` fills the square cell the Grid assigns (Grid constrains each child to exact cell w×h); no manual rows/padding cells, no aspect-ratio widget.
- iced's Grid (re-exported at `cosmic::iced::widget::{Grid, grid}`) differs from cosmic's own `widget::grid` (taffy-based, no column control — rejected earlier).

## Known limitations (likely next-step targets)

1. **Color accuracy / stocks**: inversion POC is mono-only with one hardcoded stock (`film::ACTIVE_STOCK`); cast WB is self-calibrated per image from clear-film plateaus, so frames with no measurable margins rely on the `MIN_PLAUSIBLE_BASE` fallback (statistical guard, not guaranteed); no `xyz_to_cam` matrix (moot for mono, matters for color stocks). Next natural steps: stock picker UI, "calibrate base from a base-only frame" feature (`film::measure_base` is the math), optional AsShot-WB prior for margin-less frames, color stocks with per-channel base colors.
2. **No refresh** of the folder listing after startup.
3. **Memory unbounded** across many decoded thumbs (no LRU/eviction); ~600 KB each at current size.
4. Decode errors are silent (`Err(())`); non-RAW files are listed and show the missing icon.
5. **Grain/noise**: area-average downscale (done) is step 1; remaining stackable options — collapse mono RGB to luminance before inversion (~√3 SNR gain, mono stocks only) and/or a gentle 3×3 pre-blur of the demosaiced linear buffer. Capture-side: scans exposed with the clear-film base far left in the histogram waste SNR; exposing so the base sits near saturation (~85–95%) keeps several more stops of detail in dense areas (user's current habit trades this away deliberately for highlight headroom).

## Environment gotchas

- `libcosmic` is a plain git dependency on pop-os master (lockfile pins `ef490df5`); the local clone at `../cosmic/libcosmic` is no longer patched in. Upstream includes the old `AspectRatio::layout` off-by-one fix and iced's `Grid` export. The commented `[patch]` template in `Cargo.toml` can restore the clone if needed.
- Verify changes with `just check` (clippy pedantic) and `cargo test --locked`. Expect ~130 warnings from compiling libcosmic master from source — ignore those; only warnings pointing into `src/app.rs` or `src/film.rs` matter.
- Unit tests live in `mod tests` blocks in `src/app.rs` and `src/film.rs` (thumbnail color/inversion math). Run via `cargo test --locked`.

## Useful API facts (verified against pinned libcosmic/rawloader)

- `cosmic::Task<M>` aliases `iced::Task<cosmic::Action<M>>`; async work: `cosmic::task::future(async { Message::X(...) })`; batch with `Task::batch([...])`.
- `rawloader::{RawImage, RawImageData, Orientation}` are re-exported at crate root (`decoders` module is private).
- `RawImage` fields used here: `width`, `height`, `cpp`, `data`, `wb_coeffs` ([f32;4] RGBE), `blacklevels`/`whitelevels` ([u16;4]), `orientation`, `cfa`.
- `rawloader::CFA` is crate-exported; `CFA::new("RGGB")` builds test patterns; `color_at(row, col)` mods internally (`(row+48)%48`), so any coordinates are safe. Returns 0=R, 1=G, 2=B, 3=fourth-color. `shift(x, y)` re-phases the pattern after cropping by `(left=x, top=y)`.
- `RawImage.crops: [usize;4]` is `[top, right, bottom, left]`. Tested Canon CR2s flag top/left only; their LAST two sensor rows also read elevated (~9k vs ~2k ADU) despite `bottom=0` — if a bottom-edge artifact ever appears, suspect unflagged masked rows.
- Icons: `icon::from_name("...").icon().into()` yields an Element; image handles clone cheaply.
