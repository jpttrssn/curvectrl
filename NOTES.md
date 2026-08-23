# NOTES — thumbnail listing & RAW rendering (working state)

Reference for continuing work on Page 1 thumbnails. All code in `src/app.rs`.

## Current behavior

- Page 1 lists every regular file in `$HOME/Pictures/exposure` as tiles in a responsive grid, sorted by name. Scanned once at startup.
- Columns adapt to window width; cells never exceed `THUMB_SIZE` px wide and stay square (`Grid::fluid` + `Sizing::AspectRatio(1.0)`). Previously a fixed 5-per-row manual layout.
- Each tile: image area (fills its square cell, `ContentFit::Contain`) + file name below. `image-loading-symbolic` / `image-missing-symbolic` icons for pending/failed.
- Thumbs decode progressively, one at a time, in file order.

## Architecture (all in src/app.rs)

- State: `tiles: Vec<Tile>`; `Tile { name: String, thumb: Thumb }`; `Thumb::{Loading, Ready(Handle), Failed}`.
- Messages: `FilesLoaded(Vec<String>)` builds tiles then starts chain; `ThumbReady(String, Result<Handle, ()>)` stores result and chains next.
- `decode_next()` picks first `Loading` tile → one `tokio::task::spawn_blocking` per file (`decode_thumbnail`). Strictly sequential so only one full-res RAW buffer is alive at a time (~50–100 MB peak).
- Pipeline in `convert_thumbnail`: `rawloader::decode_file` → normalize samples to 0..=1 (Integer via black/white levels; Float via max gain) → RGB passthrough if `cpp >= 3`, else 2×2 box-average reduction of bayer/mono → orientation fix (`orient`, all rawloader variants) → nearest-neighbor downscale to `THUMB_SIZE` (`resize_nearest`) → `Handle::from_rgba`.
- Constants: `THUMB_SIZE = 384.0` (f32; decode cap + grid cell width target), `TILE_ASPECT = 1.0`.
- View: Page 1 builds `cosmic::iced::widget::Grid::with_children(tiles.map(tile_view))` with `.fluid(THUMB_SIZE)` + `.height(grid::Sizing::AspectRatio(TILE_ASPECT))`, wrapped in a scrollable. `tile_view()` fills the square cell the Grid assigns (Grid constrains each child to exact cell w×h); no manual rows/padding cells, no aspect-ratio widget.
- iced's Grid (re-exported at `cosmic::iced::widget::{Grid, grid}`) differs from cosmic's own `widget::grid` (taffy-based, no column control — rejected earlier).

## Known limitations (likely next-step targets)

1. **Color/tone**: thumbs are linear-light level-normalized only — no white balance, no real demosaic, no tone curve. Bayer files look gray/washed; film scans (cpp=3 DNG) look dark. Proper pipeline would need WB coefficients + demosaic + gamma/sRGB curve (candidate crate: `rawler`).
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
- `RawImage` fields used here: `width`, `height`, `cpp`, `data`, `blacklevels`/`whitelevels` ([u16;4]), `orientation`.
- Icons: `icon::from_name("...").icon().into()` yields an Element; image handles clone cheaply.
