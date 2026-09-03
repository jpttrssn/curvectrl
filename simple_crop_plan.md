# Simple Crop Tool — Implementation Plan

A keyboard-driven crop tool for removing scanned edges outside the actual negative,
preserving the image's natural aspect ratio, with auto-selected anchor points.

## Design model

Crop = four **source-pixel margins** `{top, right, bottom, left}` derived from a
**single scalar amount `R`** plus a **directional target** (edge). Aspect ratio is
always preserved and the anchor is auto-selected. Margins are persisted in
`.film-roll.toml` and reflected everywhere:

- **Grid thumbnails** — crop baked into the decode.
- **Detail view** — live GPU uniform UV-remap (no re-decode), matching the
  exposure/curve live-edit idiom.

`R` is in **source image pixels** (raw photosites), applied at decode time, so a
1px crop is 1 real sensor pixel regardless of on-screen scale.

### Margin math (pure `crop_margins(direction, amount_px, w, h)`)

For source `W×H`, `ar = W/H`, amount `R` (clamped to the direction's valid extent):

| Direction | Anchor     | margins `(t, r, b, l)`     |
|-----------|------------|----------------------------|
| Bottom    | top-center | `(0, arR/2, R, arR/2)`     |
| Top       | bottom-center | `(R, arR/2, 0, arR/2)`  |
| Right     | left-center | `(R/(2ar), R, R/(2ar), 0)` |
| Left      | right-center | `(R/(2ar), 0, R/(2ar), R)` |

Every row satisfies `(W−r−l)/(H−t−b) == ar`, so the cropped frame keeps the
source's ratio. Corners are built by pressing two edge keys sequentially; there
are **no explicit corner targets** (4 edges only).

## Keybindings (final)

Active only while a detail view is open (inert otherwise). Bare letters
`h/j/k/l` — no conflicts with the existing bare editing keys (`- / = [ ] ; ' , .`)
or the nav arrows (which are `Named`).

- `h` = Left, `j` = Bottom, `k` = Top, `l` = Right
- **Shrink (trim more)**: edge key alone → step `+2 px`
- **Grow (trim less)**: `Alt` + edge → step `−2 px` (clamped to 0)
- **Nudge (fine)**: `Shift` (+ optional `Alt`) + edge → `±1 px`
- Anchor auto-detected per edge; aspect ratio always preserved.

Modifiers read from `modifiers.alt()` / `modifiers.shift()` on the
`keyboard::listen` event (`modifiers.alt()` confirmed present on the pinned
libcosmic commit `ef490df`).

## Persistence & copy/paste

- `crop: CropMargins` added to `EditData` (`#[serde(default)]`, older manifests
  load as no-crop). Manifest `version` bumped 3 → 4.
- Crop is **NOT** part of `ToneEdit` (the type threaded through copy/paste,
  `clipboard: Option<ToneEdit>`), so `CopyEdits`/`PasteEdits` inherently exclude
  crop. Read/written via dedicated `RollManifest::crop(name)` /
  `set_crop(name, ..)` / `set_crop_amount(name, direction, amount, w, h)`.
- `set_tone` (paste path) writes only exposure + curve, leaving an existing
  target's crop untouched.

## Incremental steps

### Step 1 — Pure crop-domain module + margins math
`src/edit_manifest.rs` (types: `CropMargins`, `CropDirection`) + `src/app.rs`
(pure `crop_margins` helper, clamped). Unit tests: aspect preserved per
direction, anchor pinned, zero amount → zero margins, clamp caps R.

### Step 2 — Persistence but not in ToneEdit
`src/edit_manifest.rs`: add `crop` to `EditData`; dedicated `crop`/`set_crop`/
`set_crop_amount` on `RollManifest`; leave `ToneEdit` untouched; bump version.
Unit tests: crop round-trips, legacy manifest → zero margins, `set_tone` does
not alter crop, crop coexists with exposure/curve.

### Step 3 — Live GPU crop (uniform UV-remap)
`src/shader/exposure.wgsl`, `src/shader.rs`, `src/app.rs`: four `f32` crop
uniforms (`crop_l/t`, `crop_w/h`); `view_uv` maps within the cropped sub-rect
using cropped dims for contain/center. `DetailProgram` + `set_crop`. New
`Message::CropChanged` + `AppModel` crop state loaded per detail open. naga WGSL
validation + pure `crop_uv_geometry` unit-tested.

### Step 4 — Keyboard bindings + direction state
`src/app.rs`: `EditAdjust::Crop { direction, delta }`; `edit_adjust_for`
signature becomes `(key, alt, shift)`; wire `h/j/k/l` arms. Plain key = ±2 px,
Shift = ±1 px, clamped ≥ 0; no-op unless detail view open. Unit tests for the
key→adjust mapping and clamping.

### Step 5 — Thumbnail + cover re-bake
`src/app.rs`: bake crop into `convert_thumbnail` (and `decode_raw_detail`) by
cropping the decoded mono to margins before downscale/unsharp. Read crop via
`roll.crop(name)` (separate from `tone()`). Re-bake selected tile + cover on
crop change. Unit tests: cropped synthetic mono dims, thumbnail honors crop,
zero crop byte-identical.

### Step 6 — Editing panel readout + reset
`src/app.rs`, `i18n/en/curvectrl.ftl`: crop readout + arm hint in drawer;
`ResetAll` restores crop to panel-open snapshot (`reset_crop`).

### Step 7 — Docs + verify
`NOTES.md`: update working-state / architecture / behavior bullets, bump test
count. Run `cargo test --locked` + `just check`.

## Validation

Each step compiles, runs `cargo test --locked`, and passes `just check`
(clippy pedantic). Logic is pure + unit-tested in `app.rs`/`edit_manifest.rs`,
matching the repo's established test culture.
