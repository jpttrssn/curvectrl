# NOTES — thumbnail listing & RAW rendering (working state)

Reference for continuing work on Page 1 thumbnails. All code in `src/app.rs`.

## Current behavior

- Page 1 lists every regular file in `$HOME/Pictures/exposure` as tiles in a responsive grid, sorted by name. Scanned once at startup.
- Columns adapt to window width; cells never exceed `THUMB_SIZE` px wide and stay square (`Grid::fluid` + `Sizing::AspectRatio(1.0)`). Previously a fixed 5-per-row manual layout.
- Each tile: image area (fills its square cell, `ContentFit::Contain`) + file name below. `image-loading-symbolic` / `image-missing-symbolic` icons for pending/failed.
- Thumbs are rendered as inverted positives via the film pipeline: per-channel level normalize → crop masked borders → mono reconstruction (bayer: `flatten_bayer` treats each CFA class as an independent density measurement and rescales classes onto one common clear-film base at FULL sensor resolution — replaced half-res true-color demosaic to keep grain texture/microcontrast; cpp≥3: passthrough → `luma()` linear Rec.709 collapse) → `measure_base` (p95 = black point anchored on clear film) → `film::invert_gray` (density-space B&W inversion, always on, hardcoded stock) → linear-light area-average downscale to `THUMB_SIZE` → gentle unsharp mask (`UNSHARP_AMOUNT`=0.4, [1,2,1]² blur, linear light before sRGB so halos/shadow noise stay quiet) → sRGB transfer curve.
- `src/film.rs`: `MonoStock { name, base, d_max, gamma }`; `ACTIVE_STOCK` = Ilford HP5+ (`base=0.82` measured from real scans' brightest band plateau, p95 estimator via `measure_base`; `d_max=2.4`, `gamma=0.7` starting values pending visual tuning). Inversion (shared `positive()` helper): density `log10(base/v)` → normalized position `clamp(density/d_max, 0..1)` → tone curve `position^gamma`. Clear film (measured base) prints black, the film's usable density limit prints white; a measured base below `MIN_PLAUSIBLE_BASE` (0.1 — no measurable clear film) falls back to `stock.base`. The per-channel path (`invert_mono` + `measure_base_channels`, both `#[allow(dead_code)]`) is kept with tests for future color stocks; `invert_gray` must stay equivalent to it on neutral pixels (tested).
- Thumbs decode progressively, one at a time, in file order.

## Architecture (all in src/app.rs)

- State: `tiles: Vec<Tile>`; `Tile { name: String, thumb: Thumb }`; `Thumb::{Loading, Ready(Handle), Failed}`.
- Messages: `FilesLoaded(Vec<String>)` builds tiles then starts chain; `ThumbReady(String, Result<Handle, ()>)` stores result and chains next.
- `decode_next()` picks first `Loading` tile → one `tokio::task::spawn_blocking` per file (`decode_thumbnail`). Strictly sequential so only one full-res RAW buffer is alive at a time (~50–100 MB peak).
- Pipeline: `rawloader::decode_file` → `normalize_samples` (per-CFA-position black/white levels for Integer data; max-gain for Float) → `crop_samples` (slices off `crops`-flagged masked borders; skipped if degenerate) → mono: bayer sources via `flatten_bayer` on the CFA shifted by the crop origin (`cfa.shift(left, top)`; strided subsample ~250k/class via odd step that can't alias the period-2 grid; per-class `measure_base` p95 with `MIN_PLAUSIBLE_BASE` guard; reference base = green class when present else dimmest measurable; emit `value·ref/base_class`); cpp≥3: passthrough triplets then `luma()` → scalar `film::measure_base` + `.filter(>= MIN_PLAUSIBLE_BASE)` + `film::invert_gray(mono, &ACTIVE_STOCK, base)` → `resize_area(..., channels)` (block-average downscale to `THUMB_SIZE`, in linear light, before tone encoding — suppresses noise/grain aliasing) → `unsharp_mask` (`blur_121` separable [1,2,1]², edge-replicating; amount `UNSHARP_AMOUNT`) → `srgb_encode` on all values → u8 quantize as `[v,v,v,255]` → orientation fix (`orient`, all rawloader variants) → `Handle::from_rgba`. The old half-res demosaic and gray box-average fallback were deleted when full-res flattening landed (it handles any CFA incl. fourth-color patterns).
- Constants: `THUMB_SIZE = 384.0` (f32; decode cap + grid cell width target), `TILE_ASPECT = 1.0`, `UNSHARP_AMOUNT = 0.4` (post-downscale sharpening strength, tune visually), `BASE_SAMPLE_TARGET = 250_000` (flatten_bayer subsample size per CFA class).
- View: Page 1 builds `cosmic::iced::widget::Grid::with_children(tiles.map(tile_view))` with `.fluid(THUMB_SIZE)` + `.height(grid::Sizing::AspectRatio(TILE_ASPECT))`, wrapped in a scrollable. `tile_view()` fills the square cell the Grid assigns (Grid constrains each child to exact cell w×h); no manual rows/padding cells, no aspect-ratio widget.
- iced's Grid (re-exported at `cosmic::iced::widget::{Grid, grid}`) differs from cosmic's own `widget::grid` (taffy-based, no column control — rejected earlier).

## Known limitations (likely next-step targets)

1. **Color accuracy / stocks**: inversion POC is mono-only with one hardcoded stock (`film::ACTIVE_STOCK`); cast WB is self-calibrated per image from clear-film plateaus, so frames with no measurable margins rely on the `MIN_PLAUSIBLE_BASE` fallback (statistical guard, not guaranteed); no `xyz_to_cam` matrix (moot for mono, matters for color stocks). Next natural steps: stock picker UI, "calibrate base from a base-only frame" feature (`film::measure_base` is the math), optional AsShot-WB prior for margin-less frames, color stocks with per-channel base colors.
2. **No refresh** of the folder listing after startup.
3. **Memory unbounded** across many decoded thumbs (no LRU/eviction); ~600 KB each at current size.
4. Decode errors are silent (`Err(())`); non-RAW files are listed and show the missing icon.
5. **Grain/noise/sharpness**: area-average downscale, luminance collapse (~√3 SNR, kills casts on mono), full-res bayer flattening (no demosaic smear) and a gentle unsharp mask are done; remaining stackable option — a stronger denoise pass for very underexposed captures. Capture-side: scans exposed with the clear-film base far left in the histogram waste SNR; exposing so the base sits near saturation (~85–95%) keeps several more stops of detail in dense areas (user's current habit trades this away deliberately for highlight headroom). Watch item: if a faint 2px-period pattern ever shows in smooth areas, suspect per-class base drift in `flatten_bayer`.

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
