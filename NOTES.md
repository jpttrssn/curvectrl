# NOTES — thumbnail listing & RAW rendering (working state)

Reference for continuing work on Page 1 thumbnails. All code in `src/app.rs`.

## Current behavior

- Page 1 lists every regular file in `$HOME/Pictures/exposure` as tiles in a responsive grid, sorted by name. Scanned once at startup.
- Columns adapt to window width; cells never exceed `THUMB_SIZE` px wide and stay square (`Grid::fluid` + `Sizing::AspectRatio(1.0)`). Previously a fixed 5-per-row manual layout.
- Each tile: image area (fills its square cell, `ContentFit::Contain`) + file name below. `image-loading-symbolic` / `image-missing-symbolic` icons for pending/failed.
- Thumbs are rendered with a real color pipeline: per-channel level normalize → half-res true-color demosaic (any valid CFA incl. X-Trans; gray box-average fallback) → in-file WB gains → sRGB transfer curve. cpp≥3 sources pass through + tone curve only.
- Thumbs decode progressively, one at a time, in file order.

## Architecture (all in src/app.rs)

- State: `tiles: Vec<Tile>`; `Tile { name: String, thumb: Thumb }`; `Thumb::{Loading, Ready(Handle), Failed}`.
- Messages: `FilesLoaded(Vec<String>)` builds tiles then starts chain; `ThumbReady(String, Result<Handle, ()>)` stores result and chains next.
- `decode_next()` picks first `Loading` tile → one `tokio::task::spawn_blocking` per file (`decode_thumbnail`). Strictly sequential so only one full-res RAW buffer is alive at a time (~50–100 MB peak).
- Pipeline: `rawloader::decode_file` → `normalize_samples` (per-CFA-position black/white levels for Integer data; max-gain for Float) → bayer: `demosaic_half` (2×2 blocks accumulated via `cfa.color_at`; `None` → legacy gray box-average) + `apply_white_balance` (`wb_coeffs` normalized against green); cpp≥3: passthrough → `srgb_encode` on all values → u8 quantize → nearest-neighbor downscale to `THUMB_SIZE` (`resize_nearest`) → orientation fix (`orient`, all rawloader variants) → `Handle::from_rgba`.
- Constants: `THUMB_SIZE = 384.0` (f32; decode cap + grid cell width target), `TILE_ASPECT = 1.0`.
- View: Page 1 builds `cosmic::iced::widget::Grid::with_children(tiles.map(tile_view))` with `.fluid(THUMB_SIZE)` + `.height(grid::Sizing::AspectRatio(TILE_ASPECT))`, wrapped in a scrollable. `tile_view()` fills the square cell the Grid assigns (Grid constrains each child to exact cell w×h); no manual rows/padding cells, no aspect-ratio widget.
- iced's Grid (re-exported at `cosmic::iced::widget::{Grid, grid}`) differs from cosmic's own `widget::grid` (taffy-based, no column control — rejected earlier).

## Known limitations (likely next-step targets)

1. **Color accuracy**: pipeline now does WB + demosaic + sRGB, but no `xyz_to_cam` color matrix conversion (hues are approximate camera-RGB), and cpp≥3 DNGs get gamma only (assumed already WB'd/demosaiced). Full accuracy would need the camera matrix + proper output transform.
2. **No refresh** of the folder listing after startup.
3. **Memory unbounded** across many decoded thumbs (no LRU/eviction); ~600 KB each at current size.
4. Decode errors are silent (`Err(())`); non-RAW files are listed and show the missing icon.

## Environment gotchas

- `libcosmic` is a plain git dependency on pop-os master (lockfile pins `ef490df5`); the local clone at `../cosmic/libcosmic` is no longer patched in. Upstream includes the old `AspectRatio::layout` off-by-one fix and iced's `Grid` export. The commented `[patch]` template in `Cargo.toml` can restore the clone if needed.
- `patch.rs` at repo root: standalone repro + PR draft for the (now-fixed upstream) AspectRatio bug (not part of the app build).
- Verify changes with `just check` (clippy pedantic). Expect ~130 warnings from compiling patched libcosmic from source — ignore those; only warnings pointing into `src/app.rs` matter.
- No test suite exists.

## Useful API facts (verified against pinned libcosmic/rawloader)

- `cosmic::Task<M>` aliases `iced::Task<cosmic::Action<M>>`; async work: `cosmic::task::future(async { Message::X(...) })`; batch with `Task::batch([...])`.
- `rawloader::{RawImage, RawImageData, Orientation}` are re-exported at crate root (`decoders` module is private).
- `RawImage` fields used here: `width`, `height`, `cpp`, `data`, `wb_coeffs` ([f32;4] RGBE), `blacklevels`/`whitelevels` ([u16;4]), `orientation`, `cfa`.
- `rawloader::CFA` is crate-exported; `CFA::new("RGGB")` builds test patterns; `color_at(row, col)` mods internally (`(row+48)%48`), so any coordinates are safe. Returns 0=R, 1=G, 2=B, 3=fourth-color.
- Icons: `icon::from_name("...").icon().into()` yields an Element; image handles clone cheaply.
