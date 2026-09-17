// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::Config;
use crate::detail_area::DetailArea;
use crate::edit_manifest::{self, RollManifest};
use crate::exif_writer;
use crate::film::{
    ACTIVE_STOCK, BaseConfig, FilmPreset, MIN_PLAUSIBLE_BASE, MonoStock, invert_gray, measure_base,
};
use crate::fl;
use crate::shader;
use cosmic::Application;
use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::futures::SinkExt;
use cosmic::iced::keyboard;
use cosmic::iced::widget::scrollable::Viewport;
use cosmic::iced::widget::{Grid, MouseArea, Stack, grid};
use cosmic::iced::{ContentFit, Length, Point, Size, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, icon, image::Handle, menu, toaster};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const APP_ICON: &[u8] =
    include_bytes!("../resources/icons/hicolor/scalable/apps/io.github.jpttrssn.curvectrl.svg");

/// Maximum dimension of decoded RAW thumbnails, also the maximum Page 1 tile width.
const THUMB_SIZE: f32 = 384.0;
/// Maximum dimension of the hi-res decode displayed in the detail view.
///
/// Sharp at typical window sizes while keeping the decoded buffer a fraction
/// of a full sensor frame.
const HI_RES_SIZE: u32 = 2048;
/// Opacity units per second for the hi-res crossfade (~0.2 s full ramp).
const FADE_SPEED: f32 = 5.0;
/// Aspect ratio (width / height) of the image area of a Page 1 tile.
const TILE_ASPECT: f32 = 1.0;

/// Maximum number of thumbnail decodes in flight at once.
///
/// Decodes run on the blocking thread pool, so several proceed in parallel on
/// multicore machines. The bound keeps the transient footprint (each in-flight
/// decode holds one full-resolution RAW buffer until it finishes downscaling)
/// to a few dozen hundred MB, not the whole roll.
const MAX_CONCURRENT_THUMBS: usize = 4;

/// Number of detail-view overview (2048px) mono buffers to cache most-recently
/// used. Sized to cover a typical film roll (~40 frames). Each entry is the
/// fixed 2048 overview decode (~12–17 MB), so the worst case is bounded at
/// ~600 MB; native zoom level-up decodes are never cached.
const DETAIL_CACHE_CAPACITY: usize = 40;

/// Maximum number of neighbor detail preload decodes in flight at once.
/// Preloads run on their own bounded channel, separate from the single
/// critical detail slot, so they never starve the currently-viewed frame.
const MAX_CONCURRENT_PRELOADS: usize = 2;

/// How many frames on each side of the currently-viewed frame to preload into
/// the overview LRU cache (so Left/Right paging to a neighbor is instant).
const DETAIL_PRELOAD_DISTANCE: usize = 1;

/// Keyboard shortcut step for exposure (EV) when an edit control is adjusted
/// with a bare key.
const EDIT_STEP_EV: f32 = 0.50;
/// Keyboard shortcut nudge step for exposure (EV) with the Shift modifier.
const EDIT_NUDGE_EV: f32 = 0.05;
/// Keyboard shortcut step for a tone-curve power (contrast/rolloff/shadows)
/// with a bare key.
const EDIT_STEP_CURVE: f32 = 0.20;
/// Keyboard shortcut nudge step for a tone-curve power with the Shift modifier.
const EDIT_NUDGE_CURVE: f32 = 0.05;
/// Keyboard shortcut step for a crop trim: a bare edge key removes this many
/// source pixels from the edge (positive = trim more / shrink the frame).
const CROP_STEP_PX: i32 = 2;
/// Keyboard shortcut nudge step for a crop trim with the Shift modifier (a
/// 1px fine adjustment).
const CROP_NUDGE_PX: i32 = 1;

/// Maximum detail-view zoom in `log2` units: 1.0 = contain fit, each +1
/// doubles the rendered scale, so this caps at 2^6 = 64× the fit scale.
const MAX_DETAIL_ZOOM: f32 = 7.0;

/// Detail-view zoom (log2 units) at which the native hi-res decode is
/// triggered. 2.0 = 2× contain: safely past the 2048 overview's own 1:1 on
/// typical widgets, before the view has upscaled 2048 pixels far enough to
/// look soft.
const NATIVE_ZOOM_THRESHOLD: f32 = 2.0;

/// wgpu's typical `max_texture_dimension_2d` ceiling. The native level-up
/// caps its target at this so a >8K sensor never asks for an oversized upload.
const MAX_TEXTURE_EDGE: u32 = 8192;

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// Display a context drawer with the designated page if defined.
    context_page: ContextPage,
    /// Per-view context-drawer open/closed memory (see [`DrawerMemory`]).
    drawer_memory: DrawerMemory,
    /// The about page for this app.
    about: About,
    /// Key bindings for the application's menu bar, consumed by
    /// `cosmic::widget::menu::items` (values are unit [`MenuAction`]s, hence the
    /// allow: the map shape is imposed by the menu API, not by choice).
    #[allow(clippy::zero_sized_map_values)]
    key_binds: HashMap<menu::KeyBind, MenuAction>,
    /// Configuration data that persists between application runs.
    config: Config,
    /// Film rolls listed on the library page, one directory of negatives each.
    rolls: Vec<Roll>,
    /// Directory of the roll currently drilled into (its frame grid and the
    /// detail view). `None` shows the library page of rolls.
    active: Option<PathBuf>,
    /// The library cell single-clicked (or arrow-key selected) on the library
    /// page: either the always-first Add Roll tile or a real roll. `None`
    /// (initial state) until the user selects a cell; drives the selected-tile
    /// highlight, the `Space` metadata drawer, and Enter/arrow navigation.
    library_selection: Option<LibrarySelection>,
    /// Frame-grid highlight: the tile single-clicked (or last opened / paged
    /// to) inside an open roll. Distinct from `selected` (the opened detail
    /// frame) so the highlight survives closing the detail view and drives the
    /// accent ring, Enter, and arrow-key navigation. Also the "primary" of any
    /// multi-selection: it is always a member of `selected_frames`.
    frame_selected: Option<String>,
    /// Multi-selected frames on the open roll's grid. A plain click (or the
    /// primary `frame_selected`) always lands here; Ctrl+click toggles a frame
    /// in/out, Shift+click selects the range from the anchor through the
    /// clicked frame, and Ctrl+A selects everything in the filtered set. Used
    /// as the batch target for copy/paste edits.
    selected_frames: HashSet<String>,
    /// Last frame used as the Shift+click range anchor within the filtered set.
    selection_anchor: Option<String>,
    /// Whether the Ctrl modifier is currently held. Tracked from the global
    /// keyboard subscription so a frame click can distinguish a plain click
    /// (clear + select) from a Ctrl+click (toggle a frame in/out of the
    /// multi-selection), since iced's `MouseArea` delivers no modifier info.
    ctrl_down: bool,
    /// Whether the Shift modifier is currently held. Drives Shift+click range
    /// selection (and whether control shortcuts activate). Tracked from the
    /// global keyboard subscription like `ctrl_down`.
    shift_down: bool,
    /// Whether an editing shortcut key (an `AdjustEdit` character) is currently
    /// held. Keyboard steps mutate the live preview (RAM + shader) on every
    /// press/repeat like a slider drag, and commit once when the key is
    /// released — mirroring the slider's drag/release cadence instead of
    /// persisting + re-baking the thumbnail per auto-repeated press.
    editing_key_held: bool,
    /// Number of columns the library grid last laid out (tracked from window
    /// resizes, matching iced's `Grid::fluid` math), so Up/Down keyboard
    /// navigation can jump by exactly one row.
    grid_cols: usize,
    /// The most recent viewport of whichever grid is mounted (library or frame
    /// page — they are mutually exclusive). Drives keyboard scroll-into-view:
    /// knowing the visible height, content height, and current translation lets
    /// `Nav` reveal the highlighted tile precisely. Cleared when the page
    /// changes; re-captured by each grid's `on_scroll`.
    grid_viewport: Option<Viewport>,
    /// The window height from the last resize, used only to estimate the grid
    /// viewport height before the first real scroll has been observed.
    window_height: f32,
    /// Names handed to the bounded in-flight roll-cover decodes, so re-baked
    /// roll tiles never double-spawn against the startup chain (memory bound).
    cover_inflight: Vec<PathBuf>,
    /// Search state: `None` hides the search input (the header shows only the
    /// search icon); `Some(term)` shows the input, which filters roll names on
    /// the library page and frame names inside an open roll. An empty term
    /// keeps the input open but matches everything — echoing cosmic-files, the
    /// mere presence of the input is the toggle, not the text.
    search: Option<String>,
    /// File entries from the open roll, displayed as tiles on its frame grid.
    tiles: Vec<Tile>,
    /// File shown enlarged in the detail view in place of the grid, if any.
    selected: Option<String>,
    /// Names handed to the bounded in-flight thumbnail decodes, so re-baked
    /// tiles never double-spawn against the startup chain (memory bound).
    thumb_inflight: Vec<String>,
    /// The frame whose EXIF parse is currently in flight for the frame-info
    /// drawer, so a highlight change to the same frame does not double-spawn.
    frame_meta_inflight: Option<String>,
    /// Persisted per-file edits for the film roll, loaded at startup and
    /// reconciled against the files on disk on each scan. Writes to the
    /// manifest happen only on explicit flush messages, never per frame.
    roll: RollManifest,
    /// GPU shader program for the detail view, rendering mono data with
    /// live exposure adjustment.  `None` while the decode is in flight.
    detail_shader: Option<shader::DetailProgram>,
    /// File handed to the one permitted in-flight hi-res detail decode.
    detail_inflight: Option<String>,
    /// True once the native-resolution decode has been requested or found
    /// unnecessary (sensor already ≤ the overview size); guards the zoom
    /// trigger against re-spawning a level-up on every wheel event.
    detail_native_queued: bool,
    /// Current opacity of the thumbnail layer during crossfade (1.0→0.0).
    detail_thumb_opacity: f32,
    /// Timestamp of the last animation frame for framerate-independent fading.
    detail_last_frame: Option<Instant>,
    /// Cached thumbnail kept visible over the shader during crossfade.
    detail_thumb: Option<Handle>,
    /// Detail-view zoom in `log2` units: 1.0 = contain fit (whole frame),
    /// each +1 doubles the rendered scale.
    detail_zoom: f32,
    /// The detail preview's laid-out logical size, reported by `DetailArea`
    /// via `DetailAreaResized`; drives the 1:1 ("100%") zoom cap.
    detail_area_size: Option<Size>,
    /// Pan offset of the image center from the widget center (logical points).
    detail_pan: (f32, f32),
    /// True while the user is pressing/dragging the detail preview (grab-pan).
    detail_panning: bool,
    /// Most recent cursor position over the detail preview, widget-relative
    /// logical points; anchors wheel-zoom at the cursor.
    detail_cursor: Option<Point>,
    /// Contrast power previewed in the detail view, pivoting the live tone
    /// curve at the image's measured mid-gray. Non-persisted: resets to
    /// `1.0` (identity) on every detail open. GPU-uniform only — the grid
    /// thumbnails always render with the default curve.
    curve_contrast: f32,
    /// Highlight-rolloff power previewed in the detail view, pivoting the
    /// live tone curve at the image's measured white point. Same lifecycle
    /// and rules as [`Self::curve_contrast`].
    curve_rolloff: f32,
    /// Shadows power previewed in the detail view, pivoting the live tone
    /// curve at the image's measured 10th-percentile shadow anchor. Same
    /// lifecycle and rules as [`Self::curve_contrast`].
    curve_shadows: f32,
    /// Exposure compensation in EV (−3.00 to +3.00).
    exposure_ev: f32,
    /// Live keyboard crop margins for the detail view; loaded from the stored
    /// manifest per open and applied to the GPU shader as a uniform UV-remap.
    crop: edit_manifest::CropMargins,
    /// Live display rotation for the detail view: cumulative counter-clockwise
    /// 90° quarter-turns (`0`…`3`) on top of the EXIF orientation. Loaded from
    /// the stored manifest per open and applied to the GPU shader as a uniform.
    rotation: u8,
    /// Draft text for the four crop margin fields in the editing drawer. Kept
    /// as strings (not the committed margins) so typing doesn't fight the
    /// read-only view; committed to `crop` on each field's submit.
    crop_drafts: CropDrafts,
    /// Draft text for the two roll date fields in the roll-info drawer, seeded
    /// from the selected roll's committed dates on selection change and
    /// committed back on submit.
    roll_date_drafts: RollDateDrafts,
    /// Whether the detail view's "view dimmed crop area" overlay is shown:
    /// the full uncropped frame drawn on top of the zoomed crop with everything
    /// outside the crop dimmed. View-only — never persisted.
    show_crop_mask: bool,
    /// Edit values as they were when the detail panel was opened — the stored
    /// manifest values for the selected file. `ResetAll` restores these, not
    /// the identity, so reset reverts the panel to its opened state.
    reset_exposure_ev: f32,
    reset_curve_contrast: f32,
    reset_curve_rolloff: f32,
    reset_curve_shadows: f32,
    /// Crop as it was when the detail panel was opened; `ResetAll` restores it.
    reset_crop: edit_manifest::CropMargins,
    /// Rotation as it was when the detail panel was opened; `ResetAll`
    /// restores it. Tracked separately from [`Self::rotation`]'s zeroing in
    /// [`Self::clear_detail`] like the other reset snapshots.
    reset_rotation: u8,
    /// The last edit copied for paste (Ctrl+C), as a full [`ToneEdit`]. `None`
    /// until the user copies — a paste with nothing copied is a no-op.
    clipboard: Option<edit_manifest::ToneEdit>,
    /// Monotonic counter incremented each time a new detail decode finishes;
    /// stamped into [`DetailProgram::image_id`] so the GPU pipeline
    /// recognises a new image and rebuilds its texture.
    next_image_id: u64,
    /// LRU of decoded detail overviews keyed by (roll dir, file name), so
    /// returning to a recently-viewed frame doesn't re-decode the RAW. Survives
    /// roll switches and detail close; eviction is global (see
    /// [`DETAIL_CACHE_CAPACITY`]). Only overview (2048px) buffers are stored.
    detail_cache: LruCache<(PathBuf, FilmPreset, String), DetailMono>,
    /// (roll dir, file name) handed to the bounded neighbor preload decodes,
    /// so a frame already being preloaded (or already cached) is never spawned
    /// twice. Independent of the single critical detail slot.
    detail_preload_inflight: Vec<(PathBuf, String)>,
    /// Completion toasts shown over the window (e.g. the export summary).
    /// Auto-dismissing: each toast is removed when its [`Message::ToastClose`]
    /// fires after the toast's duration.
    toasts: toaster::Toasts<Message>,
    /// Whether the native folder dialog is currently open. The portal dialog is
    /// modal and blocks this window's input, but a second `ExportRequested`
    /// could still fire during the async gap before the dialog appears, so a
    /// flag prevents stacking two dialogs.
    export_pending: bool,
    /// Live export progress as `(done, total)` frames, or `None` while no batch
    /// is running. Drives the header's circular progress ring; the stream task
    /// clears it via [`Message::ExportDone`] when the batch finishes.
    export_progress: Option<(usize, usize)>,
}

/// Draft text (source-pixel strings) for the four crop margin fields in the
/// editing drawer, one per edge. Initialized from the committed margins on
/// open; updated by typing; committed back into `AppModel.crop` on submit.
/// Holds a `String` (never a parsed margin) so an in-progress edit stays
/// stable while the read-only view re-renders.
#[derive(Debug, Clone)]
struct CropDrafts {
    top: String,
    right: String,
    bottom: String,
    left: String,
}

impl CropDrafts {
    /// Fresh drafts from a set of committed margins.
    fn from_margins(crop: edit_manifest::CropMargins) -> Self {
        Self {
            top: crop.top.to_string(),
            right: crop.right.to_string(),
            bottom: crop.bottom.to_string(),
            left: crop.left.to_string(),
        }
    }

    /// The draft for a given edge, updated in place (used for typing).
    fn set(&mut self, direction: edit_manifest::CropDirection, value: String) {
        use edit_manifest::CropDirection::{Bottom, Left, Right, Top};
        match direction {
            Top => self.top = value,
            Right => self.right = value,
            Bottom => self.bottom = value,
            Left => self.left = value,
        }
    }

    /// The draft for a given edge.
    fn get(&self, direction: edit_manifest::CropDirection) -> &str {
        use edit_manifest::CropDirection::{Bottom, Left, Right, Top};
        match direction {
            Top => &self.top,
            Right => &self.right,
            Bottom => &self.bottom,
            Left => &self.left,
        }
    }
}

/// Which roll date field a draft/commit message targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RollDateField {
    Start,
    End,
}

/// Draft ISO date text for the two roll-info drawer date fields (start and
/// optional end). Seeded from the selected roll's committed dates; updated by
/// typing; committed back to the roll and its manifest on submit. Holds plain
/// `String`s so an in-progress edit stays stable while the read-only view
/// re-renders. `key` is the roll directory the drafts were seeded from, so a
/// changed library selection (or a fresh drawer) reseeds on the next sync.
#[derive(Debug, Clone)]
struct RollDateDrafts {
    /// The roll directory the drafts were seeded from (`None` = never seeded).
    key: Option<PathBuf>,
    start: String,
    end: String,
}

impl RollDateDrafts {
    /// Fresh drafts from a roll's committed dates.
    fn from_dates(key: PathBuf, start: Option<&str>, end: Option<&str>) -> Self {
        Self {
            key: Some(key),
            start: start.unwrap_or("").to_owned(),
            end: end.unwrap_or("").to_owned(),
        }
    }

    /// The draft for a given field, updated in place (used for typing).
    fn set(&mut self, field: RollDateField, value: String) {
        match field {
            RollDateField::Start => self.start = value,
            RollDateField::End => self.end = value,
        }
    }

    /// The draft for a given field.
    fn get(&self, field: RollDateField) -> &str {
        match field {
            RollDateField::Start => &self.start,
            RollDateField::End => &self.end,
        }
    }
}

/// The selectable cell on the library page: either the leading Add Roll tile
/// (shown only while no search is active) or a real roll. Modeling both with a
/// single type makes the selection, highlight, keyboard navigation, and the
/// open action uniform across the grid — the add tile is selected and Entered
/// exactly like a roll card.
#[derive(Debug, Clone, PartialEq)]
enum LibrarySelection {
    /// The Add Roll tile (grid cell 0 while no search is active). Enter /
    /// double-click opens the folder picker; it has no directory or metadata
    /// of its own.
    AddRoll,
    /// A real roll card, by its directory.
    Roll(PathBuf),
}

/// A film roll: a user-chosen directory of negatives, listed as a cover tile
/// on the library page. Double-clicking (or Enter on the selected roll) drills
/// into its frame grid.
#[derive(Debug, Clone)]
pub struct Roll {
    /// Absolute directory holding this roll's negatives.
    pub dir: PathBuf,
    /// Display name (the directory's final component).
    pub name: String,
    /// The roll's cover file name (first sorted non-dot file), if any.
    pub cover: Option<String>,
    /// Number of regular non-dot files in the roll directory, surfaced in the
    /// roll-info metadata drawer.
    pub frame_count: usize,
    /// The film-inversion preset this roll's frames render with. The single
    /// in-memory source of truth for decodes; persisted to the roll's edit
    /// manifest (see [`edit_manifest::RollManifest::preset`]).
    pub preset: FilmPreset,
    /// The roll's start date (ISO `YYYY-MM-DD`), if already set. Mirrors the
    /// edit manifest; display-only (no decode impact).
    pub start_date: Option<String>,
    /// The roll's optional end date (ISO `YYYY-MM-DD`), if already set.
    pub end_date: Option<String>,
    /// Decoded cover thumbnail state.
    pub thumb: Thumb,
}

/// Basic EXIF readout for the frame-info drawer, parsed lazily from the RAW
/// file on demand and cached on its [`Tile`]. Every field is an already
/// formatted display string (`None` = absent in the file).
#[derive(Debug, Clone, Default)]
pub(crate) struct FrameMeta {
    width: Option<String>,
    height: Option<String>,
    make: Option<String>,
    model: Option<String>,
    iso: Option<String>,
    exposure: Option<String>,
    aperture: Option<String>,
    focal: Option<String>,
    lens: Option<String>,
    date: Option<String>,
}

/// A file entry displayed as a tile on the open roll's frame grid.
struct Tile {
    /// File name.
    name: String,
    /// Decoded thumbnail state.
    thumb: Thumb,
    /// Lazy EXIF/dimension readout for the frame-info drawer (parsed once per
    /// session); `None` until the first drawer request.
    meta: Option<FrameMeta>,
    /// Set when the file's metadata could not be parsed, so a failed read is
    /// not re-attempted on every drawer open.
    meta_failed: bool,
}

/// A decoded detail-view overview: the linear pre-sRGB mono buffer plus its
/// geometry, as delivered by [`decode_raw_detail`]. Exactly what an
/// [`shader::DetailProgram`] needs to (re)build without re-decoding
/// the RAW. Cached by the detail LRU keyed on (roll dir, film preset, file
/// name), so a preset change never serves a stale inversion.
#[derive(Debug, Clone)]
struct DetailMono {
    mono: Vec<f32>,
    width: u32,
    height: u32,
    /// The sensor's true long edge AFTER cropping but BEFORE the downscale, so
    /// a served cache entry can decide whether the overview was already native.
    src_long_edge: u32,
    /// Film-negative inversion carried through the cache: `Some((stock, base))`
    /// when the frame is a negative the shader density-inverts per fragment
    /// (`base` is the per-frame clear-film transmission measured from the
    /// sensor mono), `None` for an already-positive scan. Rebuilds an identical
    /// shader from a cache hit without re-decoding the RAW.
    inversion: Option<(MonoStock, f32)>,
}

/// A decoded true sensor-linear mono frame plus the data the detail and export
/// paths need to shape it.
///
/// `mono` is linear `[0,1]` relative to the sensor white point for EVERY preset
/// (an already-positive scan or a film negative) — the single source of truth;
/// the exposure gain touches it, and per-fragment shaping (density inversion
/// for film) happens downstream per preset.
#[derive(Debug, Clone)]
pub(crate) struct DetailDecode {
    mono: Vec<f32>,
    width: u32,
    height: u32,
    /// The sensor's true long edge AFTER cropping but BEFORE the downscale.
    src_long_edge: u32,
    /// `None` for an already-positive scan; `(stock, base)` for a film negative
    /// the shader must density-invert (`base` is the clear-film anchor).
    inversion: Option<(MonoStock, f32)>,
}

/// A fixed-capacity least-recently-used map keyed by (roll dir, film preset,
/// file name).
///
/// Backed by a `HashMap` for O(1) lookup plus a `VecDeque` of keys as the
/// recency index: `get` moves the key to the back, `insert` pops the front
/// (least-recent) key once the capacity is exceeded and hands back the evicted
/// value so the caller can release (drop) its memory. Dependency-free and pure,
/// so the eviction order is unit-tested.
struct LruCache<K, V> {
    map: std::collections::HashMap<K, V>,
    order: std::collections::VecDeque<K>,
    capacity: usize,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + std::hash::Hash + Clone,
{
    fn new(capacity: usize) -> Self {
        LruCache {
            map: std::collections::HashMap::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns the cached value for `key`, marking it most-recently used.
    fn get(&mut self, key: &K) -> Option<&V> {
        if self.map.contains_key(key) {
            self.bump(key.clone());
        }
        self.map.get(key)
    }

    /// Inserts `value` under `key`, marking it most-recently used. Once the
    /// capacity is exceeded the least-recently-used entry is evicted and its
    /// value returned (so the caller can drop it); `None` when nothing was
    /// evicted. With a zero capacity every insert is immediately evicted and
    /// returned, so the cache stays empty.
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        if self.capacity == 0 {
            return Some(value);
        }
        let evicted = if !self.map.contains_key(&key) && self.map.len() >= self.capacity {
            self.order
                .pop_front()
                .and_then(|oldest| self.map.remove(&oldest))
        } else {
            None
        };
        self.map.insert(key.clone(), value);
        self.bump(key);
        evicted
    }

    fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Drops every entry whose key no longer passes `keep`, returning the
    /// removed values so the caller can release (drop) them eagerly. The
    /// recency order of the surviving entries is preserved. Used to clear one
    /// roll's cached overviews when its film preset changes.
    fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) -> Vec<V> {
        let mut dropped = Vec::new();
        self.order.retain(|key| {
            if keep(key) {
                true
            } else {
                if let Some(value) = self.map.remove(key) {
                    dropped.push(value);
                }
                false
            }
        });
        dropped
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }

    /// Moves `key` to the back of the recency order (most-recently used). If it
    /// was not present, keeps the order consistent by removing any duplicate.
    fn bump(&mut self, key: K) {
        if let Some(position) = self.order.iter().position(|k| k == &key) {
            self.order.remove(position);
        }
        self.order.push_back(key);
    }
}

/// Thumbnail loading state of a [`Tile`] or roll cover.
#[derive(Debug, Clone)]
pub enum Thumb {
    /// The file has not been decoded yet.
    Loading,
    /// A decoded RGBA thumbnail.
    Ready(Handle),
    /// The file could not be decoded.
    Failed,
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum Message {
    /// Close the detail view, returning to the grid.
    DetailClosed,
    /// A hi-res decode for the detail view finished, returning the true
    /// sensor-linear mono buffer for the GPU shader. Carries the preset the
    /// decode ran under so the LRU is keyed consistently with the buffer
    /// contents.
    DetailReady(String, FilmPreset, Result<DetailDecode, ()>),
    /// A neighbor preload decode finished. Unlike [`Message::DetailReady`] this
    /// only lands into the detail LRU cache; it never becomes the active shader.
    DetailPreloaded(PathBuf, String, FilmPreset, Result<DetailDecode, ()>),
    /// The startup roll scan finished.
    RollsLoaded(Vec<Roll>),
    /// A single roll was scanned after being added; push it into the library.
    RollInfoLoaded(Roll),
    /// A roll cover decode finished.
    CoverReady(PathBuf, Result<Handle, ()>),
    /// The folder picker returned a roll directory to add, along with the
    /// film preset chosen for it in the dialog.
    RollAdded(PathBuf, FilmPreset),
    /// The selected roll's film preset was changed from the roll-info drawer.
    RollPresetChanged(PathBuf, FilmPreset),
    /// The user asked to make the currently viewed frame the roll's calibrated
    /// black point: its clear-film plateau is measured once and recorded in the
    /// roll manifest, overriding every frame's inversion base until cleared.
    CalibrateBaseFromFrame,
    /// The user switched this roll to per-frame automatic base measurement
    /// (explicit opt-in): every frame measures its own clear-film plateau.
    AutoBasePerFrame,
    /// The user returned this roll to the preset-first default: the stock's
    /// preset base is the truth (clears calibration and the auto opt-in).
    UsePresetBase,
    /// The user pressed the Add roll button.
    AddRoll,
    /// A roll tile was single-clicked — select it (highlight + metadata
    /// drawer target). Does not drill in.
    RollSelected(PathBuf),
    /// Remove a roll from the library (the library page owns the roll list).
    /// Non-destructive: drops the directory from the persisted config so it no
    /// longer shows as a roll card — the files and their edit manifest on disk
    /// are left untouched and the roll can be re-added later.
    RemoveRoll(PathBuf),
    /// Remove the roll currently selected in the library (menu-driven variant
    /// of [`Message::RemoveRoll`]); no-ops when no roll is selected.
    RemoveSelectedRoll,
    /// The Add Roll tile was single-clicked — select it (highlight). Does not
    /// open the folder picker; that still needs a double click or Enter.
    AddRollSelected,
    /// Open the library cell currently selected by a single click
    /// (`library_selection`), from Enter or a double click — a roll drills into
    /// its frame grid, the Add Roll tile opens the folder picker; on the frame
    /// page it opens the frame currently highlighted by `frame_selected`.
    OpenSelected,
    /// A roll tile was double-clicked — drill into its frame grid.
    RollActivated(PathBuf),
    /// Arrow-key navigation. On the library page it moves the roll selection;
    /// inside an open roll it moves the frame highlight, or pages the detail
    /// view left/right when one is open.
    Nav(MoveDir),
    /// A frame tile was single-clicked — select it (accent ring) without
    /// opening the detail view. Honors the current modifier state: a plain
    /// click clears and selects, Ctrl+click toggles membership, Shift+click
    /// selects the range from the anchor through this frame.
    FrameSelected(String),
    /// Select every frame in the open roll's filtered set (Ctrl+A).
    SelectAllFrames,
    /// A modifier key was pressed. Tracks Ctrl/Shift state so frame clicks can
    /// distinguish plain/Ctrl/Shift selection (iced's `MouseArea` carries no
    /// modifier info).
    ModifierDown(Mod),
    /// A modifier key was released; see [`Message::ModifierDown`].
    ModifierUp(Mod),
    /// A grid scrollable reported its geometry (bounds, content height, current
    /// translation). Cached so keyboard navigation can reveal the highlighted
    /// tile by scrolling the grid when it moves out of the visible viewport.
    GridViewport(Viewport),
    /// The frame scan for an opened roll finished.
    RollOpened(PathBuf, Vec<String>),
    /// The frame-info drawer's lazy EXIF parse for a frame finished; carries
    /// the parsed metadata (or a failure, which is cached so it is not retried).
    FrameInfoReady(String, Result<FrameMeta, ()>),
    /// Activate the search field: reveal the header input (and focus it),
    /// mirroring cosmic-files' search icon toggle. No-op if already active.
    SearchActivate,
    /// Deactivate the search field via the input's clear button: hide the
    /// input and drop any term, returning the header to the search icon.
    SearchClear,
    /// The active search term changed (typed into the header input).
    SearchInput(String),
    LaunchUrl(String),
    /// A surface action from a menu popup (Wayland): forwarded to the cosmic
    /// runtime, which creates/destroys the popup surface backing the menus.
    Surface(cosmic::surface::Action),
    ThumbReady(String, Result<Handle, ()>),
    /// A thumbnail was double-clicked, opening it in the detail view.
    ThumbnailActivated(String),
    /// Animation tick driving the hi-res crossfade.
    DetailFadeTick,
    /// The user moved the exposure slider.
    ExposureChanged(f32),
    /// A keyboard shortcut adjusted one of the editing controls by a signed
    /// delta (positive = increase). Routes through the same RAM + live-shader
    /// path as the matching slider; no-op unless a detail view is open.
    AdjustEdit(EditAdjust),
    /// An editing-shortcut key was released: commit any keyboard adjustments
    /// made while it was held (persist + re-bake), mirroring the slider's
    /// release commit instead of committing per auto-repeated press.
    EditKeyReleased,
    /// Wheel-scroll zoom in the detail view; payload is the change in zoom
    /// units (log2 of the scale ratio), positive = zoom in, negative = out.
    DetailZoom(f32),
    /// The detail preview was (re)laid out; carries its new logical size. Used
    /// to recompute the 1:1 ("100%") zoom cap, which depends on the preview
    /// area's size (window resize, drawer open/close, …).
    DetailAreaResized(Size),
    /// The user pressed the mouse on the detail preview — grab-pan begins.
    DetailPanPress,
    /// The cursor moved over the detail preview; while panning this shifts
    /// the image. Carries the widget-relative cursor position (logical points).
    DetailPanMove(Point),
    /// The mouse was released or left the preview — grab-pan ends.
    DetailPanRelease,
    /// The live tone curve changed: new contrast, rolloff, and shadows
    /// powers. Applies to the shader as a uniform-only remap.
    CurveChanged(f32, f32, f32),
    /// Reset every first-class edit (exposure + tone curve) to their
    /// identities in one action, and persist the reset like any other edit.
    ResetAll,
    /// The user typed into a crop margin field. Carries the affected edge and
    /// the new draft text (kept in RAM so typing doesn't fight a read-only
    /// view); nothing is committed until the field is submitted.
    CropDraftChange(edit_manifest::CropDirection, String),
    /// A crop margin field was submitted (Enter/return): parse the draft,
    /// clamp it, and apply it as an absolute source-pixel margin for that
    /// edge — re-deriving the perpendicular pair to preserve aspect — then
    /// persist like any other edit.
    CropDraftSubmit(edit_manifest::CropDirection),
    /// Reset only the crop margins to zero (show the full frame), leaving the
    /// exposure and tone edits untouched, and persist the reset.
    ResetCrop,
    /// The user typed into a roll date field in the roll-info drawer. Carries
    /// the affected field and the new draft text (kept in RAM so typing
    /// doesn't fight a read-only view); nothing is committed until submit.
    RollDateDraftChange(RollDateField, String),
    /// A roll date field was submitted (Enter/return): validate the ISO
    /// `YYYY-MM-DD` draft (empty clears the date), persist it to the roll's
    /// manifest, and update the in-memory roll and card.
    RollDateDraftSubmit(RollDateField),
    /// Rotate the detail view counter-clockwise by one 90° quarter-turn and
    /// persist immediately (the editing-drawer button; the keyboard routes the
    /// same rotation through `EditAdjust::RotateCcw` with commit-on-release).
    RotateCcw,
    /// Toggle the detail view's "view dimmed crop area" overlay: on shows the
    /// full uncropped frame on top of the zoomed crop with everything outside
    /// the crop dimmed; off shows the plain zoomed crop. View-only state.
    ToggleCropMask,
    /// Flush the in-memory roll edits to the manifest file on disk.
    EditSave,
    /// Copy the focused frame's full edit (exposure + tone curve) to the
    /// clipboard as a [`ToneEdit`] (Ctrl+C).
    CopyEdits,
    /// Paste the copied edit onto every multi-selected frame, or the focused
    /// frame when nothing else is selected (Ctrl+V).
    PasteEdits,
    /// Consume an input event without acting on it, blocking the grid
    /// beneath the detail view's input surface.
    Ignore,
    /// The user asked to export: open the native folder dialog (File → Export…
    /// or the bare `e` key).
    ExportRequested,
    /// One export frame finished: `done` of `total` frames are written (or
    /// skipped), driving the header's circular progress ring. Ticks may drop
    /// under channel backpressure without harm — the batch still completes.
    ExportProgress {
        done: usize,
        total: usize,
    },
    /// The folder dialog finished: `Some((dest, options))` exports the batch into
    /// `dest` with `options`, `None` means the dialog was cancelled.
    ExportChosen(Option<(PathBuf, ExportOptions)>),
    /// The export batch finished, carrying how many frames succeeded, how many
    /// were skipped (already present when overwrite was off), how many failed,
    /// the destination folder for the completion toast, and — when the roll had
    /// a start date — that date, so the toast can say the exports are stamped
    /// and dated.
    ExportDone {
        ok: usize,
        skipped: usize,
        failed: usize,
        dest: PathBuf,
        start_date: Option<String>,
    },
    /// A toast's duration elapsed (or its close button was pressed): dismiss
    /// it from the toaster.
    ToastClose(toaster::ToastId),
    /// Quit the application, persisting any open edits first.
    Quit,
    ToggleContextPage(ContextPage),
    /// Toggle the current page's context drawer (bare Space). Since the
    /// keyboard subscription's filter closure cannot capture app state, this
    /// defers the page choice to the update handler: editing while a detail
    /// view is open, roll info otherwise.
    ToggleContext,
    UpdateConfig(Config),
}

/// A direction for arrow-key navigation of a grid selection (library rolls or
/// open-roll frames), and for paging the detail view left/right.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveDir {
    Left,
    Right,
    Up,
    Down,
}

/// A single editing-control adjustment from a keyboard shortcut: which control
/// and the signed delta to apply (positive = increase). The step (coarse vs
/// nudge) is resolved at construction by `edit_adjust_for`, so the handler
/// only clamps against the control's range and applies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditAdjust {
    Exposure(f32),
    Contrast(f32),
    Rolloff(f32),
    Shadows(f32),
    Crop {
        direction: edit_manifest::CropDirection,
        delta: i32,
    },
    /// Rotate the display one quarter-turn counter-clockwise (a discrete step,
    /// no delta payload; unlike the numeric adjusts it doesn't hold-repeat a
    /// magnitude, but the edit-key machinery still commits on release).
    RotateCcw,
}

/// A tracked modifier key whose held state a frame click needs to decide its
/// multi-select behavior (iced's `MouseArea` does not deliver modifier state).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mod {
    Ctrl,
    Shift,
}

/// A curated export preset: the default combination of format, bit depth, and
/// size for a destination (cloud backup, further processing, the web).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportPreset {
    /// Cloud: JPEG, quality 90, native resolution — the classic "hand these to
    /// an archive service" output.
    Cloud,
    /// Master: lossless 16-bit PNG at native resolution, so an external editor
    /// gets the full dynamic range to work with.
    Master,
    /// Web: JPEG, quality 82, long edge capped at 2048 px.
    Web,
}

impl ExportPreset {
    /// The user-visible label for the preset, as shown in the dialog's format
    /// dropdown.
    #[must_use]
    fn choice_label(self) -> String {
        match self {
            Self::Cloud => fl!("export-choice-jpeg-90"),
            Self::Web => fl!("export-choice-jpeg-82"),
            Self::Master => fl!("export-choice-png-16"),
        }
    }
}

/// The export container format: the sRGB-baked frame is either lossy-compressed
/// as a JPEG (the `jpeg-encoder` path) or written losslessly as a PNG via the
/// raw `png` crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportFormat {
    Jpeg,
    Png,
}

impl ExportFormat {
    /// The filename extension for the format (`jpg` / `png`).
    #[must_use]
    fn ext(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }
}

/// Long-edge limit for the export decode. Original keeps native resolution;
/// the numbered variants downscale the frame before baking (sharing the
/// decoded buffer with the overview renderer).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ExportSize {
    #[default]
    Original,
    LongEdge2048,
}

impl ExportSize {
    /// The target long edge in pixels, or `0` for native resolution.
    #[must_use]
    fn long_edge(self) -> u32 {
        match self {
            Self::Original => 0,
            Self::LongEdge2048 => 2048,
        }
    }
}

/// The full export parameter set: seeded by a preset on open, then adjustable
/// through the dialog's controls until Save. Behavior is driven entirely by
/// these fields — the preset that seeded them is not stored (the dropdown's
/// format choice resolves straight to the options in `options_for_choice`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExportOptions {
    format: ExportFormat,
    quality: u8,
    size: ExportSize,
    /// Pixel density tagged in the output header: 0 writes none (the encode
    /// crate's default), otherwise JPEG tags JFIF dots-per-inch and PNG tags a
    /// `pHYs` pixels-per-meter value. Pure metadata — pixel data is untouched.
    ppi: u16,
    /// Whether an existing `<stem>.<ext>` in the destination is replaced. The
    /// dialog's overwrite checkbox defaults to `false`: frames whose file
    /// already exists are skipped rather than re-encoded.
    overwrite: bool,
}

impl ExportOptions {
    /// The options a preset imports, and the exact combo the dialog's format
    /// dropdown restores for its key.
    #[must_use]
    fn for_preset(preset: ExportPreset) -> Self {
        match preset {
            ExportPreset::Cloud => Self {
                format: ExportFormat::Jpeg,
                quality: 90,
                size: ExportSize::Original,
                ppi: 300,
                overwrite: false,
            },
            ExportPreset::Master => Self {
                format: ExportFormat::Png,
                quality: 100,
                size: ExportSize::Original,
                ppi: 300,
                overwrite: false,
            },
            ExportPreset::Web => Self {
                format: ExportFormat::Jpeg,
                quality: 82,
                size: ExportSize::LongEdge2048,
                ppi: 72,
                overwrite: false,
            },
        }
    }
}

/// Resolves a key returned by the dialog's format choice back into the
/// export options that key stands for. Unknown keys (or a dialog backend that
/// dropped the choice) fall back to the first preset.
#[must_use]
fn options_for_choice(key: &str) -> ExportOptions {
    let preset = match key {
        "jpeg-82" => ExportPreset::Web,
        "png-16" => ExportPreset::Master,
        // "jpeg-90" and any unknown key (a backend that dropped the choice)
        // fall back to the first preset.
        _ => ExportPreset::Cloud,
    };
    ExportOptions::for_preset(preset)
}

/// The dialog's "format" choice: the three export presets as one dropdown,
/// defaulting to the first (JPEG 90% full-res). The response returns the
/// selected key, which [`options_for_choice`] resolves back into options.
#[must_use]
fn export_format_choice() -> cosmic::dialog::file_chooser::Choice {
    let jpeg_90 = ExportPreset::Cloud.choice_label();
    let jpeg_82 = ExportPreset::Web.choice_label();
    let png_16 = ExportPreset::Master.choice_label();
    cosmic::dialog::file_chooser::Choice::new("format", &fl!("export-choice-label"), "jpeg-90")
        .insert("jpeg-90", &jpeg_90)
        .insert("jpeg-82", &jpeg_82)
        .insert("png-16", &png_16)
}

/// The dialog's "overwrite" checkbox, defaulting to unchecked (existing
/// `<stem>.<ext>` files are kept rather than replaced). The response returns
/// its state as the string `"true"` / `"false"`.
#[must_use]
fn export_overwrite_choice() -> cosmic::dialog::file_chooser::Choice {
    cosmic::dialog::file_chooser::Choice::boolean("overwrite", &fl!("export-overwrite"), false)
}

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = Message;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "io.github.jpttrssn.curvectrl";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
    #[allow(clippy::too_many_lines)] // Big struct literal; one field per line.
    fn init(
        mut core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        // Create the about widget
        let about = About::default()
            .name(fl!("app-title"))
            .icon(widget::icon::from_svg_bytes(APP_ICON))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("repository"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        // The editing panel drawer sits beside the detail view as a side pane
        // that resizes the content, rather than overlaying it.
        core.window.context_is_overlay = false;

        // Construct the app model with the runtime's core.
        let mut app = AppModel {
            core,
            context_page: ContextPage::default(),
            drawer_memory: DrawerMemory::default(),
            about,
            key_binds: HashMap::from([
                (
                    menu::KeyBind {
                        modifiers: vec![],
                        key: keyboard::Key::Character(" ".into()),
                    },
                    MenuAction::Details,
                ),
                (
                    menu::KeyBind {
                        modifiers: vec![menu::key_bind::Modifier::Ctrl],
                        key: keyboard::Key::Character("a".into()),
                    },
                    MenuAction::SelectAll,
                ),
                (
                    menu::KeyBind {
                        modifiers: vec![menu::key_bind::Modifier::Ctrl],
                        key: keyboard::Key::Character("c".into()),
                    },
                    MenuAction::CopyEdits,
                ),
                (
                    menu::KeyBind {
                        modifiers: vec![menu::key_bind::Modifier::Ctrl],
                        key: keyboard::Key::Character("v".into()),
                    },
                    MenuAction::PasteEdits,
                ),
                // Bare `e` exports the current selection; the key_binds map has
                // hint text for the menu bar, while the true trigger lives in
                // the global keyboard subscription (NoModifier KeyBinds are
                // label-only in this cosmic menu API).
                (
                    menu::KeyBind {
                        modifiers: vec![],
                        key: keyboard::Key::Character("e".into()),
                    },
                    MenuAction::Export,
                ),
            ]),
            // Optional configuration file for an application.
            config: cosmic_config::Config::new(Self::APP_ID, Config::VERSION)
                .map(|context| match Config::get_entry(&context) {
                    Ok(config) => config,
                    Err((_errors, config)) => {
                        // for why in errors {
                        //     tracing::error!(%why, "error loading app config");
                        // }

                        config
                    }
                })
                .unwrap_or_default(),
            rolls: Vec::new(),
            active: None,
            library_selection: None,
            frame_selected: None,
            selected_frames: HashSet::new(),
            selection_anchor: None,
            ctrl_down: false,
            shift_down: false,
            editing_key_held: false,
            // A 3-wide grid is a safe initial guess until the first resize.
            grid_cols: 3,
            // No viewport is known until the grid lays out and scrolls; the
            // resize handler estimates the height before that.
            grid_viewport: None,
            window_height: 600.0,
            cover_inflight: Vec::new(),
            search: None,
            tiles: Vec::new(),
            selected: None,
            thumb_inflight: Vec::new(),
            frame_meta_inflight: None,
            roll: RollManifest::default(),
            detail_shader: None,
            detail_inflight: None,
            detail_native_queued: false,
            detail_thumb_opacity: 1.0,
            detail_last_frame: None,
            detail_thumb: None,
            detail_zoom: 1.0,
            detail_area_size: None,
            detail_pan: (0.0, 0.0),
            detail_panning: false,
            detail_cursor: None,
            curve_contrast: 1.0,
            curve_rolloff: 1.0,
            curve_shadows: 1.0,
            exposure_ev: edit_manifest::DEFAULT_EXPOSURE_EV,
            crop: edit_manifest::CropMargins::default(),
            crop_drafts: CropDrafts::from_margins(edit_manifest::CropMargins::default()),
            roll_date_drafts: RollDateDrafts {
                key: None,
                start: String::new(),
                end: String::new(),
            },
            rotation: 0,
            show_crop_mask: false,
            reset_exposure_ev: edit_manifest::DEFAULT_EXPOSURE_EV,
            reset_curve_contrast: 1.0,
            reset_curve_rolloff: 1.0,
            reset_curve_shadows: 1.0,
            reset_crop: edit_manifest::CropMargins::default(),
            reset_rotation: 0,
            clipboard: None,
            next_image_id: 0,
            detail_cache: LruCache::new(DETAIL_CACHE_CAPACITY),
            detail_preload_inflight: Vec::new(),
            toasts: toaster::Toasts::new(Message::ToastClose),
            export_pending: false,
            export_progress: None,
        };

        // Set the window title and scan the configured roll directories.
        let rolls = app.config.rolls.clone();
        let command = Task::batch([
            app.update_title(),
            cosmic::task::future(async { Message::RollsLoaded(load_rolls(rolls).await) }),
        ]);

        (app, command)
    }

    /// Track the content width so arrow-key navigation of the library grid can
    /// mirror iced's `Grid::fluid` column count exactly
    /// (`ceil((width + spacing) / (max_width + spacing))`), and remember the
    /// window height for the pre-scroll viewport estimate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn on_window_resize(&mut self, _id: cosmic::iced::window::Id, width: f32, height: f32) {
        let spacing = f32::from(cosmic::theme::spacing().space_s);
        self.grid_cols = grid_num_cols(width, spacing).max(1);
        self.window_height = height;
    }

    /// Elements to pack at the start of the header bar.
    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        // Remove roll is only actionable on the rolls view when a roll is
        // selected; inside a roll (or with the add tile / nothing selected) it
        // shows disabled on the menu.
        let roll_selected = self.active.is_none()
            && matches!(&self.library_selection, Some(LibrarySelection::Roll(_)));
        let remove_roll = if roll_selected {
            menu::Item::Button(fl!("menu-remove-roll"), None, MenuAction::RemoveRoll)
        } else {
            menu::Item::ButtonDisabled(fl!("menu-remove-roll"), None, MenuAction::RemoveRoll)
        };

        // Export is only actionable when there is at least one frame to write
        // inside the open roll; while a batch runs it is disabled (a second
        // export cannot overlap the running one).
        let exporting = self.export_progress.is_some();
        let export = if exporting || !self.has_export_targets() {
            menu::Item::ButtonDisabled(fl!("menu-export"), None, MenuAction::Export)
        } else {
            menu::Item::Button(fl!("menu-export"), None, MenuAction::Export)
        };

        let file_menu = menu::Tree::with_children(
            menu::root(fl!("menu-file")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![
                    menu::Item::Button(fl!("menu-add-roll"), None, MenuAction::AddRoll),
                    remove_roll,
                    menu::Item::Divider,
                    export,
                    menu::Item::Divider,
                    menu::Item::Button(fl!("menu-quit"), None, MenuAction::Quit),
                ],
            ),
        );

        // Copy/paste edits are only actionable while a frame selection exists
        // (a focused/highlighted frame or an open detail view); on the library
        // page no frame is ever selected, even when a roll is. Paste
        // additionally needs a non-empty edit clipboard to apply.
        let frame_context = self.frame_selected.is_some() || self.selected.is_some();
        let copy_edits = if frame_context {
            menu::Item::Button(fl!("menu-copy-edits"), None, MenuAction::CopyEdits)
        } else {
            menu::Item::ButtonDisabled(fl!("menu-copy-edits"), None, MenuAction::CopyEdits)
        };
        let paste_edits = if frame_context && self.clipboard.is_some() {
            menu::Item::Button(fl!("menu-paste-edits"), None, MenuAction::PasteEdits)
        } else {
            menu::Item::ButtonDisabled(fl!("menu-paste-edits"), None, MenuAction::PasteEdits)
        };

        // Base calibration is roll-scoped and only meaningful while a film
        // negative roll is open: it records (or clears) the roll's black-point
        // override in the open roll's manifest. The handlers no-op where there
        // is no eligible frame, so the menu only gates on the preset.
        let negative_roll = self.active.is_some() && self.roll.preset().is_inverted();
        let base_items = if negative_roll {
            vec![
                menu::Item::Divider,
                menu::Item::Button(fl!("menu-calibrate-base"), None, MenuAction::CalibrateBase),
                menu::Item::Button(fl!("menu-auto-base"), None, MenuAction::AutoBase),
                menu::Item::Button(fl!("menu-preset-base"), None, MenuAction::PresetBase),
            ]
        } else {
            vec![
                menu::Item::Divider,
                menu::Item::ButtonDisabled(
                    fl!("menu-calibrate-base"),
                    None,
                    MenuAction::CalibrateBase,
                ),
                menu::Item::ButtonDisabled(fl!("menu-auto-base"), None, MenuAction::AutoBase),
                menu::Item::ButtonDisabled(fl!("menu-preset-base"), None, MenuAction::PresetBase),
            ]
        };

        let edit_menu = menu::Tree::with_children(
            menu::root(fl!("menu-edit")).apply(Element::from),
            menu::items(
                &self.key_binds,
                [
                    vec![
                        menu::Item::Button(fl!("menu-select-all"), None, MenuAction::SelectAll),
                        menu::Item::Divider,
                        copy_edits,
                        paste_edits,
                    ],
                    base_items,
                ]
                .concat(),
            ),
        );

        // The generic Details item opens the context drawer: the editing panel
        // while a detail view is open, the frame-info drawer on the frames grid
        // with a highlighted frame, or the roll-info drawer on the library
        // page. It stays enabled exactly where the drawer can open — a detail
        // view, a highlighted frame inside a roll, or a roll selected on the
        // library page (the page guards make it a no-op everywhere else).
        let details_enabled = self.selected.is_some()
            || (self.active.is_some() && self.frame_selected.is_some())
            || (self.active.is_none()
                && matches!(&self.library_selection, Some(LibrarySelection::Roll(_))));
        let details = if details_enabled {
            menu::Item::Button(fl!("menu-details"), None, MenuAction::Details)
        } else {
            menu::Item::ButtonDisabled(fl!("menu-details"), None, MenuAction::Details)
        };

        let view_menu = menu::Tree::with_children(
            menu::root(fl!("menu-view")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![
                    menu::Item::Button(fl!("about"), None, MenuAction::About),
                    details,
                ],
            ),
        );

        // Menu popups must be backed by real Wayland surfaces (and know which
        // window to anchor to), so the bar forwards surface actions to the
        // cosmic runtime — matching how cosmic-files wires its menu bar.
        let menu_bar = menu::bar(vec![file_menu, edit_menu, view_menu])
            .window_id_maybe(self.core().main_window_id())
            .on_surface_action(Message::Surface)
            .item_width(menu::ItemWidth::Uniform(250));

        vec![menu_bar.into()]
    }

    /// Elements to pack at the end of the header bar.
    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        // The editing drawer is no longer toggled from a header button — it
        // opens automatically with the detail view (see `open_frame`) and is
        // hidden/revealed by the Space context-drawer toggle — so the header
        // end packs only the search control and (while a batch runs) the
        // export progress ring.

        // Search filters the current view's entries (roll names on the library
        // page, frame names in a roll). Mirroring cosmic-files, the input is
        // only shown once search is active: an inactive state packs a search
        // icon that reveals (and focuses) the input, which then replaces the
        // icon until it is cleared.
        let search: Element<'_, Message> = if let Some(term) = &self.search {
            cosmic::widget::text_input::search_input(fl!("search-rolls"), term)
                .width(Length::Fixed(240.0))
                .id(search_input_id())
                .on_clear(Message::SearchClear)
                .on_input(Message::SearchInput)
                .into()
        } else {
            widget::button::icon(icon::from_name("system-search-symbolic"))
                .tooltip(fl!("search-toggle"))
                .on_press(Message::SearchActivate)
                .padding(8)
                .into()
        };

        // The COSMIC-Files-style export indicator sits at the far right of the
        // header: a small circular determinate progress ring that exists only
        // while a batch is running — zero screen space when idle. Hovering
        // shows which frame the batch is on.
        let mut end = vec![search];
        if let Some((done, total)) = self.export_progress {
            let ring =
                cosmic::widget::determinate_circular(export_fraction(done, total)).size(18.0);
            let ring = cosmic::widget::tooltip(
                ring,
                widget::text(fl!("export-progress", done = done, total = total)),
                cosmic::widget::tooltip::Position::Bottom,
            );
            end.push(ring.into());
        }
        end
    }

    /// Display a context drawer if the context page is requested.
    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        match self.context_page {
            ContextPage::About => Some(context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_string()),
                Message::ToggleContextPage(ContextPage::About),
            )),
            ContextPage::Editing => {
                // Without a selection there is nothing to edit.
                let name = self.selected.as_ref()?;

                Some(
                    context_drawer::context_drawer(
                        editing_panel(self),
                        Message::ToggleContextPage(ContextPage::Editing),
                    )
                    .title(name),
                )
            }
            ContextPage::RollInfo => {
                // Only meaningful on the library page with a roll selection:
                // the drawer shows metadata for that roll, so an active roll, a
                // stale, or a missing (or Add-Roll-tile) selection closes the
                // drawer rather than rendering an empty panel.
                if self.active.is_some() {
                    return None;
                }
                let LibrarySelection::Roll(dir) = self.library_selection.as_ref()? else {
                    return None;
                };
                let roll = self.rolls.iter().find(|roll| &roll.dir == dir)?;

                Some(
                    context_drawer::context_drawer(
                        roll_info_panel(
                            roll,
                            &self.roll_date_drafts.start,
                            &self.roll_date_drafts.end,
                        ),
                        Message::ToggleContextPage(ContextPage::RollInfo),
                    )
                    .title(fl!("roll-info-title")),
                )
            }
            ContextPage::FrameInfo => {
                // Only meaningful on the frames grid with a highlighted frame:
                // the drawer shows that frame's name/dimensions/EXIF, so an
                // open detail view, an inactive roll, or no highlight closes it.
                if self.selected.is_some() || self.active.is_none() {
                    return None;
                }
                let name = self.frame_selected.as_ref()?;
                if !self.tiles.iter().any(|tile| &tile.name == name) {
                    return None;
                }

                Some(
                    context_drawer::context_drawer(
                        frame_info_panel(self, name),
                        Message::ToggleContextPage(ContextPage::FrameInfo),
                    )
                    .title(name),
                )
            }
        }
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// Application events will be processed through the view. Any messages emitted by
    /// events received by widgets will be passed to the update method.
    fn view(&self) -> Element<'_, Self::Message> {
        // The page body holds only the content: there is no in-roll toolbar
        // (the back-to-rolls button was removed — Esc is the way out; search
        // lives in the header, and Add Roll is the first library tile). The
        // column wrapper also applies the Fill sizing the views otherwise
        // shrink to.
        let content: Element<_> = match self.active.as_deref() {
            Some(_) => frames_view(self),
            None => library_view(self),
        };

        let content: Element<_> = widget::column::with_capacity(1)
            .push(content)
            .spacing(cosmic::theme::spacing().space_s)
            .height(Length::Fill)
            .width(Length::Fill)
            .into();

        // Overlay the toaster (completion toasts) on top of the whole window.
        toaster::toaster(&self.toasts, content)
    }

    /// Register subscriptions for this application.
    ///
    /// Subscriptions are long-running async tasks running in the background which
    /// emit messages to the application through a channel. They can be dynamically
    /// stopped and started conditionally based on application state, or persist
    /// indefinitely.
    fn subscription(&self) -> Subscription<Self::Message> {
        // Add subscriptions which are always active.
        let mut subscriptions = vec![
            // Close the detail view when Escape is pressed outside any widget
            // that captures the key first; the other bindings drive the
            // library grid selection and the roll-info drawer.
            keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(named),
                    ..
                } => match named {
                    keyboard::key::Named::Escape => Some(Message::DetailClosed),
                    keyboard::key::Named::Enter => Some(Message::OpenSelected),
                    keyboard::key::Named::ArrowLeft => Some(Message::Nav(MoveDir::Left)),
                    keyboard::key::Named::ArrowRight => Some(Message::Nav(MoveDir::Right)),
                    keyboard::key::Named::ArrowUp => Some(Message::Nav(MoveDir::Up)),
                    keyboard::key::Named::ArrowDown => Some(Message::Nav(MoveDir::Down)),
                    keyboard::key::Named::Control => Some(Message::ModifierDown(Mod::Ctrl)),
                    keyboard::key::Named::Shift => Some(Message::ModifierDown(Mod::Shift)),
                    _ => None,
                },
                // Mirror the modifier releases so `ctrl_down`/`shift_down`
                // stay accurate even when the frame click that reads them
                // happens later.
                keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(named),
                    ..
                } => match named {
                    keyboard::key::Named::Control => Some(Message::ModifierUp(Mod::Ctrl)),
                    keyboard::key::Named::Shift => Some(Message::ModifierUp(Mod::Shift)),
                    _ => None,
                },
                // The spacebar carries no Named variant in this iced fork, so it arrives
                // as a character — matched by payload. A bare space (no
                // modifiers) toggles the active page's context drawer, which
                // replaced the old Ctrl+Space binding (the full-screen
                // preview feature that used bare Space was removed).
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if !modifiers.control() && character == " " => Some(Message::ToggleContext),
                // Ctrl+F reveals (and focuses) the search field.
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if modifiers.control() && character == "f" => Some(Message::SearchActivate),
                // Ctrl+A selects every frame in the open roll's filtered set.
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if modifiers.control() && character == "a" => Some(Message::SelectAllFrames),
                // Ctrl+C / Ctrl+V copy and paste edits. The handlers no-op
                // when there is nothing focused to copy (or nothing copied to
                // paste), and a focused text input captures these keys first
                // (so search-field copy/paste is unaffected).
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if modifiers.control() && character == "c" => Some(Message::CopyEdits),
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if modifiers.control() && character == "v" => Some(Message::PasteEdits),
                // Bare `e` (no modifiers) opens the export destination picker
                // for the current selection. Reached only when no focused text
                // input swallowed the key first (search/crop fields capture
                // bare characters), and placed before the editing-key wildcard
                // so `e` never routes into an edit adjust.
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if !modifiers.control() && character == "e" => Some(Message::ExportRequested),
                // Editing shortcuts: bare (no Ctrl) keys that map to one of the
                // editing controls; holding Shift switches to the fine nudge
                // step. The handler no-ops unless a detail view is open, so
                // these stay inert on the library/grid pages.
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if !modifiers.control() => {
                    edit_adjust_for(character.as_str(), modifiers.alt(), modifiers.shift())
                        .map(Message::AdjustEdit)
                }
                // Releasing an editing-shortcut key ends the hold: commit the
                // adjustments made while it was down (one persist + re-bake per
                // hold, not per auto-repeated press). Only fires for keys that
                // actually map to an editing control, so releasing a modifier
                // or a non-edit character stays a no-op. The update handler
                // ignores the release if no editing key is recorded as held.
                keyboard::Event::KeyReleased {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if !modifiers.control()
                    && edit_adjust_for(character.as_str(), modifiers.alt(), modifiers.shift())
                        .is_some() =>
                {
                    Some(Message::EditKeyReleased)
                }
                _ => None,
            }),
            // Watch for application configuration changes.
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| {
                    // for why in update.errors {
                    //     tracing::error!(?why, "app config error");
                    // }

                    Message::UpdateConfig(update.config)
                }),
        ];

        // Drive the thumbnail crossfade while opacity is ramping down.
        if self.selected.is_some()
            && self.detail_shader.is_some()
            && self.detail_thumb.is_some()
            && self.detail_thumb_opacity > 0.0
        {
            subscriptions.push(
                cosmic::iced::time::every(std::time::Duration::from_millis(16))
                    .map(|_| Message::DetailFadeTick),
            );
        }

        Subscription::batch(subscriptions)
    }

    /// Handles messages emitted by the application and its widgets.
    ///
    /// Tasks may be returned for asynchronous execution of code in the background
    /// on the application's async runtime.
    #[allow(clippy::too_many_lines)] // Message dispatch; arms stay inline for readability.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        // Reseed the roll-info drawer's date drafts whenever the library roll
        // selection changes (a different roll always starts from its own
        // committed dates; stale uncommitted text is dropped). Runs before
        // dispatch so every path — navigation, Space, direct selection — is
        // covered by one guard.
        self.sync_roll_date_drafts();
        match message {
            Message::DetailClosed => {
                self.persist_roll();
                // Escape first deactivates an active search (hiding the header
                // input back to the search icon) before any other close
                // behavior, mirroring cosmic-files.
                if self.search.is_some() {
                    self.search = None;
                    return Task::none();
                }
                // Escape never closes a context drawer (only Space toggles it).
                // On the library page it is a no-op beyond the search handling
                // above.
                if self.active.is_none() {
                    return Task::none();
                }
                if self.selected.is_some() {
                    // Close the detail view immediately — the frame highlight
                    // survives so the grid still shows where you were. The
                    // editing drawer is not closed; the grid's own remembered
                    // drawer state takes over.
                    self.selected = None;
                    self.clear_detail();
                    self.restore_drawer_for(DrawerView::Grid);
                    return self.ensure_frame_info_loaded().unwrap_or_else(Task::none);
                }
                // On the bare grid Escape backs out of the roll entirely,
                // resetting every detail- and roll-page field so nothing from
                // the closed roll leaks into the library (the frame highlight
                // is dropped as part of the reset; edits were already flushed
                // by `persist_roll` at the top of this arm).
                self.active = None;
                self.selected = None;
                self.frame_selected = None;
                self.selected_frames.clear();
                self.selection_anchor = None;
                self.grid_viewport = None;
                self.tiles = Vec::new();
                self.frame_meta_inflight = None;
                self.thumb_inflight.clear();
                self.detail_inflight = None;
                self.detail_preload_inflight.clear();
                // The RAM manifest is dropped with the roll; re-opened rolls
                // re-load it (see `RollOpened`). The overview LRU is
                // deliberately kept: it survives roll switches by design.
                self.roll = edit_manifest::RollManifest::default();
                self.clear_detail();
                self.restore_drawer_for(DrawerView::Library);
                Task::none()
            }

            Message::DetailReady(name, preset, result) => {
                self.handle_detail_ready(&name, preset, result)
            }

            Message::DetailPreloaded(dir, name, preset, result) => {
                self.handle_detail_preloaded(&dir, &name, preset, result)
            }

            Message::ThumbnailActivated(name) => self.open_frame(name),

            Message::DetailFadeTick => {
                let dt = self
                    .detail_last_frame
                    .map_or(0.0, |t| t.elapsed().as_secs_f32().min(0.1));
                self.detail_last_frame = Some(Instant::now());
                self.detail_thumb_opacity = (self.detail_thumb_opacity - dt * FADE_SPEED).max(0.0);
                // Crossfade complete — drop the thumbnail cache.
                if self.detail_thumb_opacity <= 0.0 {
                    self.detail_thumb = None;
                }
                Task::none()
            }

            Message::ExposureChanged(ev) => {
                // RAM-only until an edit flush point; the shader stays live.
                // Shared with the keyboard `AdjustEdit` path via `set_exposure`.
                self.set_exposure(ev);
                Task::none()
            }

            Message::AdjustEdit(adjust) => {
                // Keyboard shortcuts only make sense while a detail view (and
                // its editing controls) are on screen.
                self.editing_key_held = true;
                self.apply_edit_adjust(adjust);
                Task::none()
            }

            Message::EditKeyReleased => {
                // The held editing key was released: commit the adjustments
                // made while it was down exactly once (persist + re-bake tile
                // + cover), mirroring the slider's `EditSave` on release. If
                // nothing was held (stray release, or a release after the
                // detail view closed) this is a no-op.
                if self.editing_key_held {
                    self.editing_key_held = false;
                    self.commit_edit()
                } else {
                    Task::none()
                }
            }

            Message::DetailZoom(delta) => {
                let (new_zoom, new_pan) = apply_detail_zoom(
                    self.detail_zoom,
                    self.detail_pan,
                    self.detail_cursor,
                    delta,
                    self.max_detail_zoom(),
                );
                self.detail_zoom = new_zoom;
                self.detail_pan = new_pan;
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_view(new_zoom, new_pan);
                }
                // Level up to native once the view crosses the threshold.
                if self.detail_zoom >= NATIVE_ZOOM_THRESHOLD && !self.detail_native_queued {
                    return self.decode_detail_next();
                }
                Task::none()
            }

            Message::DetailAreaResized(size) => {
                self.detail_area_size = Some(size);
                self.reclamp_detail_zoom();
                Task::none()
            }

            Message::DetailPanPress => {
                self.detail_panning = true;
                Task::none()
            }

            Message::DetailPanMove(pt) => {
                let prev = self.detail_cursor;
                self.detail_cursor = Some(pt);
                if self.detail_panning
                    && let Some(prev) = prev
                {
                    // Grab-pan: the image follows the cursor 1:1 in logical
                    // points, independent of zoom.
                    self.detail_pan.0 += pt.x - prev.x;
                    self.detail_pan.1 += pt.y - prev.y;
                    if let Some(shader) = &mut self.detail_shader {
                        shader.set_view(self.detail_zoom, self.detail_pan);
                    }
                }
                Task::none()
            }

            Message::DetailPanRelease => {
                self.detail_panning = false;
                Task::none()
            }

            Message::CurveChanged(contrast, rolloff, shadows) => {
                // RAM-only until an edit flush point (slider `on_release`,
                // `DetailClosed`, window close) — same lifecycle as exposure.
                // Shared with the keyboard `AdjustEdit` path via `set_curve`.
                self.set_curve(contrast, rolloff, shadows);
                Task::none()
            }

            Message::ResetAll => {
                // Reset every persisted edit back to the state when the edit
                // panel was opened (the stored manifest values for the
                // selected file), in RAM, push the live shader back to that
                // state, and let the normal flush points persist it (the
                // button is one unconscious click away from a slider, so
                // mirror the sliders' choice of only-mutate-RAM: the same
                // `EditSave` points persist it).
                self.exposure_ev = self.reset_exposure_ev;
                self.curve_contrast = self.reset_curve_contrast;
                self.curve_rolloff = self.reset_curve_rolloff;
                self.curve_shadows = self.reset_curve_shadows;
                self.crop = self.reset_crop;
                self.crop_drafts = CropDrafts::from_margins(self.reset_crop);
                self.rotation = self.reset_rotation;
                if let Some(selected) = &self.selected {
                    self.roll.set_exposure(selected, self.reset_exposure_ev);
                    self.roll.set_curve(
                        selected,
                        self.reset_curve_contrast,
                        self.reset_curve_rolloff,
                        self.reset_curve_shadows,
                    );
                    self.roll.set_crop(selected, self.reset_crop);
                    self.roll.set_rotation(selected, self.reset_rotation);
                }
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_exposure(self.reset_exposure_ev);
                    shader.set_curve(
                        self.reset_curve_contrast,
                        self.reset_curve_rolloff,
                        self.reset_curve_shadows,
                    );
                    shader.set_crop(self.reset_crop);
                    shader.set_rotation(self.reset_rotation);
                }
                self.reclamp_detail_zoom();
                Task::none()
            }

            // A crop margin field gained a keystroke: keep the draft text in
            // RAM (never touching the committed edit) so the read-only view
            // can re-render it. Commit happens only on submit.
            Message::CropDraftChange(direction, value) => {
                self.crop_drafts.set(direction, value);
                Task::none()
            }

            // A crop margin field was submitted (Enter/return): parse the
            // draft as a non-negative source-pixel margin, set that edge
            // absolutely (clamped to the frame), re-derive the perpendicular
            // pair to preserve aspect, and persist like any other edit. A
            // non-numeric draft is dropped by restoring the committed margins.
            Message::CropDraftSubmit(direction) => {
                let draft = self.crop_drafts.get(direction);
                let Ok(abs_px) = draft.trim().parse::<u32>() else {
                    self.crop_drafts = CropDrafts::from_margins(self.crop);
                    return Task::none();
                };
                let Some((w, h)) = self
                    .detail_shader
                    .as_ref()
                    .map(shader::DetailProgram::source_dimensions)
                else {
                    return Task::none();
                };
                let next = set_crop_edge(self.crop, direction, abs_px, w, h);
                self.crop = next;
                self.crop_drafts = CropDrafts::from_margins(next);
                if let Some(selected) = &self.selected {
                    self.roll.set_crop(selected, next);
                }
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_crop(next);
                }
                self.reclamp_detail_zoom();
                self.commit_edit()
            }

            // Reset ONLY the crop to zero (full frame), leaving exposure/tone
            // untouched, and persist the reset immediately.
            Message::ResetCrop => {
                self.crop = edit_manifest::CropMargins::default();
                self.crop_drafts = CropDrafts::from_margins(self.crop);
                if let Some(selected) = &self.selected {
                    self.roll.set_crop(selected, self.crop);
                }
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_crop(self.crop);
                }
                self.reclamp_detail_zoom();
                self.commit_edit()
            }

            // A roll date field in the roll-info drawer gained a keystroke:
            // keep the draft text in RAM (never touching the committed date)
            // so the read-only view can re-render it. Commit happens only on
            // submit.
            Message::RollDateDraftChange(field, value) => {
                self.roll_date_drafts.set(field, value);
                Task::none()
            }

            // A roll date field was submitted (Enter/return): an empty draft
            // clears the date, a valid ISO `YYYY-MM-DD` draft commits it
            // (normalized), an invalid one is dropped by re-seeding the drafts
            // from the committed dates. The in-memory roll (and thus the
            // library card) is updated and the manifest is flushed.
            Message::RollDateDraftSubmit(field) => {
                let draft = self.roll_date_drafts.get(field).trim().to_owned();
                if !draft.is_empty() && !valid_iso_date(&draft) {
                    self.sync_roll_date_drafts();
                    return Task::none();
                }
                let Some(LibrarySelection::Roll(dir)) = self.library_selection.clone() else {
                    self.sync_roll_date_drafts();
                    return Task::none();
                };
                let Some(roll) = self.rolls.iter_mut().find(|roll| roll.dir == dir) else {
                    self.sync_roll_date_drafts();
                    return Task::none();
                };
                let next = if draft.is_empty() { None } else { Some(draft) };
                // A committed date must not contradict the other one: the roll
                // may not end before it starts (ending the export-relevant
                // start/end coherence). A conflicting submit is dropped by
                // re-seeding the drafts, like a malformed date.
                let coherent = match field {
                    RollDateField::Start => roll_dates_valid(next.as_deref(), roll.end_date.as_deref()),
                    RollDateField::End => roll_dates_valid(roll.start_date.as_deref(), next.as_deref()),
                };
                if !coherent {
                    self.sync_roll_date_drafts();
                    return Task::none();
                }
                match field {
                    RollDateField::Start => roll.start_date = next,
                    RollDateField::End => roll.end_date = next,
                }
                record_roll_dates(
                    &roll.dir,
                    roll.start_date.clone(),
                    roll.end_date.clone(),
                );
                self.roll_date_drafts =
                    RollDateDrafts::from_dates(dir, roll.start_date.as_deref(), roll.end_date.as_deref());
                Task::none()
            }

            // The editing-drawer rotate button: one CCW quarter-turn applied
            // live (RAM + shader) and persisted immediately, mirroring the
            // discrete crop commits (typed submit / reset) rather than the
            // hold-then-release keyboard path.
            Message::RotateCcw => {
                self.apply_rotate_ccw();
                self.commit_edit()
            }

            Message::ToggleCropMask => {
                self.show_crop_mask = !self.show_crop_mask;
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_show_mask(self.show_crop_mask);
                }
                Task::none()
            }

            Message::RollsLoaded(rolls) => {
                self.rolls = rolls;
                // A refresh supersedes any earlier cover chain.
                self.cover_inflight.clear();
                // A roll selection pointing at a roll that left the config is
                // stale; drop it and close a metadata drawer showing it. An Add
                // Roll selection always stays valid (the tile is always present).
                if matches!(
                    &self.library_selection,
                    Some(LibrarySelection::Roll(dir)) if !self.rolls.iter().any(|roll| &roll.dir == dir)
                ) {
                    self.library_selection = None;
                    if self.context_page == ContextPage::RollInfo {
                        self.core_mut().set_show_context(false);
                        self.drawer_memory.set(DrawerView::Library, false);
                    }
                }
                self.select_first_visible_roll();
                self.decode_covers()
            }

            Message::RollInfoLoaded(roll) => {
                if self.rolls.iter().any(|existing| existing.dir == roll.dir) {
                    return Task::none();
                }
                self.rolls.push(roll);
                self.rolls.sort_by(|a, b| a.name.cmp(&b.name));
                // The selection already points at the new roll (`RollAdded`),
                // so the card just lands alongside the others.
                self.decode_covers()
            }

            Message::CoverReady(dir, result) => {
                if let Some(roll) = self.rolls.iter_mut().find(|roll| roll.dir == dir) {
                    roll.thumb = match result {
                        Ok(handle) => Thumb::Ready(handle),
                        Err(()) => Thumb::Failed,
                    };
                }
                self.cover_inflight.retain(|pending| pending != &dir);
                self.decode_covers()
            }

            Message::AddRoll => open_roll_picker(),

            Message::RollAdded(dir, preset) => {
                // The library list is app-wide: persist the new roll no matter
                // where the folder picker was invoked from.
                if !self.rolls.iter().any(|roll| roll.dir == dir) {
                    self.config.rolls.push(dir.to_string_lossy().into_owned());
                    self.persist_config();
                }
                // Record the chosen preset. The manifest is the persistence
                // layer and the in-memory roll (on a re-add) the decode source:
                // writing first means the just-spawned scan reads it back.
                record_roll_preset(&dir, preset);
                if let Some(roll) = self.rolls.iter_mut().find(|roll| roll.dir == dir) {
                    roll.preset = preset;
                    roll.thumb = Thumb::Loading;
                }
                // Mark the new roll selected (the selection survives roll exit,
                // so backing out lands on it). The cover scan for the card
                // continues in parallel.
                self.library_selection = Some(LibrarySelection::Roll(dir.clone()));
                let scan_dir = dir.clone();
                let card = cosmic::task::future(async move {
                    Message::RollInfoLoaded(load_roll(scan_dir).await)
                });
                // Drill straight into the new roll's frame grid, from wherever
                // the app was when the roller was chosen.
                let open = self.open_roll(dir);
                Task::batch([card, open, self.decode_covers()])
            }

            Message::RollPresetChanged(dir, preset) => {
                record_roll_preset(&dir, preset);
                if let Some(roll) = self.rolls.iter_mut().find(|roll| roll.dir == dir) {
                    roll.preset = preset;
                    roll.thumb = Thumb::Loading;
                }
                let mut tasks = vec![self.decode_covers()];
                // The drawer is library-only today, so the active branch is a
                // defensive re-bake (kept so a preset change can never render
                // a stale inversion for an open roll).
                if self.active.as_deref() == Some(dir.as_path()) {
                    self.roll.set_preset(preset);
                    for tile in &mut self.tiles {
                        tile.thumb = Thumb::Loading;
                    }
                    self.thumb_inflight.clear();
                    tasks.push(self.decode_next());
                    // Drop every detail buffer decoded under the old profile:
                    // the live shader, the roll's cache entries, and any
                    // in-flight native level-up, then re-pump so the next
                    // frames come up under the new preset. Exposure/curve/crop
                    // edits survive (they are applied in-shader).
                    self.detail_shader = None;
                    self.detail_native_queued = false;
                    self.detail_inflight = None;
                    self.detail_preload_inflight.clear();
                    self.detail_thumb = None;
                    self.detail_last_frame = None;
                    let _ = self
                        .detail_cache
                        .retain(|(cached_dir, _, _)| cached_dir != &dir);
                    let current = self.selected.clone().unwrap_or_default();
                    tasks.push(self.decode_detail_next());
                    tasks.push(self.preload_detail_neighbors(&current));
                }
                Task::batch(tasks)
            }

            Message::CalibrateBaseFromFrame => self.calibrate_base_from_frame(),

            Message::AutoBasePerFrame => self.set_base_mode(RollManifest::set_base_auto),

            Message::UsePresetBase => self.set_base_mode(RollManifest::use_preset_base),

            Message::RollSelected(dir) => {
                self.library_selection = Some(LibrarySelection::Roll(dir));
                Task::none()
            }

            Message::AddRollSelected => {
                self.library_selection = Some(LibrarySelection::AddRoll);
                Task::none()
            }

            Message::RemoveRoll(dir) => self.remove_roll(dir),

            Message::RemoveSelectedRoll => match self.library_selection.clone() {
                Some(LibrarySelection::Roll(dir)) => self.remove_roll(dir),
                _ => Task::none(),
            },

            Message::OpenSelected => {
                if self.active.is_some() {
                    // Frame page: open the highlighted frame in the detail view.
                    if let Some(name) = self.frame_selected.clone() {
                        self.open_frame(name)
                    } else {
                        Task::none()
                    }
                } else {
                    match self.library_selection.clone() {
                        // A roll drills in; the Add Roll tile opens the folder
                        // picker, mirroring how Enter opens a selected roll.
                        Some(LibrarySelection::Roll(dir)) => self.open_roll(dir),
                        Some(LibrarySelection::AddRoll) => open_roll_picker(),
                        None => Task::none(),
                    }
                }
            }

            Message::Nav(dir) => {
                if self.active.is_some() {
                    // Inside a roll. With the detail view open, Left/Right page
                    // through the frames; on the bare grid they move the
                    // highlight.
                    if self.selected.is_some() {
                        let matched =
                            filtered_tiles(&self.tiles, self.search.as_deref().unwrap_or(""));
                        let current = self
                            .selected
                            .as_ref()
                            .and_then(|name| matched.iter().position(|tile| tile.name == *name));
                        if let Some(target) =
                            current.and_then(|idx| paginate(idx, matched.len(), dir))
                        {
                            return self.open_frame(matched[target].name.clone());
                        }
                        return Task::none();
                    }

                    // Bare frame grid: move the highlight, then reveal it if it
                    // stepped out of the viewport.
                    let matched = filtered_tiles(&self.tiles, self.search.as_deref().unwrap_or(""));
                    let selected = self
                        .frame_selected
                        .as_ref()
                        .and_then(|name| matched.iter().position(|tile| tile.name == *name));
                    let len = matched.len();
                    let cols = self.nav_cols();
                    if let Some(target) = nav_target(selected, len, cols, dir) {
                        let name = matched[target].name.clone();
                        self.frame_selected = Some(name.clone());
                        // Shift+arrow extends the multi-selection (keeping it
                        // additive); a plain arrow collapses to the new primary.
                        if self.shift_down {
                            self.selected_frames.insert(name.clone());
                        } else {
                            self.selected_frames.clear();
                            self.selected_frames.insert(name.clone());
                        }
                        self.selection_anchor = Some(name.clone());
                        return Task::batch([
                            self.scroll_selection_into_view("frames-grid", target, len, cols),
                            self.ensure_frame_info_loaded().unwrap_or_else(Task::none),
                        ]);
                    }
                    return Task::none();
                }

                // Library grid: move the selection over every visible cell —
                // the leading Add Roll tile (only when no search is active),
                // then the filtered rolls — and reveal it out of the viewport.
                // While a search is active the Add Roll tile is hidden, so an
                // empty match set yields no destination at all.
                let cells = library_cells(&self.rolls, self.search.as_deref().unwrap_or(""));
                let selected = library_cell_index(self.library_selection.as_ref(), &cells);
                let len = cells.len();
                let cols = self.nav_cols();
                if let Some(target) = nav_target(selected, len, cols, dir) {
                    self.library_selection = Some(cells[target].selection());
                    return self.scroll_selection_into_view("rolls-grid", target, len, cols);
                }
                Task::none()
            }

            Message::FrameSelected(name) => {
                // The clicked tile is always the keyboard focus / primary, even
                // when a multi-select toggle removes it from the selection set.
                self.frame_selected = Some(name.clone());
                let order = filtered_tiles(&self.tiles, self.search.as_deref().unwrap_or(""));
                let (updated, anchor) = apply_frame_click(
                    std::mem::take(&mut self.selected_frames),
                    &name,
                    self.ctrl_down,
                    self.shift_down,
                    self.selection_anchor.as_deref(),
                    &order,
                );
                self.selected_frames = updated;
                self.selection_anchor = anchor;
                self.ensure_frame_info_loaded().unwrap_or_else(Task::none)
            }

            Message::SelectAllFrames => {
                if self.active.is_some() {
                    self.selected_frames =
                        filtered_tiles(&self.tiles, self.search.as_deref().unwrap_or(""))
                            .into_iter()
                            .map(|tile| tile.name.clone())
                            .collect();
                }
                Task::none()
            }

            Message::ModifierDown(modifier) => {
                match modifier {
                    Mod::Ctrl => self.ctrl_down = true,
                    Mod::Shift => self.shift_down = true,
                }
                Task::none()
            }

            Message::ModifierUp(modifier) => {
                match modifier {
                    Mod::Ctrl => self.ctrl_down = false,
                    Mod::Shift => self.shift_down = false,
                }
                Task::none()
            }

            Message::GridViewport(viewport) => {
                self.grid_viewport = Some(viewport);
                Task::none()
            }

            Message::RollActivated(dir) => self.open_roll(dir),

            Message::RollOpened(dir, files) => {
                // A stale scan from a roll closed mid-scan must not land.
                if self.active.as_deref() != Some(dir.as_path()) {
                    return Task::none();
                }
                // Load the roll manifest and reconcile it against what is on
                // disk, so edits for removed files never reattach to a name
                // that later returns.
                self.roll = edit_manifest::load_roll_manifest(&dir);
                edit_manifest::reconcile(&mut self.roll, &files);

                // The frame grid remounts; any cached viewport is stale until
                // it scrolls again.
                self.grid_viewport = None;
                self.thumb_inflight.clear();

                self.tiles = files
                    .into_iter()
                    .map(|name| Tile {
                        name,
                        thumb: Thumb::Loading,
                        meta: None,
                        meta_failed: false,
                    })
                    .collect();

                // Pre-select the first visible frame so the grid always has a
                // highlight (the detail view stays closed; Enter opens it).
                if let Some(first) =
                    filtered_tiles(&self.tiles, self.search.as_deref().unwrap_or(""))
                        .into_iter()
                        .next()
                {
                    self.frame_selected = Some(first.name.clone());
                    self.selected_frames.clear();
                    self.selected_frames.insert(first.name.clone());
                    self.selection_anchor = Some(first.name.clone());
                }

                // The grid's remembered drawer state can only be applied once a
                // frame is highlighted (the pre-select above), so re-apply it
                // here — navigation never closes the drawer.
                self.restore_drawer_for(DrawerView::Grid);
                // With the grid's drawer open, parse the freshly pre-selected
                // frame's EXIF so the panel never shows a stuck "Loading…"
                // after re-entering a roll.
                Task::batch([
                    self.decode_next(),
                    self.ensure_frame_info_loaded().unwrap_or_else(Task::none),
                ])
            }

            Message::SearchActivate => {
                if self.search.is_none() {
                    self.search = Some(String::new());
                }
                // Focus the (now visible) input.
                cosmic::widget::text_input::focus(search_input_id())
            }

            Message::SearchClear => {
                self.search = None;
                Task::none()
            }

            Message::SearchInput(term) => {
                self.search = Some(term);
                Task::none()
            }

            Message::ThumbReady(name, result) => {
                if let Some(tile) = self.tiles.iter_mut().find(|tile| tile.name == name) {
                    match &result {
                        Ok(_) => rebake_trace(format_args!("ThumbReady: {name} -> Ready")),
                        Err(()) => rebake_trace(format_args!("ThumbReady: {name} -> Failed")),
                    }
                    tile.thumb = match result {
                        Ok(handle) => Thumb::Ready(handle),
                        Err(()) => Thumb::Failed,
                    };
                } else {
                    rebake_trace(format_args!("ThumbReady: {name} NOT FOUND in tiles"));
                }

                self.thumb_inflight.retain(|pending| pending != &name);
                self.decode_next()
            }

            Message::FrameInfoReady(name, result) => {
                if let Some(tile) = self.tiles.iter_mut().find(|tile| tile.name == name) {
                    match result {
                        Ok(meta) => tile.meta = Some(meta),
                        Err(()) => tile.meta_failed = true,
                    }
                }
                if self.frame_meta_inflight.as_deref() == Some(name.as_str()) {
                    self.frame_meta_inflight = None;
                }
                Task::none()
            }

            Message::ToggleContextPage(context_page) => {
                // The drawer's close button (X) routes here with its own page;
                // the View → About menu routes here with `About`. About is a
                // transient panel: the menu opens it, its X (or Space) dismisses
                // it by restoring the current view's remembered drawer state,
                // and the view drawers' X closes them, clearing that view's
                // remembered state.
                if context_page == ContextPage::About && self.context_page != ContextPage::About {
                    self.context_page = ContextPage::About;
                    self.core_mut().set_show_context(true);
                } else if self.context_page == ContextPage::About {
                    self.restore_drawer_for(self.current_view());
                } else if context_page == self.current_view().page() {
                    self.core_mut().set_show_context(false);
                    self.drawer_memory.set(self.current_view(), false);
                }
                Task::none()
            }

            Message::ToggleContext => {
                // Bare Space (and View → Details): toggle only the current
                // view's context drawer. About, if showing, is dismissed and
                // the view's own remembered state is restored.
                let view = self.current_view();
                // A genuinely showing About (menu-opened) is dismissed by
                // restoring the view's remembered drawer state — see #18.
                if self.context_page == ContextPage::About && self.core.window.show_context {
                    self.restore_drawer_for(view);
                    return Task::none();
                }
                // A "phantom" About is only `ContextPage`'s launch default
                // (nothing is showing): clear it and fall through so the very
                // first toggle opens the drawer instead of silently restoring
                // the remembered-closed state.
                if self.context_page == ContextPage::About {
                    self.context_page = view.page();
                }
                if self.core.window.show_context {
                    self.drawer_memory.set(view, false);
                    self.core_mut().set_show_context(false);
                    return Task::none();
                }
                if !self.view_drawer_valid(view) {
                    return Task::none();
                }
                self.drawer_memory.set(view, true);
                self.context_page = view.page();
                self.core_mut().set_show_context(true);
                if view == DrawerView::Grid {
                    return self.ensure_frame_info_loaded().unwrap_or_else(Task::none);
                }
                Task::none()
            }

            Message::UpdateConfig(config) => {
                self.config = config;
                Task::none()
            }

            Message::Ignore => Task::none(),

            Message::ExportRequested => {
                // Never stack a second dialog onto a pending one: the portal
                // dialog is modal once visible, but a stray keyboard or menu
                // double-fire can still land in the async gap before it shows.
                if self.export_pending {
                    return Task::none();
                }
                // A batch already running means a new export would queue behind
                // it; refuse the dialog outright rather than open one that
                // resolves into a no-op (the menu item is disabled to match).
                if self.export_progress.is_some() {
                    return Task::none();
                }
                // The keyboard `e` trigger fires on any page; without a selected
                // frame inside an open roll there is nothing to export (the File
                // menu item is disabled to match).
                if !self.has_export_targets() {
                    return Task::none();
                }
                self.export_pending = true;

                let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
                    .title(fl!("export-title"))
                    .accept_label(fl!("export-pick-folder"))
                    .choice(export_format_choice())
                    .choice(export_overwrite_choice())
                    .open_folder();

                cosmic::task::future(async move {
                    match dialog.await {
                        Ok(response) => {
                            // The chosen folder is the destination; every frame
                            // keeps its own `<stem>.<ext>`.
                            let Ok(dest) = response.url().to_file_path() else {
                                return Message::ExportChosen(None);
                            };
                            // The response lists the selected (id, value) pairs:
                            // the "format" choice carries the preset key and the
                            // "overwrite" checkbox carries its state. A backend
                            // that dropped a choice falls back to the defaults.
                            let key = response
                                .choices()
                                .iter()
                                .find(|(id, _)| id == "format")
                                .map_or("jpeg-90", |(_, value)| value.as_str());
                            let mut options = options_for_choice(key);
                            options.overwrite = response
                                .choices()
                                .iter()
                                .find(|(id, _)| id == "overwrite")
                                .is_some_and(|(_, value)| value == "true");
                            Message::ExportChosen(Some((dest, options)))
                        }
                        // Cancelled (or a portal failure) is a no-op.
                        Err(_) => Message::ExportChosen(None),
                    }
                })
            }

            Message::ExportChosen(result) => {
                self.export_pending = false;
                match result {
                    Some((dest, options)) => self.begin_export(dest, options),
                    None => Task::none(),
                }
            }

            Message::ExportProgress { done, total } => {
                self.export_progress = Some((done, total));
                Task::none()
            }

            Message::ExportDone {
                ok,
                skipped,
                failed,
                dest,
                start_date,
            } => {
                // The batch is over: hide the header ring before the summary
                // toast lands.
                self.export_progress = None;
                let toast = if failed == 0 && skipped == 0 {
                    toaster::Toast::new(match start_date {
                        Some(date) => fl!(
                            "export-done-dated",
                            count = ok,
                            dir = dest.display().to_string(),
                            date = date
                        ),
                        None => fl!(
                            "export-done",
                            count = ok,
                            dir = dest.display().to_string()
                        ),
                    })
                } else if failed == 0 {
                    toaster::Toast::new(fl!(
                        "export-done-skipped",
                        count = ok,
                        dir = dest.display().to_string(),
                        skipped = skipped
                    ))
                } else {
                    let total = ok + failed;
                    toaster::Toast::new(fl!("export-failed", count = total, failed = failed))
                };
                self.toasts.push(toast).map(cosmic::Action::App)
            }

            Message::ToastClose(id) => {
                self.toasts.remove(id);
                Task::none()
            }

            Message::EditSave => {
                // Slider release (and the close/flush points) commit the
                // dragged edit: persist the manifest, then re-bake the active
                // tile + roll cover through the shared commit path used by the
                // keyboard-release commit too.
                self.commit_edit()
            }

            Message::CopyEdits => {
                self.copy_edits();
                Task::none()
            }

            Message::PasteEdits => self.paste_edits(),

            Message::Quit => {
                // Flush any in-progress edit to disk, then ask the window to
                // close (the cosmic runtime runs our on_close_requested hook,
                // which persists a final time before exiting).
                self.persist_roll();
                self.persist_config();
                Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Close))
            }

            // Forward menu-bar popup surface actions to the cosmic runtime,
            // which creates/destroys the actual popup surfaces on Wayland.
            Message::Surface(action) => {
                cosmic::task::message(cosmic::Action::Cosmic(cosmic::app::Action::Surface(action)))
            }

            Message::LaunchUrl(url) => match open::that_detached(&url) {
                Ok(()) => Task::none(),
                Err(err) => {
                    eprintln!("failed to open {url:?}: {err}");
                    Task::none()
                }
            },
        }
    }

    /// Called when the user requests an app window to be closed; flush any
    /// unsaved edits before the window goes away.
    fn on_close_requested(&self, _id: cosmic::iced::window::Id) -> Option<Self::Message> {
        Some(Message::EditSave)
    }
}

impl AppModel {
    /// Updates the header and window titles.
    pub fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(fl!("app-title"), id)
        } else {
            Task::none()
        }
    }

    /// Ensures the library page always has a selection once rolls exist:
    /// whenever nothing is selected, highlight the first visible roll
    /// (respecting an active search). No-op while a roll is open, while no roll
    /// is visible, or when a selection already exists.
    fn select_first_visible_roll(&mut self) {
        if self.active.is_some() || self.library_selection.is_some() {
            return;
        }
        let cells = library_cells(&self.rolls, self.search.as_deref().unwrap_or(""));
        if let Some(LibraryCell::Roll(roll)) = cells
            .iter()
            .find(|cell| matches!(cell, LibraryCell::Roll(_)))
        {
            self.library_selection = Some(LibrarySelection::Roll(roll.dir.clone()));
        }
    }

    /// Drills into a roll's frame grid (double click, or Enter on the selected
    /// roll). The outgoing roll keeps its unsaved edits; the detail/editing
    /// state is reset for the fresh roll, and a library metadata drawer closes
    /// since it can never show while a roll is open.
    fn open_roll(&mut self, dir: PathBuf) -> Task<cosmic::Action<Message>> {
        if self.active.as_deref() == Some(dir.as_path()) {
            return Task::none();
        }
        self.persist_roll();
        self.active = Some(dir.clone());
        self.selected = None;
        self.frame_selected = None;
        self.selected_frames.clear();
        self.selection_anchor = None;
        self.grid_viewport = None;
        self.frame_meta_inflight = None;
        self.clear_detail();
        // Entering the grid: the drawer (if the grid's memory says open) shows
        // the frame-info panel; navigation never closes it.
        self.restore_drawer_for(DrawerView::Grid);
        cosmic::task::future(async move {
            let files = load_files_in(dir.clone()).await;
            Message::RollOpened(dir, files)
        })
    }

    /// Opens a frame in the detail view (double-click, Enter on the highlighted
    /// tile, or Left/Right paging while a detail view is open). Persists unsaved
    /// tweaks to the outgoing file, loads the stored edits, updates both the
    /// detail selection and the grid highlight, and starts the decode chain.
    fn open_frame(&mut self, name: String) -> Task<cosmic::Action<Message>> {
        if self.selected.as_deref() != Some(name.as_str()) {
            // Persist unsaved tweaks to the outgoing file first.
            self.persist_roll();
            // Read the stored edits BEFORE the decode builds the shader, which
            // consumes `self.exposure_ev` and the tone (via `set_curve` in
            // `handle_detail_ready`).
            let stored_tone = self.roll.tone(name.as_str());
            self.selected = Some(name.clone());
            self.frame_selected = Some(name.clone());
            // The opened frame becomes the primary of the multi-selection (and
            // the Shift+click anchor), so copy/paste batches stay consistent
            // with what is on screen.  Shift+open extends; a plain open
            // collapses the set to the single opened frame.
            if self.shift_down {
                self.selected_frames.insert(name.clone());
            } else {
                self.selected_frames.clear();
                self.selected_frames.insert(name.clone());
            }
            self.selection_anchor = Some(name.clone());
            self.clear_detail();
            self.exposure_ev = stored_tone.exposure_ev;
            self.curve_contrast = stored_tone.curve_contrast;
            self.curve_rolloff = stored_tone.curve_rolloff;
            self.curve_shadows = stored_tone.curve_shadows;
            self.crop = self.roll.crop(name.as_str());
            self.crop_drafts = CropDrafts::from_margins(self.crop);
            self.rotation = self.roll.rotation(name.as_str()) & 3;
            // Entering the detail view: the drawer (if the detail's memory says
            // open) shows the editing panel; navigation never closes it.
            self.restore_drawer_for(DrawerView::Detail);
            // Anchor the reset snapshot to the opened state (the stored
            // manifest values), so Reset reverts here rather than to identity.
            self.reset_exposure_ev = stored_tone.exposure_ev;
            self.reset_curve_contrast = stored_tone.curve_contrast;
            self.reset_curve_rolloff = stored_tone.curve_rolloff;
            self.reset_curve_shadows = stored_tone.curve_shadows;
            self.reset_crop = self.crop;
            self.reset_rotation = self.rotation;
        }

        // The editing drawer stays hidden on a fresh open; the user brings it
        // up with Space or the View → Details… menu when they
        // want the controls. A same-session paging to a neighbour file likewise
        // leaves whatever context state is current untouched.

        Task::batch([
            self.decode_detail_next(),
            self.preload_detail_neighbors(&name),
        ])
    }

    /// The frames the next export should target: every multi-selected frame
    /// when the user has built a multi-selection, otherwise just the focused/
    /// highlighted frame (which also mirrors the open detail frame). Empty when
    /// no roll is open or nothing is focused.
    fn export_targets(&self) -> Vec<String> {
        if self.active.is_none() {
            return Vec::new();
        }
        if self.selected_frames.is_empty() {
            return self.frame_selected.iter().cloned().collect();
        }
        // Deterministic order for the export so a re-run writes files in the
        // same sequence (a HashSet has no stable order).
        let mut names: Vec<String> = self.selected_frames.iter().cloned().collect();
        names.sort();
        names
    }

    /// Whether [`Self::export_targets`] would yield at least one frame.
    fn has_export_targets(&self) -> bool {
        self.active.is_some() && (self.frame_selected.is_some() || !self.selected_frames.is_empty())
    }

    /// Exports every target frame into `dest`, one file per frame named
    /// `<stem>.<ext>` per the given options (JPEG or lossless PNG, native or
    /// downscaled), decoding and baking each on the blocking worker pool.
    /// Streams [`Message::ExportProgress`] per finished frame (driving the
    /// header ring) plus a final [`Message::ExportDone`] summary, which clears
    /// the ring. Edits are read from the in-memory manifest — the same source
    /// the grid and detail view render from.
    fn begin_export(
        &mut self,
        dest: PathBuf,
        options: ExportOptions,
    ) -> Task<cosmic::Action<Message>> {
        let Some(dir) = self.active.clone() else {
            return Task::none();
        };
        let names = self.export_targets();
        if names.is_empty() {
            return Task::none();
        }
        // A batch is already running: refuse a second one (the disabled menu
        // item and the ExportRequested guard also block this path).
        if self.export_progress.is_some() {
            return Task::none();
        }
        // Snapshot each frame's stored edits up front (the manifest is shared and
        // could change while the batch runs, but a snapshot keeps the export of
        // one batch internally consistent). This reads the same in-memory
        // manifest the grid and detail view render from, so an export always
        // matches what is on screen — even for an edit that has been applied
        // but not yet flushed to the on-disk manifest. Each frame carries its
        // full-roll position (the sorted order the grid shows), the offset used
        // for a stable synthetic capture timestamp, and the roll's start date
        // (when set) travels along to stamp DateTimeOriginal on the output.
        let start_date = self.roll.start_date().map(str::to_owned);
        let frames: Vec<(
            String,
            edit_manifest::ToneEdit,
            edit_manifest::CropMargins,
            u8,
            usize,
        )> = names
            .into_iter()
            .map(|name| {
                let tone = self.roll.tone(&name);
                let crop = self.roll.crop(&name);
                let rotation = self.roll.rotation(&name) & 3;
                let index = self
                    .tiles
                    .iter()
                    .position(|tile| tile.name == name)
                    .unwrap_or(0);
                (name, tone, crop, rotation, index)
            })
            .collect();
        let total = frames.len();
        self.export_progress = Some((0, total));
        let preset = self.roll.preset();
        let base_config = self.roll.base_config();

        // Stream from an async channel so the UI sees per-frame ticks. After
        // each frame the sender pushes an `ExportProgress` message (dropping
        // harmlessly if the UI is behind on backpressure), and the final
        // `ExportDone` is awaited *through* the channel so it can never be
        // lost — the ring is guaranteed a matching completion message.
        cosmic::task::stream(cosmic::iced::stream::channel(1, async move |mut sender| {
            let mut tick = |done: usize, _total: usize| {
                let _ = sender.try_send(Message::ExportProgress { done, total });
            };
            let (ok, skipped, failed) = export_frames(
                dir,
                dest.clone(),
                frames,
                options,
                preset,
                base_config,
                start_date.clone(),
                &mut tick,
            )
            .await;
            let _ = sender
                .send(Message::ExportDone {
                    ok,
                    skipped,
                    failed,
                    dest,
                    start_date,
                })
                .await;
        }))
    }

    /// The column count arrow-key navigation should use: the exact grid count
    /// derived from the cached viewport width when one is known (matching iced's
    /// fluid math), falling back to the window-resize track when the grid has
    /// not scrolled yet.
    fn nav_cols(&self) -> usize {
        self.grid_viewport.as_ref().map_or_else(
            || self.grid_cols.max(1),
            |viewport| {
                let spacing = f32::from(cosmic::theme::spacing().space_s);
                grid_num_cols(viewport.bounds().width - 2.0 * spacing, spacing).max(1)
            },
        )
    }

    /// Scrolls the mounted grid so the tile at `index` (within the matched set
    /// of `len`, laid out `cols`-wide) is fully visible, when it has moved
    /// beyond the viewport. Uses the cached [`Viewport`] for precise reveal;
    /// before the first real scroll the geometry is estimated from the window
    /// height and row count. Returns the scroll effect, or `Task::none()` when
    /// the tile is already visible.
    #[allow(clippy::cast_precision_loss)] // row/cell counts are far below f32's exact range
    fn scroll_selection_into_view(
        &self,
        name: &'static str,
        index: usize,
        len: usize,
        cols: usize,
    ) -> Task<cosmic::Action<Message>> {
        let spacing = f32::from(cosmic::theme::spacing().space_s);
        let padding = spacing;

        let (cell_width, viewport_height, viewport_offset_y, content_height) =
            if let Some(viewport) = &self.grid_viewport {
                let available = viewport.bounds().width - 2.0 * padding;
                let cols = grid_num_cols(available, spacing).max(1);
                let cell = (available - spacing * (cols as f32 - 1.0)) / cols as f32;
                (
                    cell,
                    viewport.bounds().height,
                    viewport.absolute_offset().y,
                    viewport.content_bounds().height,
                )
            } else {
                // No viewport yet: the grid sits at the top, its height is the
                // window (≈80 px of header sizing leaves the content space),
                // and the content is sized from the row count at THUMB cells.
                let rows = len.div_ceil(cols.max(1));
                let content_height = 2.0 * padding
                    + rows as f32 * (THUMB_SIZE + spacing)
                    + spacing * (rows.saturating_sub(1)) as f32;
                (
                    THUMB_SIZE,
                    (self.window_height - 80.0).max(1.0),
                    0.0,
                    content_height,
                )
            };

        match reveal_target_y(
            cols,
            index,
            spacing,
            padding,
            cell_width,
            viewport_height,
            viewport_offset_y,
            content_height,
        ) {
            Some(y) => cosmic::iced::widget::scrollable::scroll_to::<cosmic::Action<Message>>(
                scrollable_id(name),
                cosmic::iced::widget::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(y),
                },
            ),
            None => Task::none(),
        }
    }

    /// Spawns decoding of up to [`MAX_CONCURRENT_THUMBS`] pending thumbnails.
    ///
    /// Decoding is bounded rather than strictly sequential so several
    /// `spawn_blocking` RAW decodes overlap on multicore machines; the count
    /// keeps the transient footprint to a few full-resolution buffers. A name
    /// already in flight is never spawned again, so a re-bake requested
    /// mid-chain waits for the running decode instead of racing it. The
    /// exposure read here is the value at spawn time, so every tile (initial
    /// chain or re-bake) bakes in the version of the edit that is current when
    /// it actually decodes.
    fn decode_next(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(dir) = self.active.clone() else {
            return Task::none();
        };
        let capacity = MAX_CONCURRENT_THUMBS.saturating_sub(self.thumb_inflight.len());
        if capacity == 0 {
            rebake_trace(format_args!(
                "decode_next: capacity 0 (inflight={})",
                self.thumb_inflight.len()
            ));
            return Task::none();
        }

        let pending: Vec<(
            String,
            edit_manifest::ToneEdit,
            edit_manifest::CropMargins,
            u8,
        )> = self
            .tiles
            .iter()
            .filter(|tile| matches!(tile.thumb, Thumb::Loading))
            .filter(|tile| !self.thumb_inflight.iter().any(|name| name == &tile.name))
            .take(capacity)
            .map(|tile| {
                let tone = self.roll.tone(&tile.name);
                let crop = self.roll.crop(&tile.name);
                let rotation = self.roll.rotation(&tile.name) & 3;
                (tile.name.clone(), tone, crop, rotation)
            })
            .collect();

        if pending.is_empty() {
            rebake_trace(format_args!(
                "decode_next: no loading tiles (tiles={}, inflight={})",
                self.tiles.len(),
                self.thumb_inflight.len()
            ));
            return Task::none();
        }
        rebake_trace(format_args!(
            "decode_next: spawning {} [{:?}]",
            pending.len(),
            pending.iter().map(|(n, ..)| n.as_str()).collect::<Vec<_>>()
        ));

        // The in-memory manifest is loaded synchronously in `RollOpened`
        // before the tile pump runs, so it carries the preset even while the
        // library card scan (`RollInfoLoaded`) is still in flight for a
        // freshly-added roll — `self.rolls` can be a step behind on that path.
        let preset = self.roll.preset();
        let base_config = self.roll.base_config();

        self.thumb_inflight
            .extend(pending.iter().map(|(name, ..)| name.clone()));

        Task::batch(
            pending
                .into_iter()
                .map(move |(name, tone, crop, rotation)| {
                    cosmic::task::future(decode_thumbnail(
                        dir.clone(),
                        name,
                        tone,
                        crop,
                        rotation,
                        preset,
                        base_config,
                    ))
                }),
        )
    }

    /// Spawns decoding of up to [`MAX_CONCURRENT_THUMBS`] roll-cover
    /// thumbnails, mirroring the frame chain's bounds and de-duplication.
    fn decode_covers(&mut self) -> Task<cosmic::Action<Message>> {
        let capacity = MAX_CONCURRENT_THUMBS.saturating_sub(self.cover_inflight.len());
        if capacity == 0 {
            return Task::none();
        }

        let pending: Vec<(PathBuf, String, FilmPreset)> = self
            .rolls
            .iter()
            .filter(|roll| matches!(roll.thumb, Thumb::Loading))
            .filter(|roll| !self.cover_inflight.iter().any(|dir| dir == &roll.dir))
            .filter_map(|roll| {
                roll.cover
                    .clone()
                    .map(|name| (roll.dir.clone(), name, roll.preset))
            })
            .take(capacity)
            .collect();

        if pending.is_empty() {
            return Task::none();
        }

        self.cover_inflight
            .extend(pending.iter().map(|(dir, _, _)| dir.clone()));

        Task::batch(
            pending
                .into_iter()
                .map(|(dir, name, preset)| cosmic::task::future(decode_cover(dir, name, preset))),
        )
    }

    /// Re-bake the active frame's grid thumbnail and, if that frame is also
    /// this roll's cover, the library roll card — the shared re-decode step of
    /// every commit (slider release, keyboard-shortcut release, crop commits)
    /// so the grid and roll preview reflect the latest edit. The bake reads the
    /// crop/tone from the in-memory roll at decode time, so it reflects a live
    /// edit without a prior `persist_roll()` (`commit_edit` persists first so
    /// the cover's on-disk manifest read is fresh too).
    fn re_bake_edit(&mut self) -> Task<cosmic::Action<Message>> {
        let mut tasks = Vec::with_capacity(2);
        let Some(name) = self.selected.clone() else {
            return Task::batch(tasks);
        };
        rebake_trace(format_args!(
            "re_bake_edit: selected={name} active={}",
            self.active.is_some()
        ));
        if let Some(tile) = self.tiles.iter_mut().find(|tile| tile.name == name) {
            tile.thumb = Thumb::Loading;
            rebake_trace(format_args!("re_bake_edit: tile {name} -> Loading"));
        }
        tasks.push(self.decode_next());
        if let Some(active) = self.active.as_ref() {
            let Some(roll) = self
                .rolls
                .iter_mut()
                .find(|roll| roll.dir == *active && roll.cover.as_deref() == Some(name.as_str()))
            else {
                return Task::batch(tasks);
            };
            roll.thumb = Thumb::Loading;
            rebake_trace(format_args!("re_bake_edit: roll cover {name} -> Loading"));
            tasks.push(self.decode_covers());
        }
        Task::batch(tasks)
    }

    /// Commits the open frame's edits: persists the RAM manifest to disk, then
    /// re-bakes the affected grid tile + roll cover. This is the single commit
    /// point shared by slider release (`EditSave`), keyboard-shortcut release
    /// (`EditKeyReleased`), and the discrete crop commits (typed margin submit,
    /// reset crop). Editing steps themselves mutate live (RAM + shader) only;
    /// the commit happens once per interaction, like a slider drag/release.
    fn commit_edit(&mut self) -> Task<cosmic::Action<Message>> {
        self.persist_roll();
        self.re_bake_edit()
    }

    /// Measures the currently viewed frame's clear-film plateau and records it
    /// as the roll's calibrated black point, then re-renders everything under
    /// the new base. The frame must yield a plausible measurement
    /// (`>= MIN_PLAUSIBLE_BASE`); otherwise the calibration is left untouched —
    /// a frame with no clear film (e.g. one shot against a grey card) must not
    /// poison the roll's rendering.
    fn calibrate_base_from_frame(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(dir) = self.active.clone() else {
            return Task::none();
        };
        let Some(name) = self.selected.clone() else {
            return Task::none();
        };
        let preset = self.roll.preset();
        let Some(cached) = self.detail_cache.get(&(dir, preset, name)) else {
            return Task::none();
        };
        let Some(base) = measure_base(&cached.mono).filter(|base| *base >= MIN_PLAUSIBLE_BASE)
        else {
            return Task::none();
        };
        self.roll.set_calibrated_base(base);
        self.reflow_base()
    }

    /// Applies a roll-level base-mode change (auto opt-in or preset-first
    /// return), then re-renders every rendering under the new black point.
    fn set_base_mode(
        &mut self,
        apply: impl FnOnce(&mut RollManifest),
    ) -> Task<cosmic::Action<Message>> {
        if self.active.is_none() {
            return Task::none();
        }
        apply(&mut self.roll);
        self.reflow_base()
    }

    /// Rebuilds every rendering after a base-calibration change: the live
    /// detail decode, the grid tiles, and the roll covers all carry a black
    /// point resolved at decode time, so the cached overviews and decodes are
    /// dropped and the roll is re-decoded under the new base mode. The on-disk
    /// manifest is written first since the cover path reads it from disk.
    fn reflow_base(&mut self) -> Task<cosmic::Action<Message>> {
        self.persist_roll();
        // A cached overview's `inversion` (or an in-flight decode) was resolved
        // under the old base mode and must not be served: drop the live shader,
        // the LRU, and the in-flight bookkeeping.
        self.detail_shader = None;
        self.detail_native_queued = false;
        self.detail_inflight = None;
        self.detail_preload_inflight.clear();
        self.detail_thumb = None;
        self.detail_last_frame = None;
        let _ = self.detail_cache.retain(|_| false);
        // Re-bake every grid tile and the open roll's cover under the new base.
        for tile in &mut self.tiles {
            tile.thumb = Thumb::Loading;
        }
        self.thumb_inflight.clear();
        if let Some(active) = self.active.as_ref()
            && let Some(roll) = self.rolls.iter_mut().find(|roll| &roll.dir == active)
        {
            roll.thumb = Thumb::Loading;
        }
        let current = self.selected.clone().unwrap_or_default();
        Task::batch([
            self.decode_next(),
            self.decode_covers(),
            self.decode_detail_next(),
            self.preload_detail_neighbors(&current),
        ])
    }

    /// Spawns the hi-res decode behind the detail view when one is due.
    ///
    /// Two progressive levels share the single in-flight slot: the 2048
    /// overview while the shader is absent, then a native-resolution re-decode
    /// once the zoom crosses [`NATIVE_ZOOM_THRESHOLD`] — the flag makes the
    /// level-up one-shot, and the inflight slot makes extra wheels no-ops.
    fn decode_detail_next(&mut self) -> Task<cosmic::Action<Message>> {
        if self.detail_inflight.is_some() || self.selected.is_none() {
            if self.detail_inflight.is_some() {
                detail_trace(format_args!(
                    "trigger swallowed: inflight slot busy (zoom={:.3})",
                    self.detail_zoom
                ));
            }
            return Task::none();
        }

        // The active roll's in-memory manifest is the decode source of truth
        // (loaded synchronously in `RollOpened`, kept in sync on preset
        // changes), so this is correct even while the library card scan for a
        // freshly added roll is still in flight.
        let preset = self.roll.preset();

        // Level 1 is native unless the sensor's long edge exceeds the wgpu
        // texture ceiling; `resize_area` treats a cap ≥ the source edge as
        // identity, so ≤8K scans decode at true full resolution.
        let cap: u32 = if self.detail_shader.is_none() {
            HI_RES_SIZE
        } else if !self.detail_native_queued {
            MAX_TEXTURE_EDGE
        } else {
            detail_trace(format_args!(
                "trigger swallowed: native already queued (zoom={:.3})",
                self.detail_zoom
            ));
            return Task::none();
        };

        let name = self
            .selected
            .clone()
            .expect("selected when a decode is due");

        // An overview request that's already in the LRU cache is served without
        // re-decoding the RAW (the whole point of the cache): build the shader
        // from the cached mono and return — the preview is ready immediately
        // with no thumbnail crossfade. Native level-up requests are never
        // cached/served; they always re-decode on demand. (The cached mono is
        // cloned out so the cache keeps serving future hits; the clone is a
        // fraction of the cost of a full RAW decode.)
        if cap == HI_RES_SIZE
            && let Some(dir) = self.active.clone()
            && let Some(cached) = self
                .detail_cache
                .get(&(dir, preset, name.clone()))
                .map(|c| {
                    (
                        c.mono.clone(),
                        c.width,
                        c.height,
                        c.src_long_edge,
                        c.inversion,
                    )
                })
        {
            let (mono, width, height, src_long_edge, inversion) = cached;
            detail_trace(format_args!(
                "cache hit: {name} {width}x{height} (src {src_long_edge})"
            ));
            self.install_detail_shader(mono, width, height, src_long_edge, inversion);
            return Task::none();
        }

        detail_trace(format_args!(
            "trigger fire: cap={cap}, shader={}, zoom={:.3}",
            self.detail_shader.is_some(),
            self.detail_zoom
        ));

        self.detail_inflight = Some(name.clone());

        let Some(dir) = self.active.clone() else {
            return Task::none();
        };

        let base_config = self.roll.base_config();

        cosmic::task::future(decode_detail(dir, name, cap, preset, base_config))
    }

    /// Builds and installs the detail shader from a decoded mono buffer,
    /// applying the current exposure, zoom/pan and tone curve, and marking the
    /// native level-up done when the source was already at (or below) the
    /// overview cap. Used when serving an LRU cache hit so the preview appears
    /// immediately without re-decoding the RAW.
    fn install_detail_shader(
        &mut self,
        mono: Vec<f32>,
        width: u32,
        height: u32,
        src_long_edge: u32,
        inversion: Option<(MonoStock, f32)>,
    ) {
        let image_id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1);
        self.detail_shader = Some(shader::DetailProgram::new(
            mono,
            width,
            height,
            self.exposure_ev,
            self.crop,
            self.rotation,
            image_id,
            src_long_edge,
            inversion,
        ));
        if let Some(shader) = &mut self.detail_shader {
            shader.set_view(self.detail_zoom, self.detail_pan);
            shader.set_curve(self.curve_contrast, self.curve_rolloff, self.curve_shadows);
            shader.set_crop(self.crop);
            shader.set_rotation(self.rotation);
        }
        // A new texture (first decode or the native level-up) changes the 1:1
        // cap, so re-derive it and clamp the current zoom.
        self.reclamp_detail_zoom();
        // A cached overview that was already at native resolution needs no
        // level-up re-decode.
        if src_long_edge <= HI_RES_SIZE {
            self.detail_native_queued = true;
        }
    }

    /// Handles a finished neighbor preload decode: lands the overview into the
    /// LRU cache (dropping any evicted buffer) and frees its preload slot.
    /// A preload never touches `detail_inflight` or the active shader, and a
    /// stale landing (roll switched away mid-decode) is still a valid cached
    /// overview, so it is inserted regardless.
    fn handle_detail_preloaded(
        &mut self,
        dir: &PathBuf,
        name: &str,
        preset: FilmPreset,
        result: Result<DetailDecode, ()>,
    ) -> Task<cosmic::Action<Message>> {
        self.detail_preload_inflight
            .retain(|(pending_dir, pending_name)| pending_dir != dir || pending_name != name);

        if let Ok(DetailDecode {
            mono,
            width,
            height,
            src_long_edge,
            inversion,
        }) = result
        {
            let _evicted = self.detail_cache.insert(
                (dir.clone(), preset, name.to_string()),
                DetailMono {
                    mono,
                    width,
                    height,
                    src_long_edge,
                    inversion,
                },
            );
        }
        Task::none()
    }

    /// Preloads the frames `DETAIL_PRELOAD_DISTANCE` either side of `name` in
    /// the search-filtered set into the LRU cache, so Left/Right paging to a
    /// neighbor is instant. Runs on its own bounded channel, separate from the
    /// single critical detail slot. Already-cached and already-in-flight frames
    /// are skipped.
    fn preload_detail_neighbors(&mut self, name: &str) -> Task<cosmic::Action<Message>> {
        let Some(dir) = self.active.clone() else {
            return Task::none();
        };

        let matched = filtered_tiles(&self.tiles, self.search.as_deref().unwrap_or(""));
        let Some(current) = matched.iter().position(|tile| tile.name == name) else {
            return Task::none();
        };

        // Build the neighbor names in priority order (closest first) so the
        // bounded slots fill with the most useful frames first.
        let mut neighbors = Vec::new();
        for step in 1..=DETAIL_PRELOAD_DISTANCE {
            if let Some(prev) = current.checked_sub(step) {
                neighbors.push(matched[prev].name.clone());
            }
            if let Some(next) = current.checked_add(step).filter(|&i| i < matched.len()) {
                neighbors.push(matched[next].name.clone());
            }
        }

        let capacity = MAX_CONCURRENT_PRELOADS.saturating_sub(self.detail_preload_inflight.len());
        if capacity == 0 {
            return Task::none();
        }

        let preset = self.roll.preset();

        let pending: Vec<String> = neighbors
            .into_iter()
            .filter(|n| {
                let key = (dir.clone(), preset, n.clone());
                !self.detail_cache.contains(&key)
                    && !self
                        .detail_preload_inflight
                        .iter()
                        .any(|(pd, pn)| pd == &dir && pn == n)
            })
            .take(capacity)
            .collect();

        if pending.is_empty() {
            return Task::none();
        }

        self.detail_preload_inflight
            .extend(pending.iter().map(|n| (dir.clone(), n.clone())));

        let base_config = self.roll.base_config();

        Task::batch(
            pending
                .into_iter()
                .map(|n| cosmic::task::future(preload_detail(dir.clone(), n, preset, base_config))),
        )
    }

    /// Reset all detail-view buffers, crossfade state, the view transform, and
    /// the non-persisted tone-curve preview (each open starts at the identity).
    fn clear_detail(&mut self) {
        self.detail_shader = None;
        self.detail_thumb_opacity = 1.0;
        self.detail_last_frame = None;
        self.detail_thumb = None;
        self.detail_zoom = 1.0;
        self.detail_area_size = None;
        self.detail_native_queued = false;
        self.detail_pan = (0.0, 0.0);
        self.detail_panning = false;
        self.detail_cursor = None;
        // Any held editing key is dead once the detail view (and its edit
        // context) goes away; a late `EditKeyReleased` will find it clear and
        // no-op rather than committing a stale selection.
        self.editing_key_held = false;
        self.curve_contrast = 1.0;
        self.curve_rolloff = 1.0;
        self.curve_shadows = 1.0;
        self.exposure_ev = edit_manifest::DEFAULT_EXPOSURE_EV;
        self.crop = edit_manifest::CropMargins::default();
        self.crop_drafts = CropDrafts::from_margins(self.crop);
        self.rotation = 0;
        self.show_crop_mask = false;
        // The reset snapshot mirrors the live edit values' lifecycle: reset
        // to identity on close; the next `ThumbnailActivated` re-syncs it.
        self.reset_exposure_ev = edit_manifest::DEFAULT_EXPOSURE_EV;
        self.reset_curve_contrast = 1.0;
        self.reset_curve_rolloff = 1.0;
        self.reset_curve_shadows = 1.0;
        self.reset_crop = edit_manifest::CropMargins::default();
        self.reset_rotation = 0;
    }

    /// The effective maximum zoom for the current detail view: the 1:1 ("100%")
    /// point (one image pixel per physical screen pixel) for the live shader's
    /// texture at the preview's current size, capped by the absolute
    /// [`MAX_DETAIL_ZOOM`] guard. Falls back to [`MAX_DETAIL_ZOOM`] until both
    /// a shader and a reported preview size exist.
    fn max_detail_zoom(&self) -> f32 {
        let Some(shader) = &self.detail_shader else {
            return MAX_DETAIL_ZOOM;
        };
        let Some(size) = self.detail_area_size else {
            return MAX_DETAIL_ZOOM;
        };
        let sf = self.core().scale_factor();
        shader
            .zoom_100(size.width, size.height, sf)
            .min(MAX_DETAIL_ZOOM)
    }

    /// Recompute the 1:1 zoom cap and pull the current zoom down to it if it
    /// shrank (texture level-up installs a larger texture and RAISES the cap;
    /// a crop/rotation/resize can LOWER it). Pushes the (possibly clamped) view
    /// to the shader so the render and state agree.
    fn reclamp_detail_zoom(&mut self) {
        let max = self.max_detail_zoom();
        if self.detail_zoom > max {
            self.detail_zoom = max;
            if let Some(shader) = &mut self.detail_shader {
                shader.set_view(self.detail_zoom, self.detail_pan);
            }
        }
    }

    /// The navigational view currently shown (which owns a context drawer).
    fn current_view(&self) -> DrawerView {
        if self.selected.is_some() {
            DrawerView::Detail
        } else if self.active.is_some() {
            DrawerView::Grid
        } else {
            DrawerView::Library
        }
    }

    /// Whether `view`'s drawer has a valid panel (so a toggle or restore never
    /// opens an empty drawer).
    fn view_drawer_valid(&self, view: DrawerView) -> bool {
        match view {
            DrawerView::Library => {
                self.active.is_none()
                    && matches!(&self.library_selection, Some(LibrarySelection::Roll(_)))
            }
            DrawerView::Grid => self.active.is_some() && self.frame_selected.is_some(),
            DrawerView::Detail => self.selected.is_some(),
        }
    }

    /// Point the context drawer at `view`'s panel with that view's remembered
    /// open/closed state, clamped to validity. Called on every view transition
    /// so the drawer follows the current view without ever closing on
    /// navigation.
    fn restore_drawer_for(&mut self, view: DrawerView) {
        self.context_page = view.page();
        let open = self.drawer_memory.get(view) && self.view_drawer_valid(view);
        self.core_mut().set_show_context(open);
    }

    /// Reseeds the roll-info drawer's date drafts whenever the library roll
    /// selection no longer matches the key the drafts were seeded from (a
    /// different roll always starts from its own committed dates; a stale
    /// selection such as the Add Roll tile clears the key so the next roll
    /// reseeds). Cheap: a path comparison — it runs at the top of `update` for
    /// every message.
    fn sync_roll_date_drafts(&mut self) {
        let Some(LibrarySelection::Roll(dir)) = self.library_selection.clone() else {
            self.roll_date_drafts.key = None;
            return;
        };
        if self.roll_date_drafts.key.as_deref() == Some(dir.as_path()) {
            return;
        }
        let (start, end) = self
            .rolls
            .iter()
            .find(|roll| roll.dir == dir)
            .map_or((None, None), |roll| {
                (roll.start_date.as_deref(), roll.end_date.as_deref())
            });
        self.roll_date_drafts = RollDateDrafts::from_dates(dir, start, end);
    }

    /// Kicks off the lazy EXIF parse for the highlighted frame if the frame-info
    /// drawer needs it and it hasn't been parsed (or failed) already. Called on
    /// opening the drawer and whenever the highlight changes while it is open;
    /// returns the spawned task for the caller to run.
    fn ensure_frame_info_loaded(&mut self) -> Option<Task<cosmic::Action<Message>>> {
        if self.context_page != ContextPage::FrameInfo || !self.core.window.show_context {
            return None;
        }
        let name = self.frame_selected.clone()?;
        let dir = self.active.clone()?;
        let needs_parse = self
            .tiles
            .iter()
            .find(|tile| tile.name == name)
            .is_some_and(|tile| tile.meta.is_none() && !tile.meta_failed);
        if !needs_parse || self.frame_meta_inflight.as_deref() == Some(name.as_str()) {
            return None;
        }
        self.frame_meta_inflight = Some(name.clone());

        Some(cosmic::task::future(async move {
            let parse_name = name.clone();
            let result =
                tokio::task::spawn_blocking(move || load_frame_meta(&dir, &parse_name)).await;
            Message::FrameInfoReady(name, result.unwrap_or(Err(())))
        }))
    }

    /// Writes the in-memory roll edits to the open roll's manifest file on disk.
    fn persist_roll(&self) {
        let Some(dir) = &self.active else {
            return;
        };
        if let Err(err) = edit_manifest::save_roll_manifest(dir, &self.roll) {
            eprintln!("failed to save edits: {err}");
        }
    }

    /// Persists the user-controlled `rolls` list to the app config, so added
    /// rolls survive restarts. The existing config file is written in place
    /// (a fresh `Config` context, matching the one used at load).
    fn persist_config(&self) {
        let Ok(context) = cosmic_config::Config::new(Self::APP_ID, Config::VERSION) else {
            return;
        };
        if let Err(err) = self.config.write_entry(&context) {
            eprintln!("failed to save config: {err}");
        }
    }

    /// Writes the open frame's exposure live: the RAM roll edit and the GPU
    /// shader uniform. The slider (`ExposureChanged`) and a keyboard step
    /// (`EditAdjust::Exposure`) both route here; committing (persist + re-bake)
    /// stays separate so the sliders can stream drags without re-decoding
    /// thumbnails on every move.
    fn set_exposure(&mut self, ev: f32) {
        self.exposure_ev = ev;
        if let Some(selected) = &self.selected {
            self.roll.set_exposure(selected, ev);
        }
        if let Some(shader) = &mut self.detail_shader {
            shader.set_exposure(ev);
        }
    }

    /// Writes the open frame's tone-curve powers live: the RAM roll edit and
    /// the GPU shader uniforms. The slider (`CurveChanged`) and a keyboard step
    /// (`EditAdjust::Contrast`/`Rolloff`/`Shadows`) both route here; committing
    /// (persist + re-bake) stays separate like [`Self::set_exposure`].
    fn set_curve(&mut self, contrast: f32, rolloff: f32, shadows: f32) {
        self.curve_contrast = contrast;
        self.curve_rolloff = rolloff;
        self.curve_shadows = shadows;
        if let Some(selected) = &self.selected {
            self.roll.set_curve(selected, contrast, rolloff, shadows);
        }
        if let Some(shader) = &mut self.detail_shader {
            shader.set_curve(contrast, rolloff, shadows);
        }
    }

    /// Applies a keyboard-shortcut edit adjustment, mirroring the slider
    /// messages: mutate the RAM roll edit and the live shader only — the
    /// commit (persist + re-bake) happens on key release via
    /// [`Self::commit_edit`], the same cadence as a slider drag/release.
    /// No-ops unless a detail view is open (there is nothing to edit
    /// otherwise).
    fn apply_edit_adjust(&mut self, adjust: EditAdjust) {
        // Only adjust while a detail view is up, so the bindings never mutate
        // edits for an unseen frame.
        if self.selected.is_none() {
            return;
        }

        match adjust {
            EditAdjust::Exposure(delta) => {
                let ev = clamp_ev(self.exposure_ev + delta);
                self.set_exposure(ev);
            }
            // Each tone arm only changes one power; the others keep their
            // current values so the composed curve stays fully defined.
            EditAdjust::Contrast(delta) => {
                let contrast = clamp_curve_power(self.curve_contrast + delta);
                self.set_curve(contrast, self.curve_rolloff, self.curve_shadows);
            }
            EditAdjust::Rolloff(delta) => {
                let rolloff = clamp_curve_power(self.curve_rolloff + delta);
                self.set_curve(self.curve_contrast, rolloff, self.curve_shadows);
            }
            EditAdjust::Shadows(delta) => {
                let shadows = clamp_curve_power(self.curve_shadows + delta);
                self.set_curve(self.curve_contrast, self.curve_rolloff, shadows);
            }
            // A crop trims one edge by `delta` source pixels (positive = trim
            // more / shrink, negative = trim less / grow), preserving aspect
            // via the perpendicular-pair re-derivation. The trim must be bounded
            // by the FULL-RESOLUTION display-oriented source dims the persisted
            // crop is authored against (NOT the downscaled texture dims — those
            // change between the 2048 overview and the native level-up, and
            // would shift the meaning of the stored margins); with no ready
            // detail (decode still in flight) it stays a no-op.
            EditAdjust::Crop { direction, delta } => {
                let (w, h) = match &self.detail_shader {
                    Some(shader) => shader.source_dimensions(),
                    None => return,
                };
                let next = apply_crop_amount(self.crop, direction, delta, w, h);
                self.crop = next;
                if let Some(selected) = &self.selected {
                    self.roll.set_crop(selected, next);
                }
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_crop(next);
                }
                self.reclamp_detail_zoom();
            }
            // A display rotation steps one quarter-turn counter-clockwise
            // (authoring the composite of the crop + the EXIF-upright frame;
            // the crop margins themselves are untouched). Live-only until the
            // trim key's release commits, mirroring the numeric adjusts.
            EditAdjust::RotateCcw => {
                self.apply_rotate_ccw();
            }
        }
    }

    /// Rotates the open frame's display one quarter-turn counter-clockwise by
    /// writing the RAM roll edit and the live GPU shader uniform. Shared by the
    /// keyboard shortcut (via [`Self::apply_edit_adjust`]) and the editing
    /// drawer button (which additionally persists via [`Self::commit_edit`]).
    fn apply_rotate_ccw(&mut self) {
        self.rotation = (self.rotation + 1) & 3;
        if let Some(selected) = &self.selected {
            self.roll.set_rotation(selected, self.rotation);
        }
        if let Some(shader) = &mut self.detail_shader {
            shader.set_rotation(self.rotation);
        }
        self.reclamp_detail_zoom();
    }

    /// Copies the focused frame's full edit (exposure + tone curve) to the
    /// edit clipboard for a later [`Self::paste_edits`]. The focus is the
    /// primary grid frame (`frame_selected`), falling back to the open detail
    /// frame `selected`. No-op when there is nothing focused/selected.
    fn copy_edits(&mut self) {
        let source = self.frame_selected.as_deref().or(self.selected.as_deref());
        if let Some(source) = source {
            self.clipboard = Some(self.roll.tone(source));
        }
    }

    /// Pastes the copied edit onto every multi-selected frame (or the focused
    /// frame when nothing else is selected): writes each target's full edit to
    /// the RAM manifest, updates the live shader to the pasted values when the
    /// open detail frame is a target, persists, and re-bakes the affected grid
    /// thumbnails (plus the roll cover if it is one of them).
    fn paste_edits(&mut self) -> Task<cosmic::Action<Message>> {
        let Some(tone) = self.clipboard else {
            return Task::none();
        };

        // Target frames: the multi-selection, else the focused (or open) frame.
        let mut targets: Vec<String> = self
            .selected_frames
            .iter()
            .cloned()
            .chain(
                self.frame_selected
                    .clone()
                    .into_iter()
                    .chain(self.selected.clone().into_iter()),
            )
            .collect();
        targets.sort();
        targets.dedup();
        if targets.is_empty() {
            return Task::none();
        }

        // Apply to the RAM manifest for every target.
        for name in &targets {
            self.roll.set_tone(name, tone);
        }

        // If the live detail frame is a target, sync the on-screen preview
        // state and the GPU shader to the pasted values.
        if let Some(open) = self.selected.as_deref() {
            if targets.iter().any(|name| name == open) {
                self.exposure_ev = tone.exposure_ev;
                self.curve_contrast = tone.curve_contrast;
                self.curve_rolloff = tone.curve_rolloff;
                self.curve_shadows = tone.curve_shadows;
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_exposure(tone.exposure_ev);
                    shader.set_curve(tone.curve_contrast, tone.curve_rolloff, tone.curve_shadows);
                }
            }
        }

        self.persist_roll();

        // Re-bake the affected tiles so the grid reflects the new edits.
        let mut tasks = Vec::with_capacity(2);
        for name in &targets {
            if let Some(tile) = self.tiles.iter_mut().find(|tile| &tile.name == name) {
                tile.thumb = Thumb::Loading;
            }
        }
        tasks.push(self.decode_next());
        // If the open roll's cover is among the targets, its library card also
        // re-bakes to mirror the edit.
        if let Some(active) = self.active.as_ref() {
            if let Some(roll) = self.rolls.iter_mut().find(|roll| {
                roll.dir == *active
                    && roll
                        .cover
                        .as_deref()
                        .is_some_and(|cover| targets.iter().any(|t| t == cover))
            }) {
                roll.thumb = Thumb::Loading;
                tasks.push(self.decode_covers());
            }
        }
        Task::batch(tasks)
    }

    /// Removes a roll from the library, dropping its directory from the
    /// persisted config so it stops appearing as a roll card.
    ///
    /// Non-destructive and reversible: the on-disk files and the roll's edit
    /// manifest are left untouched — only the library listing changes, so the
    /// roll can be re-added later without losing edits. No-ops when the roll
    /// is not in the library (e.g. a stale reference) or while it is open.
    fn remove_roll(&mut self, dir: PathBuf) -> Task<cosmic::Action<Message>> {
        // Only the library page owns the roll list; an open roll cannot be
        // removed from under its frame grid.
        if self.active.is_some() {
            return Task::none();
        }
        let removed = self.rolls.iter().any(|roll| roll.dir == dir);
        if !removed {
            return Task::none();
        }

        self.config
            .rolls
            .retain(|candidate| Path::new(candidate) != dir.as_path());
        self.persist_config();
        self.rolls.retain(|roll| roll.dir != dir);

        // If the removed roll was selected (driving the RollInfo drawer and
        // the keyboard highlight), clear the selection so no stale target
        // remains and close that drawer.
        if matches!(self.library_selection.as_ref(), Some(LibrarySelection::Roll(sel)) if *sel == dir)
        {
            self.library_selection = None;
            if self.context_page == ContextPage::RollInfo {
                self.core_mut().set_show_context(false);
                self.drawer_memory.set(DrawerView::Library, false);
            }
        }
        // Keep a selection alive: fall back to the first remaining roll when
        // more exist.
        self.select_first_visible_roll();
        Task::none()
    }

    /// Handle the completion of a hi-res detail decode, applying the result
    /// only if it matches the current selection and re-pumping if superseded.
    fn handle_detail_ready(
        &mut self,
        name: &str,
        preset: FilmPreset,
        result: Result<DetailDecode, ()>,
    ) -> Task<cosmic::Action<Message>> {
        if detail_result_is_current(
            self.selected.as_deref(),
            self.detail_inflight.as_deref(),
            name,
        ) {
            // Only the first load of an image restarts the thumbnail
            // crossfade; a native level-up swap must not re-flash the
            // (normally faded) thumb over the fresh hi-res texture.
            let fresh_open = self.detail_shader.is_none();
            let mut landed = false;
            match result {
                Ok(DetailDecode {
                    mono,
                    width,
                    height,
                    src_long_edge,
                    inversion,
                }) => {
                    landed = true;
                    detail_trace(format_args!(
                        "arrived ok: {name} {}x{} (src long edge {src_long_edge}), fresh={fresh_open}, zoom={:.3}",
                        width, height, self.detail_zoom
                    ));
                    // Stash a fresh overview decode into the LRU so returning
                    // to this frame is instant. Only the level-1 overview is
                    // cached (never a native level-up swap). Edits are applied
                    // in-shader at open time, so the cached mono stays valid
                    // across edit changes. Any evicted entry drops here, freeing
                    // its memory eagerly.
                    if fresh_open && let Some(dir) = self.active.clone() {
                        // Bound so the evicted entry (if any) is dropped here,
                        // freeing its memory eagerly; the value is otherwise
                        // unused.
                        let _evicted = self.detail_cache.insert(
                            (dir.clone(), preset, name.to_string()),
                            DetailMono {
                                mono: mono.clone(),
                                width,
                                height,
                                src_long_edge,
                                inversion,
                            },
                        );
                    }
                    // Cache the current thumbnail so it stays visible over the
                    // shader during the crossfade (first load only — a level-up
                    // swap must not re-insert the faded thumb).
                    if fresh_open
                        && let Some(tile) =
                            self.tiles.iter().find(|t| t.name == name).and_then(|t| {
                                match &t.thumb {
                                    Thumb::Ready(h) => Some(h.clone()),
                                    _ => None,
                                }
                            })
                    {
                        self.detail_thumb = Some(tile);
                    }
                    let image_id = self.next_image_id;
                    self.next_image_id = self.next_image_id.wrapping_add(1);
                    self.detail_shader = Some(shader::DetailProgram::new(
                        mono,
                        width,
                        height,
                        self.exposure_ev,
                        self.crop,
                        self.rotation,
                        image_id,
                        src_long_edge,
                        inversion,
                    ));
                    // Carry over any zoom/pan the user applied while the decode
                    // was in flight (the program starts at contain fit).
                    if let Some(shader) = &mut self.detail_shader {
                        shader.set_view(self.detail_zoom, self.detail_pan);
                        shader.set_curve(
                            self.curve_contrast,
                            self.curve_rolloff,
                            self.curve_shadows,
                        );
                        shader.set_crop(self.crop);
                        shader.set_rotation(self.rotation);
                    }
                    // The native texture widens the 1:1 cap; re-derive it.
                    self.reclamp_detail_zoom();
                    // The level-up is one-shot: a decode that lands after the
                    // first shader IS the native one, and an overview that was
                    // decoded at its native resolution (sensor long edge ≤
                    // `HI_RES_SIZE`) is already full-res — in both cases there
                    // is nothing more to decode. The sensor's true long edge
                    // (post-crop, pre-downscale) is the source of truth here,
                    // NOT the capped overview width: a large sensor downscaled
                    // just under 2048 must still level up to native.
                    if !fresh_open || src_long_edge <= HI_RES_SIZE {
                        self.detail_native_queued = true;
                    }
                }
                Err(()) => {
                    detail_trace(format_args!(
                        "arrived err: {name}, fresh={fresh_open}, zoom={:.3}",
                        self.detail_zoom
                    ));
                }
            }
            if fresh_open {
                self.detail_thumb_opacity = 1.0;
                self.detail_last_frame = None;
            }
            self.detail_inflight = None;

            // Landing-time re-pump: a wheel taken while a decode was running
            // fired the trigger into an occupied slot and it was swallowed.
            // If the user is already past the threshold once a decode lands,
            // start the next level onto the just-freed slot — so zooming
            // during the overview load still reaches native. Gated on the
            // landing having succeeded: an error must not spin the slot.
            if landed && !self.detail_native_queued && self.detail_zoom >= NATIVE_ZOOM_THRESHOLD {
                detail_trace(format_args!(
                    "landing re-pump: zoom {:.3} >= {NATIVE_ZOOM_THRESHOLD}",
                    self.detail_zoom
                ));
                return self.decode_detail_next();
            }
        } else {
            detail_trace(format_args!("arrived superseded: {name}"));

            // A superseded decode finished and freed the single slot;
            // start the current selection's queued request, if any.
            self.detail_inflight = None;

            return self.decode_detail_next();
        }

        Task::none()
    }
}

/// Logs a detail-pump trace line while `EXPOSURE_TRACE_DETAIL` is set.
///
/// Gated on the env var (read per call, so the default build pays nothing)
/// and allocation-free via [`std::fmt::Arguments`]; used to diagnose the
/// progressive two-level detail decode — trigger fire/swallow, arrival, and
/// landing re-pump.
fn detail_trace(args: std::fmt::Arguments<'_>) {
    if std::env::var("EXPOSURE_TRACE_DETAIL").is_ok() {
        eprintln!("[detail] {args}");
    }
}

/// Logs a thumbnail re-bake / frame-decode trace line while
/// `EXPOSURE_TRACE_REBAKE` is set. Mirrors the `detail_trace` gate so the
/// default build pays nothing; used to diagnose why a committed edit (crop or
/// tone) is not showing up in the grid thumbnail / roll cover.
fn rebake_trace(args: std::fmt::Arguments<'_>) {
    if std::env::var("EXPOSURE_TRACE_REBAKE").is_ok() {
        eprintln!("[rebake] {args}");
    }
}

/// Loads one roll's metadata: display name (directory leaf), cover file (first
/// sorted non-dot file), the count of frame files, and the film preset recorded
/// in the roll's edit manifest (defaulting to the non-inverted `None`) — with
/// nothing decoded yet.
async fn load_roll(dir: PathBuf) -> Roll {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| dir.to_string_lossy().into_owned(), str::to_string);
    let (cover, frame_count) = roll_cover_and_count(&dir).await;
    let manifest = edit_manifest::load_roll_manifest(&dir);
    let start_date = manifest.start_date().map(str::to_owned);
    let end_date = manifest.end_date().map(str::to_owned);
    Roll {
        dir,
        name,
        cover,
        frame_count,
        preset: manifest.preset(),
        start_date,
        end_date,
        thumb: Thumb::Loading,
    }
}

/// Persists a roll's film preset to its edit manifest, the on-disk source of
/// truth for decodes after a restart. Only a non-default preset is written:
/// [`FilmPreset::Hp5Plus`] records its choice key, while the `None` default is
/// implicit in the key's absence — so a default raw scan keeps a clean
/// manifest, and a write failure degrades to a stderr report instead of
/// blocking the UI.
fn record_roll_preset(dir: &Path, preset: FilmPreset) {
    if preset == FilmPreset::default() {
        return;
    }
    let mut manifest = edit_manifest::load_roll_manifest(dir);
    manifest.set_preset(preset);
    if let Err(err) = edit_manifest::save_roll_manifest(dir, &manifest) {
        eprintln!(
            "failed to write roll manifest {}: {err}",
            edit_manifest::manifest_path(dir).display()
        );
    }
}

/// Persists a roll's start and optional end dates to its edit manifest, the
/// on-disk source of truth across restarts. Unlike [`record_roll_preset`] a
/// write always happens: a cleared field must be recorded as absent, so there
/// is no implicit-default shortcut here.
fn record_roll_dates(dir: &Path, start: Option<String>, end: Option<String>) {
    let mut manifest = edit_manifest::load_roll_manifest(dir);
    manifest.set_dates(start, end);
    if let Err(err) = edit_manifest::save_roll_manifest(dir, &manifest) {
        eprintln!(
            "failed to write roll manifest {}: {err}",
            edit_manifest::manifest_path(dir).display()
        );
    }
}

/// Parses a zero-padded ISO date (`YYYY-MM-DD`) with a real calendar month and
/// day (leap-year aware) into `(year, month, day)`. `None` for anything else —
/// including a non-zero-padded shape like `2024-5-9`.
fn parse_iso_date(s: &str) -> Option<(u32, u32, u32)> {
    let (year, rest) = s.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<u32>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return None;
    };
    // Require the zero-padded `YYYY-MM-DD` shape, not `2024-5-9`.
    if s.len() != 10 || month > 12 || month == 0 || day == 0 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    (day <= days).then_some((year, month, day))
}

/// Whether `s` is a valid zero-padded ISO date (`YYYY-MM-DD`) with a real
/// calendar month and day (leap-year aware). The pure validation gate for the
/// roll-info drawer's date fields; an empty string is handled as "clear" by
/// the caller and never reaches this check.
fn valid_iso_date(s: &str) -> bool {
    parse_iso_date(s).is_some()
}

/// Whether a roll's committed dates are coherent: either may be absent, but when
/// both are set the start date must not be after the end date. ISO `YYYY-MM-DD`
/// strings compare lexicographically == chronologically (zero-padded), so this
/// is a plain ordering check.
#[must_use]
fn roll_dates_valid(start: Option<&str>, end: Option<&str>) -> bool {
    match (start, end) {
        (Some(start), Some(end)) => start <= end,
        _ => true,
    }
}

/// Formats a roll's start/end ISO dates for the library card, de-duplicated:
/// `May 3 - 4 2026` (month- and year-unique), `May 30 - Jun 2 2026`
/// (year-unique, each side keeps its month), `Dec 30 2025 - Jan 2 2026`
/// (each side keeps its year), or a single `May 3 2026` when there is no end
/// date (or it equals the start). `months` supplies the localized short month
/// names; `None` is returned for any malformed date so the caller can fall
/// back to raw ISO text.
fn format_roll_card_dates(months: &[&str; 12], start: &str, end: Option<&str>) -> Option<String> {
    let (start_year, start_month, start_day) = parse_iso_date(start)?;
    let (end_year, end_month, end_day) = match end {
        Some(end) => parse_iso_date(end)?,
        None => (start_year, start_month, start_day),
    };
    let start_name = months[(start_month - 1) as usize];
    let end_name = months[(end_month - 1) as usize];
    let single = end.is_none() || (start_year, start_month, start_day) == (end_year, end_month, end_day);
    Some(match (single, start_year == end_year, start_month == end_month) {
        (true, _, _) => format!("{start_name} {start_day} {start_year}"),
        (false, true, true) => format!("{start_name} {start_day} - {end_day} {start_year}"),
        (false, true, false) => {
            format!("{start_name} {start_day} - {end_name} {end_day} {start_year}")
        }
        (false, false, _) => format!(
            "{start_name} {start_day} {start_year} - {end_name} {end_day} {end_year}"
        ),
    })
}

/// Loads roll metadata for each configured roll directory, de-duplicated and
/// sorted by display name. Name order is the stable backing order: the library
/// view derives its date-descending display order from it per render.
async fn load_rolls(rolls: Vec<String>) -> Vec<Roll> {
    let mut seen = HashSet::new();
    let mut loaded = Vec::with_capacity(rolls.len());
    for entry in rolls {
        let dir = PathBuf::from(entry);
        if !seen.insert(dir.clone()) {
            continue;
        }
        loaded.push(load_roll(dir).await);
    }
    loaded.sort_by(|a, b| a.name.cmp(&b.name));
    loaded
}

/// Whether `name` looks like an output of this app's exporter (JPEG or PNG)
/// rather than a source negative. Negatives are RAW files (none of them use
/// these extensions), while exporting a roll into its own folder would
/// otherwise make every shipped file show back up as a fake frame — and,
/// without this, even become the roll's cover, which no scan ever searches
/// this directory for.
#[must_use]
fn is_export_artifact(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png"))
}

/// Scans a roll directory once: returns the first regular non-dot frame file
/// name in sorted order — the roll's cover, if it has any negatives yet —
/// alongside the count of frame files (both `None`/0 for a missing or empty
/// directory). A single pass covers the cover thumbnail and the
/// metadata-drawer frame count. Export artifacts (JPEG/PNG) are not frames.
async fn roll_cover_and_count(dir: &Path) -> (Option<String>, usize) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return (None, 0);
    };

    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|ty| ty.is_file())
            && let Some(name) = entry.file_name().into_string().ok()
            && !name.starts_with('.')
            && !is_export_artifact(&name)
        {
            files.push(name);
        }
    }

    let count = files.len();
    files.sort();
    (files.into_iter().next(), count)
}

/// Scans a roll directory for its frame files and returns their sorted names.
/// Dotfiles (including the edit manifest) and export artifacts (JPEG/PNG) are
/// never shown as tiles.
async fn load_files_in(dir: PathBuf) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };

    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|ty| ty.is_file())
            && let Some(name) = entry.file_name().into_string().ok()
            && !name.starts_with('.')
            && !is_export_artifact(&name)
        {
            files.push(name);
        }
    }

    files.sort();
    files
}

/// Rolls whose name matches the toolbar query (case-insensitive substring).
/// Shared by the library view and arrow-key navigation so both move over the
/// same visible set.
fn filtered_rolls<'a>(rolls: &'a [Roll], query: &str) -> Vec<&'a Roll> {
    let query = query.trim().to_lowercase();
    rolls
        .iter()
        .filter(move |roll| query.is_empty() || roll.name.to_lowercase().contains(&query))
        .collect()
}

/// Orders two rolls for the library grid: undated rolls lead (by name), then
/// dated rolls newest-start-date first. ISO `YYYY-MM-DD` start dates compare
/// lexicographically == chronologically; name decides ties so the order is
/// deterministic. The sort is derived per-view, so a roll's committed date
/// reorders it on the next render — never mutating `rolls`.
fn roll_date_cmp(a: &Roll, b: &Roll) -> Ordering {
    match (a.start_date.as_deref(), b.start_date.as_deref()) {
        (Some(a_date), Some(b_date)) => b_date.cmp(a_date).then_with(|| a.name.cmp(&b.name)),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.name.cmp(&b.name),
    }
}

/// A selectable cell in the library grid: the always-first Add Roll tile, or a
/// search-filtered roll. One type so rendering and arrow-key navigation walk
/// the same set, keeping the Add Roll tile selectable just like a roll card.
enum LibraryCell<'a> {
    /// The Add Roll tile (grid cell 0).
    AddRoll,
    /// A real roll card.
    Roll(&'a Roll),
}

impl LibraryCell<'_> {
    /// The [`LibrarySelection`] this cell carries, for storing a picked cell.
    fn selection(&self) -> LibrarySelection {
        match self {
            LibraryCell::AddRoll => LibrarySelection::AddRoll,
            LibraryCell::Roll(roll) => LibrarySelection::Roll(roll.dir.clone()),
        }
    }
}

/// Every selectable cell in the library grid: the Add Roll tile always first,
/// then the rolls whose name matches the query — undated rolls leading, the
/// dated ones newest-first (see [`roll_date_cmp`]). Since the add tile is
/// always present, the returned slice is never empty.
fn library_cells<'a>(rolls: &'a [Roll], query: &str) -> Vec<LibraryCell<'a>> {
    // The Add Roll tile leads the grid only while no search is active: while
    // searching, only the matching rolls are shown (and `cells` may be empty).
    let mut cells: Vec<LibraryCell<'a>> = Vec::with_capacity(rolls.len().saturating_add(1));
    if query.trim().is_empty() {
        cells.push(LibraryCell::AddRoll);
    }
    let mut matching = filtered_rolls(rolls, query);
    matching.sort_by(|a, b| roll_date_cmp(a, b));
    cells.extend(matching.into_iter().map(LibraryCell::Roll));
    cells
}

/// The index of `selection` within `cells`, if it names a cell in the set.
/// None means nothing is selected (or the selection left the set).
fn library_cell_index(
    selection: Option<&LibrarySelection>,
    cells: &[LibraryCell<'_>],
) -> Option<usize> {
    let selection = selection?;
    cells.iter().position(|cell| cell.selection() == *selection)
}

/// The add-roll dialog's "film preset" choice: whether the chosen folder holds
/// already-positive scans (regular RAWs, the non-inverted default) or HP5+
/// negatives. The response returns the selected key, which the picker resolves
/// back into a [`FilmPreset`].
#[must_use]
fn roll_preset_choice() -> cosmic::dialog::file_chooser::Choice {
    cosmic::dialog::file_chooser::Choice::new("preset", &fl!("preset-label"), "none")
        .insert("none", &fl!("preset-none"))
        .insert("hp5", ACTIVE_STOCK.name)
}

/// Opens the system folder picker, and on success emits [`Message::RollAdded`]
/// for the chosen directory (a cancel or portal failure is a no-op). The dialog
/// carries the film-preset choice, which lands in the [`Message::RollAdded`]
/// payload alongside the directory. Shared by the double-click handler and
/// Enter on a selected Add Roll tile.
fn open_roll_picker() -> Task<cosmic::Action<Message>> {
    cosmic::task::future(async {
        let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
            .choice(roll_preset_choice())
            .open_folder();
        match dialog.await {
            Ok(response) => {
                let Ok(dir) = response.url().to_file_path() else {
                    return Message::Ignore;
                };
                // The response lists the selected (id, value) pairs; the
                // "preset" choice carries the preset key. A backend that
                // dropped the choice falls back to the default (None).
                let preset = response
                    .choices()
                    .iter()
                    .find(|(id, _)| id == "preset")
                    .map_or(FilmPreset::default(), |(_, value)| {
                        FilmPreset::from_key(value.as_str())
                    });
                Message::RollAdded(dir, preset)
            }
            // Cancelled (or a portal failure) is a no-op.
            Err(_) => Message::Ignore,
        }
    })
}

/// The exported file name for a frame: the source file name with its extension
/// replaced by the format's (e.g. `img_0001.cr2` → `img_0001.jpg` or
/// `img_0001.png`). A file name with no extension simply gains the extension; a
/// hidden leading dot is part of the stem.
#[must_use]
fn export_name(name: &str, format: ExportFormat) -> PathBuf {
    let mut out = PathBuf::from(name);
    out.set_extension(format.ext());
    out
}

/// A deterministic 6-hex-char fingerprint of a roll's directory: FNV-1a over
/// the path's lossy bytes, masked to 24 bits and lowercased. Fixed-width so
/// filenames built from it sort lexicographically; specified independently of
/// Rust (`std::hash::DefaultHasher` is not stable across versions, so it is
/// never used here) and identical on every platform/run — the same directory,
/// however it was spelled, always yields the same id.
#[must_use]
fn roll_hash(dir: &std::path::Path) -> String {
    let mut hash = 0x811c_9dc5u32;
    for &byte in dir.to_string_lossy().as_bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// The dated export file name for a frame of a dated roll:
/// `YYYYMMDD-<roll-hash>-<frame>.<ext>` — e.g. `20240509-1a2b3c-01.jpg` — where
/// `YYYYMMDD` is the roll's start date (the same prefix for every frame, so
/// rolled shoots group under one day), `<roll-hash>` is the roll's 6-hex
/// [`roll_hash`] (the second sort key, so same-day rolls stay grouped when
/// filenames sort), and `<frame>` is the frame's 1-based full-roll position
/// padded to two digits (widening naturally past 99). Uses `format.ext()`.
/// Returns `None` when `start_iso` is not exactly `YYYY-MM-DD` or the position
/// does not fit a `u16` frame.
#[must_use]
fn dated_export_name(
    format: ExportFormat,
    start_iso: &str,
    index: usize,
    roll_hash: &str,
) -> Option<PathBuf> {
    let iso = start_iso.as_bytes();
    if iso.len() != 10 || iso[4] != b'-' || iso[7] != b'-' {
        return None;
    }
    let date = start_iso.get(..4)?.to_owned() + &start_iso[5..7] + &start_iso[8..10];
    let frame = u16::try_from(index).ok()? + 1;
    let mut out = PathBuf::from(format!("{date}-{roll_hash}-{frame:02}"));
    out.set_extension(format.ext());
    Some(out)
}

/// A same-directory sibling for an atomic export write: the encode goes to
/// `dest`'s `.tmp` neighbor and is renamed over `dest` once it is fully on
/// disk. Same filesystem, so the rename is atomic — a failed or interrupted
/// export leaves any pre-existing `dest` untouched instead of truncating it.
#[must_use]
fn temp_export_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map_or_else(|| "export".to_owned(), |n| n.to_string_lossy().into_owned());
    name.push_str(".tmp");
    dest.with_file_name(name)
}

/// The determinate fraction (0.0..=1.0) an export batch has reached after
/// `done` of `total` frames, for the header's progress ring.
#[must_use]
#[allow(clippy::cast_precision_loss)] // frame counts are far below f32's exact range
fn export_fraction(done: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        done as f32 / total as f32
    }
}

/// The index arrow-key navigation moves the selection to: `selected` as an
/// index into the visible cells (None = nothing selected yet), `len` visible
/// cells, `cols` grid columns. Left/Right step within the row and never wrap;
/// Up/Down step a full row, clamped to the first/last item.
fn nav_target(selected: Option<usize>, len: usize, cols: usize, dir: MoveDir) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cols = cols.max(1);
    Some(match selected {
        None => 0,
        Some(selected) => {
            let selected = selected.min(len - 1);
            match dir {
                MoveDir::Left => selected.saturating_sub(1),
                MoveDir::Right => (selected + 1).min(len - 1),
                MoveDir::Up => selected.saturating_sub(cols),
                MoveDir::Down => (selected + cols).min(len - 1),
            }
        }
    })
}

/// Frames whose name matches the toolbar query (case-insensitive substring).
/// Shared by the frame grid view and frame navigation so both move over the
/// same visible set.
fn filtered_tiles<'a>(tiles: &'a [Tile], query: &str) -> Vec<&'a Tile> {
    let query = query.trim().to_lowercase();
    tiles
        .iter()
        .filter(move |tile| query.is_empty() || tile.name.to_lowercase().contains(&query))
        .collect()
}

/// When a detail view is open, Left/Right step one frame through the visible,
/// search-filtered set. The step is clamped at both ends (no wrap): `None` when
/// there is no current frame anchored (or the list is empty), matching the
/// selection-driven grid nav. Up/Down never page.
fn paginate(current: usize, len: usize, dir: MoveDir) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = current.min(len - 1);
    match dir {
        MoveDir::Left => Some(current.saturating_sub(1)),
        MoveDir::Right => Some((current + 1).min(len - 1)),
        MoveDir::Up | MoveDir::Down => None,
    }
}

/// Updates the multi-selection after a frame tile is clicked, given the current
/// modifier state and the visible (filtered) `order` of frames.
///
/// - Plain click: replace the selection with just the clicked frame.
/// - Ctrl+click: toggle the clicked frame's membership.
/// - Shift+click: replace the selection with the inclusive range from the
///   `anchor` through the clicked frame (in `order`); a missing anchor or a
///   clicked frame outside `order` collapses to a plain single selection.
///
/// Returns the new selection set and the new shift-anchor (the clicked frame,
/// unless it was toggled out by a Ctrl+click, in which case the anchor is
/// unchanged so a later Shift+click still sources a valid frame).
fn apply_frame_click(
    mut selected: HashSet<String>,
    clicked: &str,
    ctrl: bool,
    shift: bool,
    anchor: Option<&str>,
    order: &[&Tile],
) -> (HashSet<String>, Option<String>) {
    // Index of the clicked frame in the visible order (None when filtered out).
    let click_idx = order.iter().position(|tile| tile.name == clicked);

    if shift {
        // Range: anchor through clicked, inclusive, in display order.
        if let Some(anchor) = anchor {
            if let (Some(click_idx), Some(anchor_idx)) =
                (click_idx, order.iter().position(|tile| tile.name == anchor))
            {
                let (lo, hi) = if anchor_idx <= click_idx {
                    (anchor_idx, click_idx)
                } else {
                    (click_idx, anchor_idx)
                };
                selected.clear();
                for tile in &order[lo..=hi] {
                    selected.insert(tile.name.clone());
                }
                return (selected, Some(clicked.to_owned()));
            }
        }
        // No usable anchor: fall through to a plain single selection.
    }

    if ctrl {
        if selected.contains(clicked) {
            selected.remove(clicked);
            // Keep the anchor stable (the toggled-off frame is gone from the
            // set but may still be the focus); return the old anchor.
            return (selected, anchor.map(str::to_owned));
        }
        selected.insert(clicked.to_owned());
        return (selected, Some(clicked.to_owned()));
    }

    // Plain click: single selection.
    selected.clear();
    selected.insert(clicked.to_owned());
    (selected, Some(clicked.to_owned()))
}

/// Column count for a fluid grid of `THUMB_SIZE` cells at `available` width,
/// mirroring iced's `Grid::fluid`/`Constraint::MaxWidth` math exactly
/// (`ceil((available + spacing) / (max + spacing))`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn grid_num_cols(available: f32, spacing: f32) -> usize {
    (((available + spacing) / (THUMB_SIZE + spacing)).ceil()) as usize
}

/// The absolute scroll offset (content-space y) that brings the tile at
/// `index` into the grid's visible viewport, or `None` when it is already
/// fully visible. Row geometry mirrors `Grid`: each square cell is `cell_width`
/// tall and rows advance by `cell_width + spacing`, with the grid inset by
/// `padding` inside the scrollable content.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_arguments
)]
fn reveal_target_y(
    cols: usize,
    index: usize,
    spacing: f32,
    padding: f32,
    cell_width: f32,
    viewport_height: f32,
    viewport_offset_y: f32,
    content_height: f32,
) -> Option<f32> {
    if cols == 0 {
        return None;
    }
    let row = index / cols;
    let top = padding + row as f32 * (cell_width + spacing);
    let bottom = top + cell_width;

    let target = if top < viewport_offset_y {
        top
    } else if bottom > viewport_offset_y + viewport_height {
        bottom - viewport_height
    } else {
        return None;
    };

    // Clamp to the scroller's range so an estimate can never overshoot.
    let max = (content_height - viewport_height).max(0.0);
    Some(target.clamp(0.0, max))
}

/// Renders the library page: a responsive grid of selectable cells. The Add
/// Roll tile is always the first cell; when no rolls (or no matches) remain, a
/// centered hint overlays the empty space beside the still-present add tile.
fn library_view(app: &AppModel) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let cells = library_cells(&app.rolls, app.search.as_deref().unwrap_or(""));
    let selected_index = library_cell_index(app.library_selection.as_ref(), &cells);
    // The hint overlays only when a search filters every real roll away (the
    // Add Roll tile is hidden while searching, so `cells` can be empty).
    let empty = !cells
        .iter()
        .any(|cell| matches!(cell, LibraryCell::Roll(_)));

    let grid = Grid::with_children(cells.iter().enumerate().map(|(index, cell)| {
        let selected = selected_index == Some(index);
        match cell {
            LibraryCell::AddRoll => add_roll_tile(selected),
            LibraryCell::Roll(roll) => roll_tile(roll, selected),
        }
    }))
    .fluid(THUMB_SIZE)
    .height(grid::Sizing::AspectRatio(TILE_ASPECT))
    .spacing(space_s);

    let body = widget::scrollable(widget::container(grid).width(Length::Fill).padding(space_s))
        // Report the viewport so `Nav` can scroll the highlighted tile into
        // view when it moves beyond the visible area.
        .id(scrollable_id("rolls-grid"))
        .on_scroll(Message::GridViewport)
        .height(Length::Fill);

    // A truly empty library shows no hint (the Add Roll tile alone is
    // self-explanatory), and a grid with matching rolls shows none either —
    // only the search-mismatch case (rolls exist but none match) overlays.
    if app.rolls.is_empty() || !empty {
        return body.into();
    }

    // Nothing matches: keep the hint over the space the grid leaves empty.
    let hint = widget::container(widget::text(fl!("no-rolls-found")))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center);

    let mut page = Stack::with_capacity(2);
    page = page.push(body);
    page = page.push(hint);
    page.width(Length::Fill).height(Length::Fill).into()
}

/// The stable widget [`Id`] names the mounted grid's [`Scrollable`], the target
/// for keyboard scroll-into-view effects. Only one page's grid is mounted at a
/// time, so the ids never collide.
fn scrollable_id(name: &'static str) -> cosmic::iced::widget::Id {
    cosmic::iced::widget::Id::new(name)
}

/// The stable widget [`Id`] of the header's search input, so activating search
/// can focus it. It is only mounted while search is active.
fn search_input_id() -> cosmic::iced::widget::Id {
    cosmic::iced::widget::Id::new("search-input")
}

/// Renders an open roll's frame grid (search-filtered), with the detail view
/// overlaid on an opaque surface when a frame is selected.
///
/// The grid stays mounted (scroll position persists) under the detail surface
/// that captures input, so the detail view cannot leak wheel/clicks to it.
fn frames_view(app: &AppModel) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let matched = filtered_tiles(&app.tiles, app.search.as_deref().unwrap_or(""));

    let tiles: Element<'_, Message> = if matched.is_empty() {
        widget::container(widget::text(fl!("no-files")))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into()
    } else {
        let grid = Grid::with_children(
            matched
                .into_iter()
                .map(|tile| tile_view(tile, app.selected_frames.contains(&tile.name))),
        )
        .fluid(THUMB_SIZE)
        .height(grid::Sizing::AspectRatio(TILE_ASPECT))
        .spacing(space_s);

        widget::scrollable(widget::container(grid).width(Length::Fill).padding(space_s))
            .id(scrollable_id("frames-grid"))
            .on_scroll(Message::GridViewport)
            .height(Length::Fill)
            .into()
    };

    let mut page = Stack::with_capacity(1);
    page = page.push(tiles);

    if let Some(detail) = detail_view(app) {
        page = page.push(
            widget::container(
                MouseArea::new(detail)
                    .on_press(Message::Ignore)
                    .on_double_press(Message::Ignore)
                    .on_double_click(Message::Ignore)
                    .on_release(Message::Ignore)
                    .on_right_press(Message::Ignore)
                    .on_right_release(Message::Ignore)
                    .on_middle_press(Message::Ignore)
                    .on_middle_release(Message::Ignore)
                    .on_scroll(|_delta| Message::Ignore),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|theme| cosmic::iced::widget::container::Style {
                background: Some(cosmic::iced::Background::Color(
                    theme.cosmic().background(false).base.into(),
                )),
                ..Default::default()
            }),
        );
    }

    page.width(Length::Fill).height(Length::Fill).into()
}

/// Renders the editing panel for the context drawer.
///
/// Shown whenever a selection is active; the body is empty while the GPU
/// shader is still loading, since `detail_view` already shows the cached
/// thumbnail during that brief decode gap. The drawer pane supplies the
/// width and padding, so the panel fills the available space.
fn editing_panel(app: &AppModel) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    if app.detail_shader.is_none() {
        return widget::column::with_capacity(1)
            .push("Loading...")
            .spacing(space_s)
            .width(Length::Fill)
            .into();
    }

    let label = widget::text(fl!("exposure-label"));
    let slider = widget::slider(-3.0..=4.0, app.exposure_ev, Message::ExposureChanged)
        .step(0.01_f32)
        // A finished drag is an edit flush point.
        .on_release(Message::EditSave);

    // Tone-editing controls: three pivoted powers re-shape the GPU texture
    // via a uniform-only remap — contrast pivots at the image's measured
    // mid-gray, highlight rolloff at the measured white point, shadows at the
    // measured shadow anchor. Grid thumbnails are unaffected; every detail
    // open starts from the stored edits.
    let contrast_label = widget::text(fl!("contrast-label"));
    let contrast_slider = widget::slider(
        0.2..=3.0,
        app.curve_contrast,
        // When any slider moves, the other values travel along so the
        // remap always composes the full curve, not a half-updated one.
        move |contrast| Message::CurveChanged(contrast, app.curve_rolloff, app.curve_shadows),
    )
    .step(0.05_f32)
    // A finished drag is an edit flush point, like exposure.
    .on_release(Message::EditSave);
    let rolloff_label = widget::text(fl!("rolloff-label"));
    let rolloff_slider = widget::slider(0.2..=3.0, app.curve_rolloff, move |rolloff| {
        Message::CurveChanged(app.curve_contrast, rolloff, app.curve_shadows)
    })
    .step(0.05_f32)
    .on_release(Message::EditSave);
    let shadows_label = widget::text(fl!("shadows-label"));
    let shadows_slider = widget::slider(0.2..=3.0, app.curve_shadows, move |shadows| {
        Message::CurveChanged(app.curve_contrast, app.curve_rolloff, shadows)
    })
    .step(0.05_f32)
    .on_release(Message::EditSave);
    // Keyboard crop readout + arm hint. The four values are the live margins
    // removed from each edge in source pixels (T/R/B/L); a `Crop: T0 R0 B0 L0`
    // readout is the untrimmed frame. The hint doubles as the arm table (which
    // key trims which edge, and how to grow/nudge instead).
    let crop_label = widget::text(fl!("crop-summary"));
    let crop_readout = widget::text(format!(
        "T{} R{} B{} L{}",
        app.crop.top, app.crop.right, app.crop.bottom, app.crop.left
    ));
    // Manual crop controls: a labeled text field per edge (Top/Right/Bottom/
    // Left) in source pixels, committed on Enter. Editing one edge re-centers
    // the perpendicular pair to preserve the aspect ratio, matching the
    // keyboard trim. The fields are seeded from the draft strings (not the
    // committed margins) so typing doesn't fight the read-only view.
    let crop_top = crop_margin_field(
        fl!("crop-top"),
        app.crop_drafts.get(edit_manifest::CropDirection::Top),
        edit_manifest::CropDirection::Top,
    );
    let crop_right = crop_margin_field(
        fl!("crop-right"),
        app.crop_drafts.get(edit_manifest::CropDirection::Right),
        edit_manifest::CropDirection::Right,
    );
    let crop_bottom = crop_margin_field(
        fl!("crop-bottom"),
        app.crop_drafts.get(edit_manifest::CropDirection::Bottom),
        edit_manifest::CropDirection::Bottom,
    );
    let crop_left = crop_margin_field(
        fl!("crop-left"),
        app.crop_drafts.get(edit_manifest::CropDirection::Left),
        edit_manifest::CropDirection::Left,
    );
    let crop_hint = widget::text(fl!("crop-hint"));
    // View-only toggle: on shows the full uncropped frame on top of the zoomed
    // crop with everything outside the crop dimmed (to see where the crop
    // lands); off shows the plain zoomed crop. Never persisted.
    let crop_mask = widget::toggler(app.show_crop_mask)
        .on_toggle(|_| Message::ToggleCropMask)
        .label(fl!("crop-mask-toggle"));
    // Display rotation controls: a cumulative counter-clockwise quarter-turn
    // readout (0° / 90° / 180° / 270°) plus a button stepping it, on top of
    // the EXIF orientation like the crop bake. The keyboard (`r`) routes the
    // same step through the edit-key machinery; the button commits on press.
    let rotate_label = widget::text(fl!("orientation-label"));
    let rotate_readout = widget::text(format!("{}°", u32::from(app.rotation) * 90));
    let rotate_button = widget::button::standard(fl!("rotate-ccw")).on_press(Message::RotateCcw);
    let reset_all = widget::button::standard(fl!("reset-all")).on_press(Message::ResetAll);
    let reset_crop = widget::button::standard(fl!("reset-crop")).on_press(Message::ResetCrop);

    widget::column::with_capacity(20)
        .push(label)
        .push(slider)
        .push(contrast_label)
        .push(contrast_slider)
        .push(rolloff_label)
        .push(rolloff_slider)
        .push(shadows_label)
        .push(shadows_slider)
        .push(widget::divider::horizontal::default())
        .push(crop_label)
        .push(crop_readout)
        .push(crop_top)
        .push(crop_right)
        .push(crop_bottom)
        .push(crop_left)
        .push(crop_hint)
        .push(crop_mask)
        .push(widget::divider::horizontal::default())
        .push(rotate_label)
        .push(rotate_readout)
        .push(rotate_button)
        .push(
            widget::row::with_capacity(2)
                .push(reset_all)
                .push(reset_crop)
                .spacing(space_s),
        )
        .spacing(space_s)
        .width(Length::Fill)
        .into()
}

/// Parses the basic EXIF readout (dimensions, camera, ISO, shutter, aperture,
/// focal length, lens, capture date) from a RAW file. Runs on the blocking
/// pool when the frame-info drawer opens and is cached on the tile; `Err` for
/// files the EXIF reader cannot parse (non-TIFF-based formats, corrupt files).
///
/// `display_value()` already formats the fields nicely (`1/250`, `2.8`,
/// `50.0`, `2024-05-01 …`), so the panel renders them as strings directly.
fn load_frame_meta(dir: &Path, name: &str) -> Result<FrameMeta, ()> {
    let file = std::fs::File::open(dir.join(name)).map_err(|_| ())?;
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(file))
        .map_err(|_| ())?;

    let field = |tag: exif::Tag| -> Option<String> {
        exif.get_field(tag, exif::In::PRIMARY)
            .map(|value| value.display_value().to_string())
    };
    // CR2/DNG may carry the recorded dimensions on the pixel tags rather than
    // the plain IFD width/height, so prefer ImageWidth/ImageLength and fall
    // back to PixelXDimension/PixelYDimension.
    let dimension = |primary: exif::Tag, pixel: exif::Tag| field(primary).or_else(|| field(pixel));

    Ok(FrameMeta {
        width: dimension(exif::Tag::ImageWidth, exif::Tag::PixelXDimension),
        height: dimension(exif::Tag::ImageLength, exif::Tag::PixelYDimension),
        make: field(exif::Tag::Make),
        model: field(exif::Tag::Model),
        iso: field(exif::Tag::PhotographicSensitivity),
        exposure: field(exif::Tag::ExposureTime),
        aperture: field(exif::Tag::FNumber),
        focal: field(exif::Tag::FocalLength),
        lens: field(exif::Tag::LensModel),
        date: field(exif::Tag::DateTimeOriginal),
    })
}

/// The frame-info drawer body for the highlighted frame: its dimensions and
/// basic EXIF readout (the file name is the drawer title, set by the caller).
/// When the roll has a start date, a roll-derived "Original date" row leads the
/// panel with the frame's exact exported `DateTimeOriginal` (the roll's start
/// date plus the frame's full-roll offset in seconds, the same value the export
/// stamps). That row depends only on the roll, so it shows even while the lazy
/// EXIF parse is in flight or failed. Shows a loading placeholder while the
/// lazy parse is in flight and only rows for fields the file actually carries;
/// a failed parse renders a quiet hint.
fn frame_info_panel<'a>(app: &'a AppModel, name: &str) -> Element<'a, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    // The synthetic capture timestamp this frame will be stamped with on
    // export — the roll's start date plus the frame's full-roll position in
    // seconds (the tile order is the grid order, which is also the export
    // order). Only present when the roll carries a start date; independent of
    // the file's own EXIF.
    let dated = app
        .roll
        .start_date()
        .and_then(|start| {
            app.tiles
                .iter()
                .position(|tile| tile.name == name)
                .and_then(|index| exif_writer::shifted_datetime(start, index))
        });

    let tile = app.tiles.iter().find(|tile| tile.name == name);
    let meta_rows: Vec<Element<'_, Message>> = if let Some(meta) = tile.and_then(|t| t.meta.as_ref())
    {
        let mut rows = Vec::with_capacity(8);
        if let (Some(width), Some(height)) = (&meta.width, &meta.height) {
            rows.push(
                widget::text::body(fl!(
                    "frame-dimensions",
                    dimensions = format!("{width} × {height}")
                ))
                .into(),
            );
        }
        let camera = match (&meta.make, &meta.model) {
            (Some(make), Some(model)) => format!("{make} {model}"),
            (Some(make), None) => make.clone(),
            (None, Some(model)) => model.clone(),
            _ => String::new(),
        };
        if !camera.is_empty() {
            rows.push(widget::text::body(fl!("frame-camera", camera = camera)).into());
        }
        if let Some(value) = meta.iso.clone() {
            rows.push(widget::text::body(fl!("frame-iso", iso = value)).into());
        }
        if let Some(value) = meta.exposure.clone() {
            rows.push(widget::text::body(fl!("frame-exposure", exposure = value)).into());
        }
        if let Some(value) = meta.aperture.clone() {
            rows.push(widget::text::body(fl!("frame-aperture", aperture = value)).into());
        }
        if let Some(value) = meta.focal.clone() {
            rows.push(widget::text::body(fl!("frame-focal", focal = value)).into());
        }
        if let Some(value) = meta.lens.clone() {
            rows.push(widget::text::body(fl!("frame-lens", lens = value)).into());
        }
        if let Some(value) = meta.date.clone() {
            rows.push(widget::text::body(fl!("frame-date", date = value)).into());
        }
        rows
    } else if tile.is_some_and(|t| t.meta_failed) {
        vec![widget::text(fl!("frame-info-unavailable")).into()]
    } else {
        vec![widget::text(fl!("frame-info-loading")).into()]
    };

    // The roll-derived stamp leads the panel, above whatever the file itself
    // carries, so a frame with no (or unreadable) EXIF still shows the date it
    // would be exported with.
    let mut rows = Vec::with_capacity(meta_rows.len() + usize::from(dated.is_some()));
    if let Some(date) = dated {
        rows.push(widget::text::body(fl!("frame-original-date", date = date)).into());
    }
    rows.extend(meta_rows);

    widget::column::with_capacity(rows.len())
        .extend(rows)
        .spacing(space_s)
        .width(Length::Fill)
        .into()
}

/// A labeled text field for one crop edge margin (source pixels), committed on
/// Enter via [`Message::CropDraftSubmit`]. Seeded from the live draft so an
/// in-progress edit survives view re-renders.
fn crop_margin_field(
    label: String,
    value: &str,
    direction: edit_manifest::CropDirection,
) -> Element<'_, Message> {
    widget::column::with_capacity(2)
        .push(widget::text(label))
        .push(
            widget::text_input(fl!("crop-field-placeholder"), value)
                .width(Length::Fill)
                .on_input(move |v| Message::CropDraftChange(direction, v))
                .on_submit(move |_| Message::CropDraftSubmit(direction)),
        )
        .spacing(cosmic::theme::spacing().space_xs)
        .into()
}

/// A labeled ISO-date text field for the roll-info drawer, committed on Enter
/// via [`Message::RollDateDraftSubmit`]. Seeded from the live draft so an
/// in-progress edit survives view re-renders.
fn roll_date_field(
    label: String,
    value: &str,
    field: RollDateField,
) -> Element<'_, Message> {
    widget::column::with_capacity(2)
        .push(widget::text(label))
        .push(
            widget::text_input(fl!("roll-date-placeholder"), value)
                .width(Length::Fill)
                .on_input(move |v| Message::RollDateDraftChange(field, v))
                .on_submit(move |_| Message::RollDateDraftSubmit(field)),
        )
        .spacing(cosmic::theme::spacing().space_xs)
        .into()
}

/// Renders the roll metadata drawer: the roll name as heading, its full path,
/// frame count, and cover file, then the roll dates (start + optional end,
/// committed on Enter), the film preset, and the removal action. The drawer
/// pane supplies the width/padding.
fn roll_info_panel<'a>(
    roll: &'a Roll,
    start_draft: &'a str,
    end_draft: &'a str,
) -> Element<'a, Message> {
    let space_xs = cosmic::theme::spacing().space_xs;

    let remove = widget::button::destructive(fl!("remove-roll"))
        .on_press(Message::RemoveRoll(roll.dir.clone()));

    // The film preset selector: non-inverted (regular RAW) by default, or the
    // HP5+ negative profile. Index order must match `FilmPreset::index()`.
    let roll_dir = roll.dir.clone();
    let preset = widget::dropdown::dropdown(
        vec![fl!("preset-none"), ACTIVE_STOCK.name.to_owned()],
        Some(roll.preset.index()),
        move |index| Message::RollPresetChanged(roll_dir.clone(), FilmPreset::from_index(index)),
    )
    .width(Length::Fill);

    widget::column::with_capacity(14)
        .push(widget::text::heading(&roll.name))
        .push(widget::divider::horizontal::default())
        .push(widget::text::body(fl!(
            "roll-path",
            path = roll.dir.display().to_string()
        )))
        .push(widget::text::body(fl!(
            "roll-frames-line",
            frames = roll.frame_count.to_string()
        )))
        .push(widget::text::body(fl!(
            "roll-cover",
            cover = roll.cover.clone().unwrap_or_else(|| fl!("roll-no-cover"))
        )))
        .push(widget::divider::horizontal::default())
        .push(widget::text(fl!("roll-dates-label")))
        .push(roll_date_field(
            fl!("roll-date-start-label"),
            start_draft,
            RollDateField::Start,
        ))
        .push(roll_date_field(
            fl!("roll-date-end-label"),
            end_draft,
            RollDateField::End,
        ))
        .push(widget::divider::horizontal::default())
        .push(widget::text(fl!("preset-label")))
        .push(preset)
        .push(widget::divider::horizontal::default())
        .push(remove)
        .spacing(space_xs)
        .width(Length::Fill)
        .into()
}

/// Renders the library grid's first tile: an "Add Roll" card that opens the
/// folder picker on double-click. Mirrors a roll card's surface, radius, and
/// selection ring so it reads and behaves as another tile — single-clicking
/// selects it (highlight), double-clicking (or Enter) opens the picker — with
/// a large plus icon.
fn add_roll_tile(selected: bool) -> Element<'static, Message> {
    let content = widget::container(
        icon::from_name("list-add-symbolic")
            .size(130)
            .icon()
            .opacity(0.50),
    )
    .align_x(Horizontal::Center)
    .align_y(Vertical::Center)
    .height(Length::Fill)
    .width(Length::Fill);

    // Single-click selects the tile (highlight only); double-click opens the
    // folder picker.
    let card: Element<'_, Message> = MouseArea::new(content)
        .on_press(Message::AddRollSelected)
        .on_double_click(Message::AddRoll)
        .into();

    selectable_tile(card, selected)
}

/// Renders a library page roll card, filling the square cell the grid assigns
/// it. Single-clicking anywhere on the card selects the roll; double-clicking
/// (or Enter) drills into its frame grid. The selection highlight is an accent
/// ring drawn OVER the card (see the tail of this function), so the full-bleed
/// cover never hides it.
fn roll_tile(roll: &Roll, selected: bool) -> Element<'_, Message> {
    let space_xs = cosmic::theme::spacing().space_xs;
    // The tile's corner radius follows the COSMIC system Roundness setting
    // (rounded / slightly-rounded / square) exactly as `Container::Primary`
    // resolves it, so the full-bleed cover's top corners clip to the tile's
    // own curves.
    let radius = cosmic::theme::active().cosmic().corner_radii.radius_s[0];

    // The cover runs full-bleed: flush against the card's top edge with no
    // padding, and clipped to the tile's corners — the wgpu image renderer
    // discards fragments outside the rounded box, so the curves genuinely cut
    // into the photo. Only the TOP corners are rounded: iced-wgpu's image
    // shader applies the radius vec rotated 180° from the `border::Radius`
    // struct order (the first two entries render on the bottom), so the top
    // clip is expressed with `bottom()`. The cover's bottom edge is interior,
    // so its bottom corners stay square. Crop-to-fill (`ContentFit::Cover`)
    // makes any aspect fill the area: landscape crops almost nothing, portrait
    // center-crops the top/bottom. Placeholders stay centered in the same
    // sheet.
    let content: Element<'_, Message> = match &roll.thumb {
        Thumb::Ready(handle) => widget::image(handle.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(ContentFit::Cover)
            .border_radius(cosmic::iced::border::Radius::default().bottom(radius))
            .into(),
        Thumb::Loading => widget::container(icon::from_name("image-loading-symbolic").icon())
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into(),
        Thumb::Failed => widget::container(icon::from_name("image-missing-symbolic").icon())
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into(),
    };

    // Title (flush-left) plus a single caption row: the frame count in its own
    // left column and, when a start date is set, the date (or start – end
    // range) in a right column — a display-only mirror of the roll-info
    // drawer's dates. Both sit in a block padded 12px (`space_xs`) away from
    // the card's edges; its top padding is also the gap below the full-bleed
    // cover. The two `Length::Fill` row cells split the row into equal columns;
    // an undated roll simply leaves the right cell out.
    let mut meta = widget::row::with_capacity(2);
    meta = meta.push(
        widget::container(widget::text::caption(fl!(
            "roll-frames",
            count = roll.frame_count
        )))
        .width(Length::Fill)
        .align_x(Horizontal::Left),
    );
    if let Some(start) = &roll.start_date {
        let month_names: [String; 12] = [
            fl!("month-01"),
            fl!("month-02"),
            fl!("month-03"),
            fl!("month-04"),
            fl!("month-05"),
            fl!("month-06"),
            fl!("month-07"),
            fl!("month-08"),
            fl!("month-09"),
            fl!("month-10"),
            fl!("month-11"),
            fl!("month-12"),
        ];
        let months: [&str; 12] = std::array::from_fn(|i| month_names[i].as_str());
        let dates = widget::text::caption(
            match format_roll_card_dates(&months, start, roll.end_date.as_deref()) {
                // Malformed dates (hand-edited manifests) keep today's raw ISO
                // display instead of inventing a format.
                None => match &roll.end_date {
                    Some(end) => fl!("roll-card-dates", start = start.clone(), end = end.clone()),
                    None => fl!("roll-card-date", start = start.clone()),
                },
                Some(formatted) => formatted,
            },
        );
        meta = meta.push(
            widget::container(dates)
                .width(Length::Fill)
                .align_x(Horizontal::Right),
        );
    }
    let info: Element<'_, Message> = widget::container(
        widget::column::with_capacity(2)
            .push(widget::text(&roll.name))
            .push(meta)
            .spacing(space_xs)
            .align_x(Horizontal::Left),
    )
    .width(Length::Fill)
    .padding(space_xs)
    .into();

    // The whole card is one interactive surface — selectable even while its
    // cover is still decoding, openable by double-click anywhere, not just the
    // image.
    let card = widget::column::with_capacity(2)
        .push(content)
        .push(info)
        .spacing(0);

    let card: Element<'_, Message> = MouseArea::new(card)
        .on_press(Message::RollSelected(roll.dir.clone()))
        .on_double_click(Message::RollActivated(roll.dir.clone()))
        .into();

    selectable_tile(card, selected)
}

/// Wraps a library tile's interactive content in the shared selection chrome: a
/// `Container::Primary` surface (background only, its radius follows the theme)
/// with an accent ring drawn ON TOP when selected. The selection is NOT a card
/// style here: iced paints a container's border behind its children, which a
/// full-bleed cover would cover up, so the ring is a separate overlay layer.
fn selectable_tile(content: Element<'_, Message>, selected: bool) -> Element<'_, Message> {
    let surface = widget::container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .class(cosmic::theme::Container::Primary);

    let mut stack = Stack::with_capacity(1);
    stack = stack.push(surface);

    // Selection: an accent ring drawn ON TOP of this card.
    if selected {
        stack = stack.push(selection_ring());
    }

    stack.width(Length::Fill).height(Length::Fill).into()
}

/// The selection highlight shared by every selectable tile (library rolls and
/// open-roll frames): a transparent overlay ring drawn ON TOP of the card,
/// rounded to the theme radius, so the accent border is visible around a
/// full-bleed preview on every side. Decorative only — no `MouseArea` — so it
/// never eats the clicks the card below expects.
fn selection_ring() -> Element<'static, Message> {
    widget::container(
        widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .class(cosmic::theme::Container::custom(|theme| {
        cosmic::iced::widget::container::Style {
            border: cosmic::iced::border::Border {
                color: theme.cosmic().accent.base.into(),
                width: 2.0,
                radius: theme.cosmic().corner_radii.radius_s.into(),
            },
            ..Default::default()
        }
    }))
    .into()
}

/// Renders a single frame tile of the open roll, filling the square cell the
/// grid assigns it. Single-clicking selects (accent ring highlight); a
/// double-click (or Enter on the highlighted tile) opens the detail view. Like
/// the library cards, the tile is a surface + Stack with the ring overlay; the
/// image itself stays `Contain` so a film frame's full framing is never cropped.
fn tile_view(tile: &Tile, selected: bool) -> Element<'_, Message> {
    // The tile is the image alone in its square cell with ContentFit::Contain
    // (no cropping); placeholders stay centered in the same sheet. The frame
    // name now lives in the frame-info context drawer (its title), not on the
    // tile.
    let content: Element<'_, Message> = match &tile.thumb {
        Thumb::Ready(handle) => widget::image(handle.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(ContentFit::Contain)
            .into(),
        Thumb::Loading => widget::container(icon::from_name("image-loading-symbolic").icon())
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into(),
        Thumb::Failed => widget::container(icon::from_name("image-missing-symbolic").icon())
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into(),
    };

    let card: Element<'_, Message> = MouseArea::new(content)
        .on_press(Message::FrameSelected(tile.name.clone()))
        .on_double_click(Message::ThumbnailActivated(tile.name.clone()))
        .into();

    let mut stack = Stack::with_capacity(1);
    stack = stack.push(
        widget::container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .class(cosmic::theme::Container::Primary),
    );

    if selected {
        stack = stack.push(selection_ring());
    }

    stack.width(Length::Fill).height(Length::Fill).into()
}

/// Renders the detail view for the selected file, if any, over the still-mounted
/// thumbnail grid (which the caller paints an opaque, theme-colored surface on).
///
/// Shows the cached thumbnail while the GPU shader is loading, then
/// crossfades: the thumbnail sits on top of the shader in a Stack and
/// fades out via `.opacity()`.
fn detail_view(app: &AppModel) -> Option<Element<'_, Message>> {
    let space_s = cosmic::theme::spacing().space_s;
    let name = app.selected.as_ref()?;
    let tile = app.tiles.iter().find(|tile| &tile.name == name)?;

    let preview: Element<'_, Message> = match (
        app.detail_shader.as_ref(),
        app.detail_thumb.as_ref(),
        app.detail_thumb_opacity > 0.0,
    ) {
        (Some(shader), Some(thumb), true) => {
            // Crossfade in progress — thumbnail fading out over shader.
            Stack::with_children([
                shader.view().into(),
                widget::image(thumb.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(ContentFit::Contain)
                    .opacity(app.detail_thumb_opacity)
                    .into(),
            ])
            .into()
        }
        (Some(_shader), _, _) => app
            .detail_shader
            .as_ref()
            .map(shader::DetailProgram::view)
            .unwrap()
            .into(),
        (None, _, _) => {
            // Shader not ready — show the cached thumbnail or a status icon.
            match &tile.thumb {
                Thumb::Ready(handle) => widget::image(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(ContentFit::Contain)
                    .into(),
                Thumb::Loading => icon::from_name("image-loading-symbolic").icon().into(),
                Thumb::Failed => icon::from_name("image-missing-symbolic").icon().into(),
            }
        }
    };

    // The shader widget is `Length::Fill × Length::Fill`; bounding it inside
    // a Fill × Fill container caps its widget bounds at the parent column's
    // allocation. The WGSL's `contained_uv` does `ContentFit::Contain` math so
    // the image letterbox/pillarbox-fits within those bounds with
    // transparent bars (alpha=0 + `BlendState::ALPHA_BLENDING`) — no overflow
    // into the header or any other widget.
    Some(
        widget::column::with_capacity(2)
            .push(
                // Interactive surface for the preview: wheel zooms, press+drag
                // pans. The outer full-surface `MouseArea` in `view` still
                // swallows events over the caption/bars and blocks the grid
                // behind; iced delivers to the inner widget first, so its
                // `capture_event()` wins and the outer handlers are skipped.
                // `DetailArea.on_move` reports cursor positions relative to
                // the widget center, matching the center-relative pan offset
                // the zoom-anchor math compares against.
                DetailArea::new(
                    widget::container(preview)
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .on_scroll(|delta| Message::DetailZoom(detail_zoom_delta(delta)))
                .on_resize(Message::DetailAreaResized)
                .on_press(Message::DetailPanPress)
                .on_move(Message::DetailPanMove)
                .on_release(Message::DetailPanRelease),
            )
            .spacing(space_s)
            .align_x(Horizontal::Center)
            .into(),
    )
}

/// Converts a wheel scroll delta into a detail-view zoom change (in `log2`
/// units, so +1 = double the rendered scale, −1 = halve it).
///
/// A single wheel notch (one line) zooms half a unit; trackpad pixel deltas
/// treat a ~400 px swipe as one full unit.
fn detail_zoom_delta(delta: cosmic::iced::mouse::ScrollDelta) -> f32 {
    match delta {
        cosmic::iced::mouse::ScrollDelta::Lines { y, .. } => y * 0.5,
        cosmic::iced::mouse::ScrollDelta::Pixels { y, .. } => y / 400.0,
    }
}

/// Applies a wheel zoom `delta` (log2 units) to the detail view, clamping to
/// `[1.0, max_zoom]` (the 1:1 "100%" cap, see [`AppModel::max_detail_zoom`]).
/// Zooming all the way back out to contain fit re-centers the image. Returns
/// the new `(zoom, pan)`.
fn apply_detail_zoom(
    zoom: f32,
    pan: (f32, f32),
    cursor: Option<Point>,
    delta: f32,
    max_zoom: f32,
) -> (f32, (f32, f32)) {
    let new_zoom = (zoom + delta).clamp(1.0, max_zoom);
    // At contain fit the whole frame must be centered. `zoom + delta <= 1.0`
    // is equivalent to `new_zoom == 1.0` because of the clamp above.
    let new_pan = if zoom + delta <= 1.0 {
        (0.0, 0.0)
    } else {
        cursor.map_or(pan, |cursor| zoom_about_anchor(zoom, new_zoom, pan, cursor))
    };
    (new_zoom, new_pan)
}

/// Computes the pan that keeps the image point under `cursor` fixed on
/// screen while the zoom changes from `zoom_old` to `zoom_new` (log2 units).
///
/// The rendered size is proportional to `2^(zoom-1)`, so the unknown
/// contain-fit scale cancels:
/// `off1 = off0 + (1 - 2^(z1-z0)) * (cursor - off0)`.
/// Both `pan` and `cursor` are relative to the widget center; the widget
/// center itself never enters the formula.
fn zoom_about_anchor(zoom_old: f32, zoom_new: f32, pan: (f32, f32), cursor: Point) -> (f32, f32) {
    let ratio = (zoom_new - zoom_old).exp2();
    let k = 1.0 - ratio;
    (
        pan.0 + k * (cursor.x - pan.0),
        pan.1 + k * (cursor.y - pan.1),
    )
}

/// Clamps an exposure adjustment in EV to the slider's range (−3.0..=+4.0).
fn clamp_ev(ev: f32) -> f32 {
    ev.clamp(-3.0, 4.0)
}

/// Clamps a tone-curve power (contrast/rolloff/shadows) to the slider's range
/// (0.2..=3.0) so a keyboard shortcut and the slider agree on bounds.
fn clamp_curve_power(power: f32) -> f32 {
    power.clamp(0.2, 3.0)
}

/// Derives the four source-pixel crop margins for trimming a single edge by
/// `amount_px`, keeping the frame's natural aspect ratio and auto-selecting the
/// anchor (the opposite edge's midpoint).
///
/// Trimming one edge cascades into the two perpendicular edges proportionally to
/// the aspect ratio, so the cropped region satisfies
/// `(w − left − right) / (h − top − bottom) == w / h` exactly. `amount_px` is
/// clamped to the largest trim that keeps a non-empty, non-inverted frame.
/// Test-only convenience: build margins from zero via [`apply_crop_amount`],
/// so a unit test can express "trim the bottom edge by 100" without carrying a
/// current-margin value.
#[cfg(test)]
#[allow(clippy::cast_possible_wrap)]
fn crop_margins(
    direction: edit_manifest::CropDirection,
    amount_px: u32,
    width: u32,
    height: u32,
) -> edit_manifest::CropMargins {
    apply_crop_amount(
        edit_manifest::CropMargins::default(),
        direction,
        amount_px as i32,
        width,
        height,
    )
}

/// Incrementally adjusts one edge's crop margin by `delta_px` (positive = trim
/// more / shrink the frame, negative = trim less / grow it), preserving the
/// natural aspect ratio and the anchor (the opposite edge stays fixed).
///
/// The chosen edge's margin accumulates on top of `current`; the two
/// perpendicular margins stay equal (centered) and are re-derived so the
/// cropped region keeps the source's aspect ratio:
///
/// - trimming **top/bottom**: `left = right = round(ar·(top+bottom)/2)`
/// - trimming **left/right**: `top = bottom = round((left+right)/(2·ar))`
///
/// with `ar = width/height`. `delta_px` and the resulting margin are clamped so
/// the frame never inverts or collapses to a zero-area region.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn apply_crop_amount(
    current: edit_manifest::CropMargins,
    direction: edit_manifest::CropDirection,
    delta_px: i32,
    width: u32,
    height: u32,
) -> edit_manifest::CropMargins {
    use edit_manifest::CropDirection::{Bottom, Left, Right, Top};

    let (ar, sum_limit_vertical, sum_limit_horizontal) = crop_limits(width, height);
    let (t, r, b, l) = (current.top, current.right, current.bottom, current.left);

    let clamped = match direction {
        Top => (clamp_axis(t, b, delta_px, sum_limit_vertical), r, b, l),
        Bottom => (t, r, clamp_axis(b, t, delta_px, sum_limit_vertical), l),
        Left => (t, r, b, clamp_axis(l, r, delta_px, sum_limit_horizontal)),
        Right => (t, clamp_axis(r, l, delta_px, sum_limit_horizontal), b, l),
    };

    recenter_perpendicular(
        edit_manifest::CropMargins {
            top: clamped.0,
            right: clamped.1,
            bottom: clamped.2,
            left: clamped.3,
        },
        direction,
        ar,
    )
}

/// Sets one edge's margin to an absolute non-negative source-pixel value
/// (clamped so the frame never inverts), then re-derives the perpendicular
/// pair to preserve aspect — the same aspect-lock rule the keyboard crop uses.
/// Backs the manual crop-margin fields: typing a value for one edge re-centers
/// the other axis to keep the natural ratio.
#[allow(clippy::cast_possible_truncation)]
fn set_crop_edge(
    current: edit_manifest::CropMargins,
    direction: edit_manifest::CropDirection,
    abs_px: u32,
    width: u32,
    height: u32,
) -> edit_manifest::CropMargins {
    use edit_manifest::CropDirection::{Bottom, Left, Right, Top};

    let (ar, sum_limit_vertical, sum_limit_horizontal) = crop_limits(width, height);
    let (t, r, b, l) = (current.top, current.right, current.bottom, current.left);

    // Clamp the edited edge to `[0, sum_limit − opposite]` so the parallel edge
    // never sums past the frame, exactly like `clamp_axis` does for a delta.
    let clamped = match direction {
        Top => (abs_px.min(sum_limit_vertical.saturating_sub(b)), r, b, l),
        Bottom => (t, r, abs_px.min(sum_limit_vertical.saturating_sub(t)), l),
        Left => (t, r, b, abs_px.min(sum_limit_horizontal.saturating_sub(r))),
        Right => (t, abs_px.min(sum_limit_horizontal.saturating_sub(l)), b, l),
    };

    recenter_perpendicular(
        edit_manifest::CropMargins {
            top: clamped.0,
            right: clamped.1,
            bottom: clamped.2,
            left: clamped.3,
        },
        direction,
        ar,
    )
}

/// The aspect ratio (`width/height`) and, per axis, the largest total of the
/// two parallel margins that leaves ≥ `MIN_LEFT` pixels of frame on both axes
/// after the perpendicular pair re-centers. Kept as source-pixel integers so
/// the aspect stays exactly expressible when the frame is comfortably large.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn crop_limits(width: u32, height: u32) -> (f64, u32, u32) {
    // Keep at least a couple of pixels of frame on both axes so integer
    // rounding of the perpendicular pair never collides with the edge.
    const MIN_LEFT: u32 = 2;
    let ar = f64::from(width) / f64::from(height.max(1));
    let sum_limit_vertical = ((((f64::from(width) - f64::from(MIN_LEFT)) / ar)
        .floor()
        .min(f64::from(height) - f64::from(MIN_LEFT)))
    .max(0.0)) as u32;
    let sum_limit_horizontal = (((f64::from(height) - f64::from(MIN_LEFT)) * ar)
        .floor()
        .min(f64::from(width) - f64::from(MIN_LEFT))
        .max(0.0)) as u32;
    (ar, sum_limit_vertical, sum_limit_horizontal)
}

/// Re-centers the perpendicular pair to preserve aspect: only the axis being
/// explicitly trimmed can carry asymmetric margins; the other axis is equal +
/// centered. The perpendicular TOTAL is rounded once, then split so the two
/// halves always sum back to that total.
fn recenter_perpendicular(
    crop: edit_manifest::CropMargins,
    direction: edit_manifest::CropDirection,
    ar: f64,
) -> edit_manifest::CropMargins {
    use edit_manifest::CropDirection::{Bottom, Left, Right, Top};
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (t, r, b, l) = match direction {
        Top | Bottom => {
            let total = ((ar * f64::from(crop.top + crop.bottom)).round().max(0.0)) as u32;
            (crop.top, total / 2, crop.bottom, total - total / 2)
        }
        Left | Right => {
            let total = ((f64::from(crop.left + crop.right) / ar).round().max(0.0)) as u32;
            (total / 2, crop.right, total - total / 2, crop.left)
        }
    };
    edit_manifest::CropMargins {
        top: t,
        right: r,
        bottom: b,
        left: l,
    }
}

/// Clamps a single edge margin after applying `delta_px`, keeping the edge in
/// `[0, sum_limit − opposite]` so a parallel-axis trim never overshoots the
/// frame (the two parallel edges sum to at most `sum_limit`).
fn clamp_axis(edge: u32, opposite: u32, delta_px: i32, sum_limit: u32) -> u32 {
    let max = sum_limit.saturating_sub(opposite);
    let raw = i64::from(edge) + i64::from(delta_px);
    raw.clamp(0, i64::from(max)).try_into().unwrap_or(u32::MAX)
}

/// Maps a keyboard shortcut character to an [`EditAdjust`], or `None` when the
/// key is not bound. `alt` and `shift` are the event's modifier state.
///
/// The four control pairs are laid out on a US keyboard left-to-right to match
/// the editing panel's control order (Exposure → Contrast → Rolloff → Shadows):
/// `-`/`=` exposure, `[`/`]` contrast, `;`/`'` rolloff, `,`/`.` shadows. A bare
/// key uses the coarse step; holding `Shift` selects the fine nudge step. The
/// crop edges map to movement keys `h`/`j`/`k`/`l` (Left/Bottom/Top/Right): a
/// bare edge key trims more (+`CROP_STEP_PX`), `Alt`+edge trims less
/// (−, clamped ≥ 0), and `Shift`(+`Alt`)+edge nudges by ±1px. `r` rotates the
/// display one quarter-turn counter-clockwise (cumulative; modifiers ignored).
/// The `key` payload
/// is deliberately layout-stable: iced's `keyboard::listen` delivers the
/// unmodified character (`key_without_modifiers`), and the iced fork never
/// reports the `Shift`-produced symbols from the base keys used here, so nudge
/// is driven by the event's modifier state rather than by matching
/// `_`/`+`/`{`/`}`/`:`/`"`/`<`/`>`.
fn edit_adjust_for(key: &str, alt: bool, shift: bool) -> Option<EditAdjust> {
    use edit_manifest::CropDirection::{Bottom, Left, Right, Top};
    let ev = if shift { EDIT_NUDGE_EV } else { EDIT_STEP_EV };
    let curve = if shift {
        EDIT_NUDGE_CURVE
    } else {
        EDIT_STEP_CURVE
    };
    let crop = |direction| {
        let step = if shift { CROP_NUDGE_PX } else { CROP_STEP_PX };
        let delta = if alt { -step } else { step };
        EditAdjust::Crop { direction, delta }
    };
    match key {
        "-" => Some(EditAdjust::Exposure(-ev)),
        "=" => Some(EditAdjust::Exposure(ev)),
        "[" => Some(EditAdjust::Contrast(-curve)),
        "]" => Some(EditAdjust::Contrast(curve)),
        ";" => Some(EditAdjust::Rolloff(-curve)),
        "'" => Some(EditAdjust::Rolloff(curve)),
        "," => Some(EditAdjust::Shadows(-curve)),
        "." => Some(EditAdjust::Shadows(curve)),
        "h" => Some(crop(Left)),
        "j" => Some(crop(Bottom)),
        "k" => Some(crop(Top)),
        "l" => Some(crop(Right)),
        // Rotate the display one quarter-turn counter-clockwise. A bare `r`
        // fires on press and the edit-key release commits the persist + re-bake
        // (the release arm matches through this same function, so `r` is
        // covered); modifiers are ignored like the tone-adjacent keys.
        "r" => Some(EditAdjust::RotateCcw),
        _ => None,
    }
}

/// Decodes a RAW frame from the open roll into a thumbnail message, baking in
/// the given exposure, tone curve and display rotation so the grid tile reflects
/// the stored edits (grid == detail).
async fn decode_thumbnail(
    dir: PathBuf,
    name: String,
    tone: edit_manifest::ToneEdit,
    crop: edit_manifest::CropMargins,
    rotation: u8,
    preset: FilmPreset,
    base_config: BaseConfig,
) -> Message {
    let result = decode_raw(dir, name.clone(), move |image| {
        convert_thumbnail(image, THUMB_SIZE, tone, crop, rotation, preset, base_config)
    })
    .await;

    Message::ThumbReady(name, result)
}

/// Decodes a roll's cover file into a thumbnail message, baking in the cover
/// file's stored exposure, tone curve and display rotation from the roll's
/// manifest so the roll tile preview parallels the edited frame (grid == detail
/// for covers too). A roll with no manifest (or an unedited cover) falls back
/// to identity. The cover's own on-disk manifest also supplies the roll's base
/// mode, so a library-page cover reflects a calibration recorded from the open
/// roll without extra plumbing.
async fn decode_cover(dir: PathBuf, name: String, preset: FilmPreset) -> Message {
    let manifest = edit_manifest::load_roll_manifest(&dir);
    let tone = manifest.tone(&name);
    let crop = manifest.crop(&name);
    let rotation = manifest.rotation(&name) & 3;
    let base_config = manifest.base_config();
    let result = decode_raw(dir.clone(), name, move |image| {
        convert_thumbnail(image, THUMB_SIZE, tone, crop, rotation, preset, base_config)
    })
    .await;

    Message::CoverReady(dir, result)
}

/// Decodes a RAW frame from the open roll into a hi-res message for
/// the detail view, returning the oriented linear mono data that the GPU
/// shader uploads and applies exposure to.  `max_edge` caps the long edge in
/// pixels; the overview level uses [`HI_RES_SIZE`], the native level-up
/// [`MAX_TEXTURE_EDGE`].
async fn decode_detail(
    dir: PathBuf,
    name: String,
    max_edge: u32,
    preset: FilmPreset,
    base_config: BaseConfig,
) -> Message {
    let result = decode_raw_detail(dir, name.clone(), max_edge, preset, base_config).await;
    Message::DetailReady(name, preset, result)
}

/// Decodes a neighbor frame's overview for the preload cache. Unlike
/// [`decode_detail`] this only populates the LRU — it never becomes the active
/// detail shader — so it always decodes at the fixed overview cap.
async fn preload_detail(
    dir: PathBuf,
    name: String,
    preset: FilmPreset,
    base_config: BaseConfig,
) -> Message {
    let result =
        decode_raw_detail(dir.clone(), name.clone(), HI_RES_SIZE, preset, base_config).await;
    Message::DetailPreloaded(dir, name, preset, result)
}

/// Runs a RAW decode plus mono reconstruction on a blocking worker thread,
/// returning TRUE sensor-linear `mono` (post-downscale, post-unsharp) oriented
/// to display upright, for every preset — the shader applies the exposure gain
/// and (for film) the density inversion per fragment. `max_edge` is the
/// downscale target for the long edge before unsharp.
///
/// The `src_long_edge` field is the sensor's true long edge AFTER cropping but
/// BEFORE the downscale — i.e. the real native long edge the overview was
/// scaled down from (< `max_edge` means the overview is already full-res).
/// `inversion` is `Some((stock, base))` when the preset marks a film negative,
/// threading the clear-film anchor to the shader; the roll's `base_config`
/// resolves that anchor preset-first (calibration, then the auto opt-in, then
/// the stock's preset base).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn decode_raw_detail(
    dir: PathBuf,
    name: String,
    max_edge: u32,
    preset: FilmPreset,
    base_config: BaseConfig,
) -> Result<DetailDecode, ()> {
    let path = dir.join(name);

    tokio::task::spawn_blocking(move || {
        let image = rawloader::decode_file(&path).map_err(|_| ())?;

        let width = usize::max(image.width, 1);
        let height = usize::max(image.height, 1);

        let normalized = normalize_samples(&image);

        let (samples, width, height) = match crop_samples(&normalized, width, height, image.crops) {
            Some(cropped) => cropped,
            None => (normalized, width, height),
        };

        let src_long_edge = u32::max(width as u32, height as u32);

        let (mono, width, height) = if image.cpp >= 3 {
            if samples.len() < width * height * 3 {
                return Err(());
            }

            let mut rgb = Vec::with_capacity(width * height * 3);
            for pixel in 0..width * height {
                let base = pixel * image.cpp;
                rgb.push(samples[base]);
                rgb.push(samples[base + 1]);
                rgb.push(samples[base + 2]);
            }

            (luma(&rgb), width, height)
        } else {
            if samples.len() < width * height {
                return Err(());
            }

            let cfa = image.cfa.shift(image.crops[3], image.crops[0]);
            (flatten_bayer(&samples, width, height, &cfa), width, height)
        };

        // True sensor-linear data for every preset: an already-positive scan
        // (None) and a film negative both stay linear `[0,1]` relative to the
        // sensor white point — the exposure gain touches the RAW values, and
        // the density inversion for a negative happens per fragment in the
        // shader. The only film-side work here is resolving the frame's
        // clear-film anchor (the inversion's black point) from the roll's base
        // mode: calibration, then the per-frame auto opt-in (measured from the
        // sensor data), then the stock's preset base.
        let inversion = preset.stock().map(|stock| {
            let measured = if base_config.auto {
                measure_base(&mono)
            } else {
                None
            };
            (stock, base_config.resolve(measured, &stock))
        });

        let (mono, width, height) = resize_area(&mono, width as u32, height as u32, max_edge, 1);

        let mut mono = mono;
        unsharp_mask(&mut mono, width as usize, height as usize);

        let (oriented, width, height) = orient_mono(&mono, width, height, image.orientation);

        Ok(DetailDecode {
            mono: oriented,
            width,
            height,
            src_long_edge,
            inversion,
        })
    })
    .await
    .unwrap_or(Err(()))
}

/// Exports every listed frame into `dest` per the given options, sequentially,
/// so only one full-resolution decode is in flight at a time (bounded transient
/// memory). After each frame — written, skipped, or failed — `progress` is
/// called with the running `(done, total)` counts so the caller can stream
/// live progress to the UI. Returns how many frames succeeded, how many were
/// skipped (their output already existed and overwrite was off), and how many
/// failed.
#[allow(clippy::too_many_arguments)]
async fn export_frames(
    dir: PathBuf,
    dest: PathBuf,
    frames: Vec<(
        String,
        edit_manifest::ToneEdit,
        edit_manifest::CropMargins,
        u8,
        usize,
    )>,
    options: ExportOptions,
    preset: FilmPreset,
    base_config: BaseConfig,
    start_date: Option<String>,
    mut progress: impl FnMut(usize, usize) + Send,
) -> (usize, usize, usize) {
    let mut ok = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let total = frames.len();
    let mut done = 0;
    // Dated rolls export under `YYYYMMDD-<roll-hash>-<frame>.<ext>` (see
    // `dated_export_name`); the roll's directory fingerprint is shared by the
    // whole batch. Undated rolls keep the plain `<stem>.<ext>` names.
    let roll_hash = roll_hash(&dir);
    for (name, tone, crop, rotation, index) in frames {
        let target = dest.join(
            start_date
                .as_deref()
                .and_then(|start| dated_export_name(options.format, start, index, &roll_hash))
                .unwrap_or_else(|| export_name(&name, options.format)),
        );
        // With overwrite off, an already-present file is kept as-is: the frame
        // is skipped before any decode.
        if !options.overwrite && target.exists() {
            skipped += 1;
        } else if export_one(
            dir.clone(),
            name,
            index,
            target,
            tone,
            crop,
            rotation,
            options,
            preset,
            base_config,
            start_date.clone(),
        )
        .await
        .is_ok()
        {
            ok += 1;
        } else {
            failed += 1;
        }
        done += 1;
        progress(done, total);
    }
    (ok, skipped, failed)
}

/// Decodes a frame (at native resolution, or downscaled when the size option
/// caps the long edge) and writes `dest` per the options: a JPEG at the chosen
/// quality, or a lossless 16-bit grayscale PNG. The stored tone curve,
/// exposure, crop, and display rotation are baked into the pixels — the same
/// edit pipeline as the detail view and thumbnails, just at the chosen scale
/// (the overview/native downscale is skipped for `Original`).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
#[allow(clippy::too_many_arguments)]
async fn export_one(
    dir: PathBuf,
    name: String,
    index: usize,
    dest: PathBuf,
    tone: edit_manifest::ToneEdit,
    crop: edit_manifest::CropMargins,
    rotation: u8,
    options: ExportOptions,
    preset: FilmPreset,
    base_config: BaseConfig,
    start_date: Option<String>,
) -> Result<(), ()> {
    let max_edge = options.size.long_edge();
    // True sensor-linear mono (masked-border cropped, downscaled per the size,
    // unsharpened — the exact detail-view decode), at the size option's long
    // edge or full native resolution. `src_long_edge` is the pre-downscale
    // long edge, so the factor pins how far below native the print is; the
    // film inversion (if any) is applied in the bake below, not in the decode.
    let DetailDecode {
        mut mono,
        width,
        height,
        src_long_edge,
        inversion: _,
    } = decode_raw_detail(
        dir.clone(),
        name.clone(),
        if max_edge == 0 { u32::MAX } else { max_edge },
        preset,
        base_config,
    )
    .await?;

    // The crop margins are authored in display-upright source pixels; scale
    // them onto the (possibly downscaled) print. When the decode was already
    // at native resolution the factor is 1.0 and this is the identity.
    let factor = if max_edge > 0 && src_long_edge > max_edge {
        max_edge as f32 / src_long_edge as f32
    } else {
        1.0
    };
    let source_w = (width as f32 / factor).round() as u32;
    let source_h = (height as f32 / factor).round() as u32;
    let crop = scale_crop(crop, source_w, source_h, width, height);

    // Bake the stored tone curve, exposure, then sRGB-encode via the one
    // shared tail (`bake_tone`) — the detail shader's exact ordering per preset:
    // film negatives get the EV gain on the true sensor data first (2^-EV from
    // the user-space +EV, `sensor_gain`'s twin), then the density inversion,
    // then the EV-exact pivoted curve; positive scans get curve then gain.
    bake_tone(&mut mono, tone, preset, base_config);

    // When the roll carries a start date, stamp each output's DateTimeOriginal
    // with `start date @ 00:00:00 + frame's full-roll offset in seconds` — the
    // synthetic film-capture time that keeps exports in chronological order in
    // photo/cloud apps. DateTimeDigitized mirrors the RAW's own scan time
    // (read as its raw `YYYY:MM:DD HH:MM:SS` EXIF text) when it has one. Rolls
    // without a start date export un-stamped. The extra EXIF parse runs once
    // per frame beside the (far heavier) full decode.
    let tiff = start_date.as_deref().and_then(|start| {
        let original = exif_writer::shifted_datetime(start, index)?;
        let digitized = exif_writer::raw_datetime_original(&dir, &name);
        exif_writer::build_tiff(&original, digitized.as_deref())
    });

    match options.format {
        ExportFormat::Jpeg => {
            export_jpeg(mono, width, height, crop, rotation, &dest, tiff.as_deref(), options)
        }
        ExportFormat::Png => {
            export_png(mono, width, height, crop, rotation, &dest, tiff.as_deref(), options)
        }
    }
}

/// Writes `dest` as a quality-`options` JPEG, quantizing the sRGB mono buffer
/// to a single luminance channel (same shared bake as `export_png`). When
/// `options.ppi` is non-zero the JFIF header tags that dots-per-inch density.
/// An optional `exif_tiff` blob carries DateTimeOriginal/DateTimeDigitized and
/// is spliced into the JPEG stream as an Exif APP1 segment right after the SOI
/// byte-order marker.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[allow(clippy::too_many_arguments)]
fn export_jpeg(
    mono: Vec<f32>,
    width: u32,
    height: u32,
    crop: edit_manifest::CropMargins,
    rotation: u8,
    dest: &Path,
    exif_tiff: Option<&[u8]>,
    options: ExportOptions,
) -> Result<(), ()> {
    let mut rgba = Vec::with_capacity(mono.len() * 4);
    for &value in &mono {
        let level = (value * 255.0).round() as u8;
        rgba.extend_from_slice(&[level, level, level, 255]);
    }
    // Free the linear mono before the (similar-sized) steps below.
    drop(mono);

    let (rgba, width, height) = bake_geometry(rgba, width, height, crop, rotation);

    // Negatives render monochrome: all four channels carry the same level, so
    // drop to a single luminance channel. Revisit once color negatives are
    // supported (see NOTES.md).
    let mut gray: Vec<u8> = Vec::with_capacity(width as usize * height as usize);
    for px in rgba.chunks(4) {
        gray.push(px[0]);
    }
    drop(rgba);

    // The JPEG header fields are u16; exports are far below that, so the
    // conversion can only fail for absurd geometry — bail before creating any
    // temp file.
    let width = u16::try_from(width).map_err(|_| ())?;
    let height = u16::try_from(height).map_err(|_| ())?;

    // Encode into an in-memory buffer so we can splice the optional Exif APP1
    // after the SOI marker, then atomically flush the final byte stream to disk
    // with a same-directory rename.
    let mut bytes = Vec::with_capacity(gray.len() + 200);
    let mut encoder = jpeg_encoder::Encoder::new(&mut bytes, options.quality);
    // The default header is a bare (1,1) pixel-aspect-ratio; tag a real density
    // so print/layout tools scale the file by its intended PPI.
    if options.ppi != 0 {
        encoder.set_density(jpeg_encoder::PixelDensity::dpi(options.ppi));
    }
    // `encode` consumes the encoder (flush-on-drop), so its output is complete
    // by the time it returns; only then can we splice and rename.
    if encoder
        .encode(&gray, width, height, jpeg_encoder::ColorType::Luma)
        .is_err()
    {
        return Err(());
    }
    if let Some(tiff) = exif_tiff {
        let app1 = exif_writer::jpeg_app1(tiff);
        bytes = exif_writer::splice_after_soi(&bytes, &app1);
    }
    let tmp = temp_export_path(dest);
    std::fs::write(&tmp, &bytes).map_err(|_| ())?;
    std::fs::rename(&tmp, dest).map_err(|_| ())
}

/// Writes `dest` as a lossless 16-bit grayscale PNG via the `png` crate,
/// quantizing the sRGB mono buffer to `u16` luminance samples (big-endian —
/// the byte order the PNG container requires, written as-is by the raw `png`
/// crate). When `options.ppi` is non-zero a `pHYs` chunk tags that density in
/// pixels-per-meter. An optional `exif_tiff` blob carries
/// DateTimeOriginal/DateTimeDigitized as an `eXIf` chunk (the raw TIFF, which
/// PNG stores without JPEG's `Exif\0\0` prefix).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[allow(clippy::too_many_arguments)]
fn export_png(
    mono: Vec<f32>,
    width: u32,
    height: u32,
    crop: edit_manifest::CropMargins,
    rotation: u8,
    dest: &Path,
    exif_tiff: Option<&[u8]>,
    options: ExportOptions,
) -> Result<(), ()> {
    let mut rgba = Vec::with_capacity(mono.len() * 4);
    for &value in &mono {
        let level = (value * 65535.0).round() as u16;
        rgba.extend_from_slice(&[level, level, level, u16::MAX]);
    }
    drop(mono);
    let (rgba, width, height) = bake_geometry16(rgba, width, height, crop, rotation);

    // Negatives render monochrome: all four channels carry the same level, so
    // drop to a single luminance channel. Revisit once color negatives are
    // supported (see NOTES.md). Samples are stored big-endian — the byte order
    // the PNG container requires; the raw `png` crate writes them as-is (the
    // `image` wrapper used to re-swap them for us, hence the earlier
    // native-endian notes).
    let mut gray = Vec::with_capacity(width as usize * height as usize * 2);
    for px in rgba.chunks(4) {
        gray.extend_from_slice(&px[0].to_be_bytes());
    }
    drop(rgba);

    // Encode to a same-directory temp file, then rename over `dest` only once
    // every byte is on disk: a mid-encode failure must not truncate (or, with
    // overwrite on, replace) an existing file.
    let tmp = temp_export_path(dest);
    let file = std::fs::File::create(&tmp).map_err(|_| ())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Sixteen);
    // The samples are sRGB-encoded; a `sRGB` chunk makes that interpretation
    // deterministic for color-managed consumers (print pipelines especially),
    // instead of relying on an unstated viewer default.
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    // `pHYs` stores pixels per meter (1 in = 0.0254 m), unlike JFIF's per-inch.
    if options.ppi != 0 {
        let ppm = (f32::from(options.ppi) / 0.0254).round() as u32;
        encoder.set_pixel_dims(Some(png::PixelDimensions {
            xppu: ppm,
            yppu: ppm,
            unit: png::Unit::Meter,
        }));
    }
    let mut writer = encoder.write_header().map_err(|_| ())?;
    // `eXIf` holds the raw TIFF blob (no `Exif\0\0` prefix — PNG uses the
    // chunk name to mark EXIF data), written before any IDAT as the spec asks.
    if let Some(tiff) = exif_tiff
        && writer.write_chunk(png::chunk::eXIf, tiff).is_err()
    {
        drop(writer);
        std::fs::remove_file(&tmp).ok();
        return Err(());
    }
    if writer.write_image_data(&gray).is_err() {
        drop(writer);
        std::fs::remove_file(&tmp).ok();
        return Err(());
    }
    // `finish` writes the IEND trailer and consumes the writer; its dropped
    // BufWriter flushes any residual bytes, so the temp file is complete on
    // success and only then renamed into place.
    if writer.finish().is_err() {
        std::fs::remove_file(&tmp).ok();
        return Err(());
    }
    std::fs::rename(&tmp, dest).map_err(|_| ())
}

/// Runs a RAW decode plus conversion on a blocking worker thread so the UI
/// never stalls on CPU-heavy work.
async fn decode_raw<F>(dir: PathBuf, name: String, convert: F) -> Result<Handle, ()>
where
    F: Fn(&rawloader::RawImage) -> Result<Handle, ()> + Send + 'static,
{
    let path = dir.join(name);

    tokio::task::spawn_blocking(move || {
        rawloader::decode_file(&path)
            .map_err(|_| ())
            .and_then(|image| convert(&image))
    })
    .await
    .unwrap_or(Err(()))
}

/// Normalizes raw sensor samples into linear 0..=1 intensity.
fn normalize_samples(image: &rawloader::RawImage) -> Vec<f32> {
    let width = usize::max(image.width, 1);

    match &image.data {
        rawloader::RawImageData::Integer(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                // Black/white levels are per-channel, so look them up by CFA position.
                let color = image.cfa.color_at(index / width, index % width);
                let black = f32::from(image.blacklevels[color]);
                let span = (f32::from(image.whitelevels[color]) - black).max(f32::EPSILON);

                (f32::from(*value) - black).clamp(0.0_f32, span) / span
            })
            .collect(),
        rawloader::RawImageData::Float(values) => {
            let max = values.iter().copied().fold(0.0_f32, f32::max);
            let gain = if max > f32::EPSILON { 1.0 / max } else { 1.0 };

            values
                .iter()
                .map(|value| (*value * gain).clamp(0.0, 1.0))
                .collect()
        }
    }
}

/// Slices a sample buffer down to the usable area described by rawloader's
/// `[top, right, bottom, left]` crops, discarding masked sensor borders.
fn crop_samples(
    samples: &[f32],
    width: usize,
    height: usize,
    crops: [usize; 4],
) -> Option<(Vec<f32>, usize, usize)> {
    let [top, right, bottom, left] = crops;
    if right + left >= width || top + bottom >= height || samples.len() < width * height {
        return None;
    }

    let (out_width, out_height) = (width - right - left, height - top - bottom);
    let mut cropped = Vec::with_capacity(out_width * out_height);
    for y in top..top + out_height {
        let row = y * width + left;
        cropped.extend_from_slice(&samples[row..row + out_width]);
    }

    Some((cropped, out_width, out_height))
}

/// Approximate number of photosites sampled per CFA class when measuring
/// clear-film bases in [`flatten_bayer`].
const BASE_SAMPLE_TARGET: usize = 250_000;

/// Reconstructs a full-resolution monochrome negative from bayer samples.
///
/// Monochrome film carries no color signal, so each CFA class is treated as an
/// independent density measurement: every class's clear-film transmission is
/// measured from a strided subsample and the classes are rescaled onto one
/// common base. This reads the sensor at native resolution instead of
/// averaging 2x2 blocks, keeping grain texture a demosaic would smear.
fn flatten_bayer(samples: &[f32], width: usize, height: usize, cfa: &rawloader::CFA) -> Vec<f32> {
    let pixels = width * height;
    // An odd step cannot alias with the period-2 CFA grid.
    let step = usize::max(pixels / BASE_SAMPLE_TARGET, 1) | 1;

    let mut class_samples: [Vec<f32>; 4] = Default::default();
    for idx in (0..pixels).step_by(step) {
        let (y, x) = (idx / width, idx % width);
        class_samples[cfa.color_at(y, x)].push(samples[idx]);
    }

    let mut anchored = [ACTIVE_STOCK.base; 4];
    for (base, class) in anchored.iter_mut().zip(&class_samples) {
        if let Some(measured) =
            measure_base(class).filter(|measured| *measured >= MIN_PLAUSIBLE_BASE)
        {
            *base = measured;
        }
    }
    // Anchor onto the green class when present (best SNR, keeps magnitudes
    // close to true transmissions); otherwise the dimmest measurable class.
    let empty = std::array::from_fn(|class| class_samples[class].is_empty());
    let gains = class_gains(anchored, empty);

    samples[..pixels]
        .iter()
        .enumerate()
        .map(|(idx, value)| value * gains[cfa.color_at(idx / width, idx % width)])
        .collect()
}

/// Gains that rescale the four CFA classes onto one common base.
///
/// Anchors onto the green class when present (best SNR, keeps magnitudes close
/// to true transmissions); otherwise the dimmest measurable class. A class
/// with no samples at its measurement resolution is rescaled by its anchored
/// base like any other. Mirrored by the fused thumbnail downscaler so the
/// full-res detail path and the collapsed thumbnail path share one rule.
///
/// `anchored` and `empty` are indexed by CFA color (0=R, 1=G, 2=B, 3=fourth).
fn class_gains(anchored: [f32; 4], empty: [bool; 4]) -> [f32; 4] {
    let reference = if empty[1] {
        let Some(dimmest) = anchored
            .iter()
            .zip(empty)
            .filter(|(_, class_empty)| !*class_empty)
            .map(|(base, _)| *base)
            .reduce(f32::min)
        else {
            // No measurable class at all; keep every class unscaled.
            return [1.0; 4];
        };
        dimmest
    } else {
        anchored[1]
    };

    std::array::from_fn(|class| reference / anchored[class])
}

/// Multiplies linear samples by `2^EV` in place, mirroring the detail
/// shader's gain so CPU and GPU rendering stay bit-consistent.
fn apply_exposure(mono: &mut [f32], exposure_ev: f32) {
    let gain = f32::exp2(exposure_ev);
    for value in mono {
        *value *= gain;
    }
}

/// Encodes a linear intensity into the sRGB transfer function.
fn srgb_encode(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// Collapses interleaved linear RGB to Rec.709 luminance.
///
/// Only meaningful in linear light, where luminance coefficients are defined;
/// stripping the channels this way removes any capture cast from monochrome
/// film scans outright.
fn luma(rgb: &[f32]) -> Vec<f32> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| 0.212_6 * r + 0.715_2 * g + 0.072_2 * b)
        .collect()
}

/// Start and length of the source range that output coordinate `out` covers,
/// offset by `origin` (the cropped edge on that axis).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn block_span(out: u32, source: u32, out_total: u32, origin: usize) -> (usize, usize) {
    let start = range(out, source, out_total) + origin;
    let len = range_len(start, range(out + 1, source, out_total) + origin);
    (start, len)
}

/// Rec.709 luminance of three per-block channel means.
fn luma_of_means(r: f32, g: f32, b: f32) -> f32 {
    0.212_6 * r + 0.715_2 * g + 0.072_2 * b
}

/// Fused normalization, cropping, and phase-preserving downscale.
///
/// Reads the raw sensor samples exactly once into per-output-pixel
/// accumulators, bypassing the full-resolution linear buffer the separate
/// [`normalize_samples`]/[`crop_samples`]/[`flatten_bayer`]/[`resize_area`]
/// steps build. The size math and block mapping match [`resize_area`] (never
/// upscales). Returns a small linear negative-space mono; inversion happens in
/// the caller.
///
/// RGB sources collapse through the Rec.709 luminance of each block's channel
/// means — exact, because luma is linear. Bayer mosaics keep each CFA class
/// separate per block and rescale the class means onto one common base with
/// the same [`class_gains`] rule the detail path uses, so a heavy downscale
/// averages each site's cast independently (flatten-before-average tone
/// accepted over the full-res invert-then-average ordering).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn downsample_thumbnail(
    image: &rawloader::RawImage,
    max_size: u32,
) -> Option<(Vec<f32>, u32, u32)> {
    let width = usize::max(image.width, 1);
    let height = usize::max(image.height, 1);
    let [top, right, bottom, left] = image.crops;
    let cw = width.checked_sub(right.saturating_add(left))?;
    let ch = height.checked_sub(top.saturating_add(bottom))?;
    if cw == 0 || ch == 0 {
        return None;
    }

    let scale = f32::min(
        1.0,
        f32::min(max_size as f32 / cw as f32, max_size as f32 / ch as f32),
    );
    let out_w = u32::max((cw as f32 * scale) as u32, 1);
    let out_h = u32::max((ch as f32 * scale) as u32, 1);

    let mono = if image.cpp >= 3 {
        downsample_rgb(image, out_w as usize, out_h as usize)?
    } else {
        downsample_bayer(image, out_w as usize, out_h as usize)?
    };

    Some((mono, out_w, out_h))
}

/// Fused RGB-source branch of [`downsample_thumbnail`]: block channel means
/// collapsed through Rec.709 luminance.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn downsample_rgb(image: &rawloader::RawImage, out_w: usize, out_h: usize) -> Option<Vec<f32>> {
    let width = usize::max(image.width, 1);
    let height = usize::max(image.height, 1);
    let cpp = usize::max(image.cpp, 1);
    let [top, right, bottom, left] = image.crops;
    let (cw, ch) = (width - right - left, height - top - bottom);

    let mut mono = Vec::with_capacity(out_w * out_h);

    match &image.data {
        rawloader::RawImageData::Integer(values) => {
            if values.len() < width * height * cpp {
                return None;
            }
            for out_y in 0..out_h as u32 {
                let (row_start, rows) = block_span(out_y, ch as u32, out_h as u32, top);
                for out_x in 0..out_w as u32 {
                    let (col_start, cols) = block_span(out_x, cw as u32, out_w as u32, left);
                    let mut sums = [0.0_f32; 3];
                    for y in row_start..row_start + rows {
                        for x in col_start..col_start + cols {
                            // Black/white levels are per-channel, looked up by
                            // CFA position, matching normalize_samples.
                            let color = image.cfa.color_at(y, x);
                            let black = f32::from(image.blacklevels[color]);
                            let span =
                                (f32::from(image.whitelevels[color]) - black).max(f32::EPSILON);
                            let offset = (y * width + x) * cpp;
                            for (channel, sum) in sums.iter_mut().enumerate() {
                                let value = f32::from(values[offset + channel]);
                                *sum += (value - black).clamp(0.0, span) / span;
                            }
                        }
                    }
                    let count = (rows * cols) as f32;
                    mono.push(luma_of_means(
                        sums[0] / count,
                        sums[1] / count,
                        sums[2] / count,
                    ));
                }
            }
        }
        rawloader::RawImageData::Float(values) => {
            let max = values.iter().copied().fold(0.0_f32, f32::max);
            let gain = if max > f32::EPSILON { 1.0 / max } else { 1.0 };
            for out_y in 0..out_h as u32 {
                let (row_start, rows) = block_span(out_y, ch as u32, out_h as u32, top);
                for out_x in 0..out_w as u32 {
                    let (col_start, cols) = block_span(out_x, cw as u32, out_w as u32, left);
                    let mut sums = [0.0_f32; 3];
                    for y in row_start..row_start + rows {
                        for x in col_start..col_start + cols {
                            let offset = (y * width + x) * cpp;
                            for (channel, sum) in sums.iter_mut().enumerate() {
                                *sum += (values[offset + channel] * gain).clamp(0.0, 1.0);
                            }
                        }
                    }
                    let count = (rows * cols) as f32;
                    mono.push(luma_of_means(
                        sums[0] / count,
                        sums[1] / count,
                        sums[2] / count,
                    ));
                }
            }
        }
    }

    Some(mono)
}

/// Fused bayer branch of [`downsample_thumbnail`]: per-CFA-class block means
/// rescaled onto one common base.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn downsample_bayer(image: &rawloader::RawImage, out_w: usize, out_h: usize) -> Option<Vec<f32>> {
    let width = usize::max(image.width, 1);
    let height = usize::max(image.height, 1);
    let [top, right, bottom, left] = image.crops;
    let (cw, ch) = (width - right - left, height - top - bottom);
    let out_pixels = out_w * out_h;

    let mut sums = vec![[0.0_f32; 4]; out_pixels];
    let mut counts = vec![[0_u32; 4]; out_pixels];

    match &image.data {
        rawloader::RawImageData::Integer(values) => {
            if values.len() < width * height {
                return None;
            }
            for out_y in 0..out_h as u32 {
                let (row_start, rows) = block_span(out_y, ch as u32, out_h as u32, top);
                for out_x in 0..out_w as u32 {
                    let (col_start, cols) = block_span(out_x, cw as u32, out_w as u32, left);
                    let slot = out_y as usize * out_w + out_x as usize;
                    for y in row_start..row_start + rows {
                        for x in col_start..col_start + cols {
                            let color = image.cfa.color_at(y, x);
                            let black = f32::from(image.blacklevels[color]);
                            let span =
                                (f32::from(image.whitelevels[color]) - black).max(f32::EPSILON);
                            let value = f32::from(values[y * width + x]);
                            sums[slot][color] += (value - black).clamp(0.0, span) / span;
                            counts[slot][color] += 1;
                        }
                    }
                }
            }
        }
        rawloader::RawImageData::Float(values) => {
            let max = values.iter().copied().fold(0.0_f32, f32::max);
            let gain = if max > f32::EPSILON { 1.0 / max } else { 1.0 };
            for out_y in 0..out_h as u32 {
                let (row_start, rows) = block_span(out_y, ch as u32, out_h as u32, top);
                for out_x in 0..out_w as u32 {
                    let (col_start, cols) = block_span(out_x, cw as u32, out_w as u32, left);
                    let slot = out_y as usize * out_w + out_x as usize;
                    for y in row_start..row_start + rows {
                        for x in col_start..col_start + cols {
                            let color = image.cfa.color_at(y, x);
                            let value = values[y * width + x];
                            sums[slot][color] += (value * gain).clamp(0.0, 1.0);
                            counts[slot][color] += 1;
                        }
                    }
                }
            }
        }
    }

    // Collapse each class to its block mean, measure a per-class base, and
    // rescale onto one common base with the same anchoring the detail path
    // uses. A class absent from a block simply does not contribute to that
    // output pixel.
    let mut class_values: [Vec<f32>; 4] = Default::default();
    for (slot, sums) in sums.iter().enumerate() {
        for class in 0..4 {
            if counts[slot][class] != 0 {
                class_values[class].push(sums[class] / counts[slot][class] as f32);
            }
        }
    }

    let mut anchored = [ACTIVE_STOCK.base; 4];
    for (base, class) in anchored.iter_mut().zip(&class_values) {
        if let Some(measured) =
            measure_base(class).filter(|measured| *measured >= MIN_PLAUSIBLE_BASE)
        {
            *base = measured;
        }
    }
    let empty = std::array::from_fn(|class| class_values[class].is_empty());
    let gains = class_gains(anchored, empty);

    let mut mono = Vec::with_capacity(out_pixels);
    for (slot, sums) in sums.iter().enumerate() {
        let mut sum = 0.0_f32;
        let mut non_empty = 0_u32;
        for class in 0..4 {
            if counts[slot][class] != 0 {
                sum += (sums[class] / counts[slot][class] as f32) * gains[class];
                non_empty += 1;
            }
        }
        let level = if non_empty == 0 {
            ACTIVE_STOCK.base
        } else {
            sum / non_empty as f32
        };
        mono.push(level);
    }

    Some(mono)
}

/// Scales source-pixel crop margins onto a print of target dimensions.
///
/// Both the source dims and the print must be in the SAME (display-oriented)
/// frame: the caller resolves the full-resolution display dimensions via
/// [`display_source_dims`] (the sensor's post-masked-border dims, rotated to
/// upright), so the print's horizontal axis always maps back to the source's
/// horizontal and no axis-swap detection is needed here. A rotation merely
/// swaps which of `src_w`/`src_h` the caller passes.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scale_crop(
    crop: edit_manifest::CropMargins,
    src_w: u32,
    src_h: u32,
    out_width: u32,
    out_height: u32,
) -> edit_manifest::CropMargins {
    let sx = f64::from(out_width) / f64::from(src_w.max(1));
    let sy = f64::from(out_height) / f64::from(src_h.max(1));
    edit_manifest::CropMargins {
        top: (f64::from(crop.top) * sy).round() as u32,
        right: (f64::from(crop.right) * sx).round() as u32,
        bottom: (f64::from(crop.bottom) * sy).round() as u32,
        left: (f64::from(crop.left) * sx).round() as u32,
    }
}

/// The display-upright dimensions of a sensor whose post-masked-border dims
/// are `(cw, ch)`: any orientation that swaps the print axes (90°/270°
/// rotation, transpose) maps the display horizontal onto the sensor vertical.
fn display_source_dims(cw: u32, ch: u32, orientation: rawloader::Orientation) -> (u32, u32) {
    use rawloader::Orientation;
    match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Transpose
        | Orientation::Transverse => (ch, cw),
        _ => (cw, ch),
    }
}

/// Removes the given margins from an RGBA frame into a new, smaller buffer.
/// Returns the frame unchanged when the margins overrun the extent so a
/// degenerate crop can never collapse a thumbnail.
fn crop_rgba(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    crop: edit_manifest::CropMargins,
) -> (Vec<u8>, u32, u32) {
    let cw = crop.cropped_width(width);
    let ch = crop.cropped_height(height);
    if cw == 0 || ch == 0 {
        return (rgba, width, height);
    }
    let mut out = Vec::with_capacity((cw * ch) as usize * 4);
    let mut rows = 0u32;
    let top = crop.top.min(height.saturating_sub(ch));
    for y in top..top.saturating_add(ch) {
        let row = y as usize * width as usize + crop.left as usize;
        let start = row * 4;
        if start >= rgba.len() {
            break;
        }
        let end = (start + cw as usize * 4).min(rgba.len());
        out.extend_from_slice(&rgba[start..end]);
        rows += 1;
    }
    (out, cw, rows)
}

/// Rotates an RGBA buffer counter-clockwise by `turns` 90° quarter turns
/// (`0`…`3`, masked to `& 3`), swapping the print dims on odd turns. The
/// output naturally has the swapped dimensions (no re-fit is attempted):
/// [`convert_thumbnail`] hands the already-downscaled thumbnail to the GPU at
/// the same aspect, and the detail shader's rotated-fraction UV layout matches
/// this exact pixel mapping, so the grid tile and the detail view stay in
/// agreement. `0` (and any multiple of 4) is the identity — the untouched bake
/// stays byte-identical.
fn rotate_quarters(rgba: Vec<u8>, width: u32, height: u32, turns: u8) -> (Vec<u8>, u32, u32) {
    let out_w = height;
    let out_h = width;
    let mut out = vec![0_u8; width as usize * height as usize * 4];
    match turns & 3 {
        0 => (rgba, width, height),
        1 => {
            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) as usize) * 4;
                    let ox = y;
                    let oy = width - 1 - x;
                    let dst = ((oy * out_w + ox) as usize) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
            (out, out_w, out_h)
        }
        2 => {
            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) as usize) * 4;
                    let oy = height - 1 - y;
                    let ox = width - 1 - x;
                    let dst = ((oy * width + ox) as usize) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
            (out, width, height)
        }
        _ => {
            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) as usize) * 4;
                    let ox = height - 1 - y;
                    let oy = x;
                    let dst = ((oy * out_w + ox) as usize) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
            (out, out_w, out_h)
        }
    }
}

/// Applies the export geometry to an RGBA print: crops to `crop` (scaled by the
/// caller to the print's native dims) then quarter-turns to `rotation`, matching
/// the thumbnail/detail ordering (crop-then-rotate). Shared by the JPEG and PNG
/// export encoders so both bake the same framing.
fn bake_geometry(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    crop: edit_manifest::CropMargins,
    rotation: u8,
) -> (Vec<u8>, u32, u32) {
    let (rgba, width, height) = if crop == edit_manifest::CropMargins::default() {
        (rgba, width, height)
    } else {
        crop_rgba(rgba, width, height, crop)
    };
    rotate_quarters(rgba, width, height, rotation)
}

/// The 16-bit analogue of [`rotate_quarters`]: rotates an RGBA buffer of `u16`
/// samples counter-clockwise by `turns` 90° quarter turns, swapping the print
/// dims on odd turns. Used by the 16-bit PNG export path.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rotate_quarters16(rgba: Vec<u16>, width: u32, height: u32, turns: u8) -> (Vec<u16>, u32, u32) {
    let out_w = height;
    let out_h = width;
    let mut out = vec![0_u16; width as usize * height as usize * 4];
    match turns & 3 {
        0 => (rgba, width, height),
        1 => {
            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) as usize) * 4;
                    let ox = y;
                    let oy = width - 1 - x;
                    let dst = ((oy * out_w + ox) as usize) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
            (out, out_w, out_h)
        }
        2 => {
            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) as usize) * 4;
                    let oy = height - 1 - y;
                    let ox = width - 1 - x;
                    let dst = ((oy * width + ox) as usize) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
            (out, width, height)
        }
        _ => {
            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) as usize) * 4;
                    let ox = height - 1 - y;
                    let oy = x;
                    let dst = ((oy * out_w + ox) as usize) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
            (out, out_w, out_h)
        }
    }
}

/// Convenience for [`rotate_quarters`] vs [`rotate_quarters16`]: the export
/// path picks the deeper buffer's rotation directly.
fn bake_geometry16(
    rgba: Vec<u16>,
    width: u32,
    height: u32,
    crop: edit_manifest::CropMargins,
    rotation: u8,
) -> (Vec<u16>, u32, u32) {
    let (rgba, width, height) = if crop == edit_manifest::CropMargins::default() {
        (rgba, width, height)
    } else {
        crop_rgba16(rgba, width, height, crop)
    };
    rotate_quarters16(rgba, width, height, rotation)
}

/// The 16-bit analogue of [`crop_rgba`]: crops an RGBA buffer of `u16` samples
/// to the given margins (which the caller scaled to the print's dims), letting
/// a degenerate crop leave the frame unchanged.
fn crop_rgba16(
    rgba: Vec<u16>,
    width: u32,
    height: u32,
    crop: edit_manifest::CropMargins,
) -> (Vec<u16>, u32, u32) {
    let cw = crop.cropped_width(width);
    let ch = crop.cropped_height(height);
    if cw == 0 || ch == 0 {
        return (rgba, width, height);
    }
    let mut out = Vec::with_capacity((cw * ch) as usize * 4);
    let mut rows = 0u32;
    let top = crop.top.min(height.saturating_sub(ch));
    for y in top..top.saturating_add(ch) {
        let row = y as usize * width as usize + crop.left as usize;
        let start = row * 4;
        if start >= rgba.len() {
            break;
        }
        let end = (start + cw as usize * 4).min(rgba.len());
        out.extend_from_slice(&rgba[start..end]);
        rows += 1;
    }
    (out, cw, rows)
}

/// Measures the tone pivots (shadow/mid-gray/white) a frame needs — WITHOUT
/// mutating `mono` — plus, for a film negative, the resolved clear-film base and
/// its stock (needed by the density inversion).
///
/// The inverted (film) path uses the shader's EV-exact mechanic: raw sensor
/// fractiles measured on the intact pre-gain buffer, mapped through
/// `invert_value(fractile · 2^-EV)` so the pivots describe the ACTUAL render at
/// the current exposure. The positive path measures the regular anchors on the
/// positive. Returning the pair separately lets the whole-frame parity test feed
/// `render_tail` and the WGSL reference the identical inputs.
fn pivots_for(
    mono: &[f32],
    tone: edit_manifest::ToneEdit,
    preset: FilmPreset,
    base_config: BaseConfig,
) -> (Option<(MonoStock, f32)>, (f32, f32, f32)) {
    if let Some(stock) = preset.stock() {
        // Measured BEFORE any gain so the ranks stay in the shader's upload
        // domain (the gain would shift them).
        let fractiles = shader::film_anchor_fractiles(mono);
        let measured = if base_config.auto {
            measure_base(mono)
        } else {
            None
        };
        let base = base_config.resolve(measured, &stock);
        let pivots = shader::film_pivots_at_gain(
            fractiles,
            base,
            shader::sensor_gain(tone.exposure_ev, true),
            stock,
        );
        (Some((stock, base)), pivots)
    } else {
        (None, shader::tone_anchors(mono))
    }
}

/// Applies the detail shader's exact per-pixel tone ordering to a mono buffer in
/// place, then sRGB-encodes: for a film negative the EV gain hits the true
/// sensor data first (`2^-EV`), then the density inversion, then the pivoted
/// curve; for an already-positive scan the curve comes first, then the `2^EV`
/// gain. `stock_and_base` is `Some((stock, base))` exactly when the buffer is a
/// negative to invert. Shared by the grid thumbnail bake, the export bake, and
/// the WGSL-parity tests.
fn render_tail(
    mono: &mut [f32],
    tone: edit_manifest::ToneEdit,
    stock_and_base: Option<(MonoStock, f32)>,
    pivots: (f32, f32, f32),
) {
    if let Some((stock, base)) = stock_and_base {
        apply_exposure(mono, -tone.exposure_ev);
        invert_gray(mono, &stock, base);
    }
    let (shadow, mid, white) = pivots;
    shader::apply_curve(
        mono,
        tone.curve_contrast,
        tone.curve_rolloff,
        tone.curve_shadows,
        shadow,
        mid,
        white,
    );
    if stock_and_base.is_none() {
        apply_exposure(mono, tone.exposure_ev);
    }
    for value in mono {
        *value = srgb_encode(*value);
    }
}

/// The one shared tone tail for every CPU bake (grid thumbnails and exports):
/// measure the pivots from `mono`, then apply the shader's exact ordering and
/// sRGB-encode. Grid == detail == export hold structurally because this single
/// body (plus its WGSL-parity tests) is the only place the tone math lives for
/// the non-shader paths.
fn bake_tone(
    mono: &mut [f32],
    tone: edit_manifest::ToneEdit,
    preset: FilmPreset,
    base_config: BaseConfig,
) {
    let (stock_and_base, pivots) = pivots_for(mono, tone, preset, base_config);
    render_tail(mono, tone, stock_and_base, pivots);
}

/// Converts a decoded RAW image into a small oriented RGBA image, scaled so no
/// dimension exceeds `max_size`, baking the tone edit (`ToneEdit`: exposure,
/// curve powers) and the display rotation into the pixels.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn convert_thumbnail(
    image: &rawloader::RawImage,
    max_size: f32,
    tone: edit_manifest::ToneEdit,
    crop: edit_manifest::CropMargins,
    rotation: u8,
    preset: FilmPreset,
    base_config: BaseConfig,
) -> Result<Handle, ()> {
    // One fused pass: normalize, discard masked borders, and phase-preserve
    // downscale straight from the sensor samples into a small TRUE sensor-linear
    // mono (the same domain the detail decode produces, for every preset).
    let (mut mono, width, height) = downsample_thumbnail(image, max_size as u32).ok_or(())?;

    // Restore edge punch lost to the heavy downscale — in true sensor space for
    // every preset, matching the detail decode's unsharp so a film negative
    // carries its sharpening INTO the density inversion instead of leaving it
    // on the positive.
    unsharp_mask(&mut mono, width as usize, height as usize);

    // The one shared tone tail: measure the EV-exact (film) or histogram
    // (positive) pivots, apply the shader's exact ordering per preset (gain
    // before the density inversion for a negative, curve then gain for a
    // positive scan), then sRGB-encode — the same `bake_tone` every CPU bake
    // uses, so grid == detail == export hold structurally.
    bake_tone(&mut mono, tone, preset, base_config);

    let mut rgba = Vec::with_capacity(mono.len() * 4);
    for &value in &mono {
        let level = (value * 255.0).round() as u8;
        rgba.extend_from_slice(&[level, level, level, 255]);
    }

    let (rgba, width, height) = orient(&rgba, width, height, image.orientation);

    // Bake the live crop into the print. The persisted margins are authored in
    // full-resolution DISPLAY source pixels (matching the detail shader's
    // source frame); resolve the oriented source dims and scale them onto the
    // oriented print. A zero crop is a no-op.
    let (rgba, width, height) = if crop == edit_manifest::CropMargins::default() {
        (rgba, width, height)
    } else {
        let src_w = usize::max(image.width, 1);
        let src_h = usize::max(image.height, 1);
        let [ct, cr, cb, cl] = image.crops;
        let cw = src_w.saturating_sub(cr.saturating_add(cl)).max(1) as u32;
        let ch = src_h.saturating_sub(ct.saturating_add(cb)).max(1) as u32;
        let (disp_w, disp_h) = display_source_dims(cw, ch, image.orientation);
        let scaled = scale_crop(crop, disp_w, disp_h, width, height);
        crop_rgba(rgba, width, height, scaled)
    };

    // Bake the user's display rotation: quarter-turn the already EXIF-oriented
    // (and cropped) print so the grid tile matches the rotated detail view.
    // The crop margins are authored in the EXIF-upright source frame, so the
    // rotation composes AFTER the crop sub-rect was taken — the same ordering
    // as the detail shader's uniform rotate (`rotate_ccw(crop(orient(EXIF)))`).
    let (rgba, width, height) = rotate_quarters(rgba, width, height, rotation);

    Ok(Handle::from_rgba(width, height, rgba))
}

/// Downscales an interleaved buffer of `channels`-component samples so no
/// dimension exceeds `max`, never upscaling. Each output pixel averages the
/// block of source pixels that maps into it, which suppresses noise and grain
/// aliasing.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn resize_area(
    samples: &[f32],
    width: u32,
    height: u32,
    max: u32,
    channels: usize,
) -> (Vec<f32>, u32, u32) {
    let scale = f32::min(
        1.0,
        f32::min(max as f32 / width as f32, max as f32 / height as f32),
    );
    let out_width = u32::max((width as f32 * scale) as u32, 1);
    let out_height = u32::max((height as f32 * scale) as u32, 1);

    let mut resized = Vec::with_capacity(out_width as usize * out_height as usize * channels);
    let mut sums = vec![0.0_f32; channels];
    for out_y in 0..out_height {
        let row_start = range(out_y, height, out_height);
        let rows = range_len(row_start, range(out_y + 1, height, out_height));
        for out_x in 0..out_width {
            let col_start = range(out_x, width, out_width);
            let cols = range_len(col_start, range(out_x + 1, width, out_width));
            let count = (rows * cols) as f32;

            sums.fill(0.0);
            for y in row_start..row_start + rows {
                for x in col_start..col_start + cols {
                    let offset = (y * width as usize + x) * channels;
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += samples[offset + channel];
                    }
                }
            }

            resized.extend(sums.iter().map(|sum| sum / count));
        }
    }

    (resized, out_width, out_height)
}

/// Start of the source range that output coordinate `out` covers.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn range(out: u32, source: u32, out_total: u32) -> usize {
    (u64::from(out) * u64::from(source) / u64::from(out_total)) as usize
}

/// Length of the source range between `start` and the next output coordinate's
/// start, always covering at least one source pixel.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn range_len(start: usize, next_start: usize) -> usize {
    usize::max(next_start.saturating_sub(start), 1)
}

/// Strength of the post-downscale unsharp mask; 0 disables.
const UNSHARP_AMOUNT: f32 = 0.4;

/// Applies a gentle unsharp mask to a linear buffer, restoring edge punch lost
/// to heavy downscaling. Runs before tone encoding so overshoot stays out of
/// the perceptually amplified display range and shadow noise stays quiet.
fn unsharp_mask(samples: &mut [f32], width: usize, height: usize) {
    let blurred = blur_121(samples, width, height);
    for (slot, blur) in samples.iter_mut().zip(blurred) {
        *slot = (*slot + UNSHARP_AMOUNT * (*slot - blur)).clamp(0.0, 1.0);
    }
}

/// Separable 3x3 binomial blur ([1, 2, 1] per axis), replicating edges.
fn blur_121(samples: &[f32], width: usize, height: usize) -> Vec<f32> {
    let mut horizontal = vec![0.0_f32; samples.len()];
    for y in 0..height {
        for x in 0..width {
            let left = samples[y * width + x.saturating_sub(1)];
            let center = samples[y * width + x];
            let right = samples[y * width + usize::min(x + 1, width - 1)];

            horizontal[y * width + x] = (left + 2.0 * center + right) / 4.0;
        }
    }

    let mut blurred = vec![0.0_f32; samples.len()];
    for y in 0..height {
        let up = y.saturating_sub(1);
        let down = usize::min(y + 1, height - 1);
        for x in 0..width {
            blurred[y * width + x] = (horizontal[up * width + x]
                + 2.0 * horizontal[y * width + x]
                + horizontal[down * width + x])
                / 4.0;
        }
    }

    blurred
}

/// Applies the RAW orientation metadata to a linear mono buffer.
///
/// Mirror of [`orient`] for `Vec<f32>` data going to the GPU shader: same
/// transformations, per-element instead of per-RGBA-bunch. Equivalent in
/// behaviour to orientation-after-quantization when the per-element source
/// `(sx, sy)` mapping is identical.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn orient_mono(
    mono: &[f32],
    width: u32,
    height: u32,
    orientation: rawloader::Orientation,
) -> (Vec<f32>, u32, u32) {
    use rawloader::Orientation;

    let (out_width, out_height) = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Transpose
        | Orientation::Transverse => (height, width),
        _ => (width, height),
    };

    let mut oriented = vec![0.0_f32; (out_width * out_height) as usize];
    for y in 0..out_height {
        for x in 0..out_width {
            let (sx, sy) = match orientation {
                Orientation::Normal | Orientation::Unknown => (x, y),
                Orientation::HorizontalFlip => (width - 1 - x, y),
                Orientation::Rotate180 => (width - 1 - x, height - 1 - y),
                Orientation::VerticalFlip => (x, height - 1 - y),
                Orientation::Transpose => (y, x),
                Orientation::Transverse => (height - 1 - y, width - 1 - x),
                Orientation::Rotate90 => (y, height - 1 - x),
                Orientation::Rotate270 => (width - 1 - y, x),
            };

            let source = (sy * width + sx) as usize;
            let target = (y * out_width + x) as usize;
            oriented[target] = mono[source];
        }
    }

    (oriented, out_width, out_height)
}

/// Applies the RAW orientation metadata to an RGBA buffer.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn orient(
    rgba: &[u8],
    width: u32,
    height: u32,
    orientation: rawloader::Orientation,
) -> (Vec<u8>, u32, u32) {
    use rawloader::Orientation;

    let (out_width, out_height) = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Transpose
        | Orientation::Transverse => (height, width),
        _ => (width, height),
    };

    let mut oriented = vec![0_u8; (out_width * out_height * 4) as usize];
    for y in 0..out_height {
        for x in 0..out_width {
            let (sx, sy) = match orientation {
                Orientation::Normal | Orientation::Unknown => (x, y),
                Orientation::HorizontalFlip => (width - 1 - x, y),
                Orientation::Rotate180 => (width - 1 - x, height - 1 - y),
                Orientation::VerticalFlip => (x, height - 1 - y),
                Orientation::Transpose => (y, x),
                Orientation::Transverse => (height - 1 - y, width - 1 - x),
                Orientation::Rotate90 => (y, height - 1 - x),
                Orientation::Rotate270 => (width - 1 - y, x),
            };

            let source = ((sy * width + sx) * 4) as usize;
            let target = ((y * out_width + x) * 4) as usize;
            oriented[target..target + 4].copy_from_slice(&rgba[source..source + 4]);
        }
    }

    (oriented, out_width, out_height)
}

/// Whether a finished hi-res decode belongs to the current selection and is
/// the one this model dispatched.
///
/// Guards [`Message::DetailReady`] against results from selections that were
/// replaced or closed while their decode was still running.
fn detail_result_is_current(
    selected: Option<&str>,
    inflight: Option<&str>,
    finished: &str,
) -> bool {
    selected == Some(finished) && inflight == Some(finished)
}

/// The context page to display in the context drawer.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ContextPage {
    #[default]
    About,
    /// The editing panel for the active detail view.
    Editing,
    /// The metadata drawer for the library roll selection (`library_selection`).
    RollInfo,
    /// The frame-info drawer on the frames grid: name, dimensions, and basic
    /// EXIF for the highlighted frame (`frame_selected`).
    FrameInfo,
}

/// The three navigational views that own a context drawer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum DrawerView {
    /// The library page → [`ContextPage::RollInfo`].
    Library,
    /// The open roll's frame grid → [`ContextPage::FrameInfo`].
    Grid,
    /// The open frame's detail view → [`ContextPage::Editing`].
    Detail,
}

impl DrawerView {
    /// The drawer page this view shows.
    #[must_use]
    const fn page(self) -> ContextPage {
        match self {
            Self::Library => ContextPage::RollInfo,
            Self::Grid => ContextPage::FrameInfo,
            Self::Detail => ContextPage::Editing,
        }
    }
}

/// Per-view context-drawer state: whether each view's drawer is open, remembered
/// across navigation. Space toggles only the current view's slot; moving
/// between views restores the incoming view's own remembered state (Esc and
/// navigation never close a drawer).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct DrawerMemory {
    library: bool,
    grid: bool,
    detail: bool,
}

impl DrawerMemory {
    #[must_use]
    const fn get(self, view: DrawerView) -> bool {
        match view {
            DrawerView::Library => self.library,
            DrawerView::Grid => self.grid,
            DrawerView::Detail => self.detail,
        }
    }

    fn set(&mut self, view: DrawerView, open: bool) {
        match view {
            DrawerView::Library => self.library = open,
            DrawerView::Grid => self.grid = open,
            DrawerView::Detail => self.detail = open,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    AddRoll,
    RemoveRoll,
    Quit,
    SelectAll,
    CopyEdits,
    PasteEdits,
    Export,
    CalibrateBase,
    AutoBase,
    PresetBase,
    About,
    Details,
}

impl menu::action::MenuAction for MenuAction {
    type Message = Message;

    fn message(&self) -> Self::Message {
        match self {
            MenuAction::AddRoll => Message::AddRoll,
            MenuAction::RemoveRoll => Message::RemoveSelectedRoll,
            MenuAction::Quit => Message::Quit,
            MenuAction::SelectAll => Message::SelectAllFrames,
            MenuAction::CopyEdits => Message::CopyEdits,
            MenuAction::PasteEdits => Message::PasteEdits,
            MenuAction::Export => Message::ExportRequested,
            MenuAction::CalibrateBase => Message::CalibrateBaseFromFrame,
            MenuAction::AutoBase => Message::AutoBasePerFrame,
            MenuAction::PresetBase => Message::UsePresetBase,
            MenuAction::About => Message::ToggleContextPage(ContextPage::About),
            MenuAction::Details => Message::ToggleContext,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roll(dir: &str, name: &str) -> Roll {
        Roll {
            dir: PathBuf::from(dir),
            name: name.to_string(),
            cover: None,
            frame_count: 0,
            preset: FilmPreset::default(),
            start_date: None,
            end_date: None,
            thumb: Thumb::Loading,
        }
    }

    fn tile(name: &str) -> Tile {
        Tile {
            name: name.to_string(),
            thumb: Thumb::Loading,
            meta: None,
            meta_failed: false,
        }
    }

    #[test]
    fn valid_iso_date_accepts_real_dates() {
        assert!(valid_iso_date("2024-05-09"));
        assert!(valid_iso_date("2024-02-29")); // leap year
        assert!(valid_iso_date("2000-02-29")); // 400-year leap
        assert!(valid_iso_date("1900-12-31"));
    }

    #[test]
    fn valid_iso_date_rejects_impossible_dates() {
        assert!(!valid_iso_date("2023-02-29")); // not a leap year
        assert!(!valid_iso_date("1900-02-29")); // 100-year non-leap
        assert!(!valid_iso_date("2024-13-01")); // month 13
        assert!(!valid_iso_date("2024-00-01")); // month 0
        assert!(!valid_iso_date("2024-04-31")); // April has 30 days
        assert!(!valid_iso_date("2024-01-00")); // day 0
    }

    /// The English short month table, exercising the localized-format path
    /// with the fallback locale's values.
    fn months_jan_to_dec() -> [&'static str; 12] {
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
    }

    #[test]
    fn format_roll_card_dates_single_date_and_equal_range() {
        let months = months_jan_to_dec();
        assert_eq!(
            format_roll_card_dates(&months, "2026-05-03", None),
            Some("May 3 2026".to_owned())
        );
        // A one-day range collapses to the single-date form.
        assert_eq!(
            format_roll_card_dates(&months, "2026-05-03", Some("2026-05-03")),
            Some("May 3 2026".to_owned())
        );
    }

    #[test]
    fn format_roll_card_dates_same_month_share_month_and_year() {
        let months = months_jan_to_dec();
        assert_eq!(
            format_roll_card_dates(&months, "2026-05-03", Some("2026-05-04")),
            Some("May 3 - 4 2026".to_owned())
        );
    }

    #[test]
    fn format_roll_card_dates_same_year_keep_both_months() {
        let months = months_jan_to_dec();
        assert_eq!(
            format_roll_card_dates(&months, "2026-05-30", Some("2026-06-02")),
            Some("May 30 - Jun 2 2026".to_owned())
        );
    }

    #[test]
    fn format_roll_card_dates_cross_year_keep_both_years() {
        let months = months_jan_to_dec();
        assert_eq!(
            format_roll_card_dates(&months, "2025-12-30", Some("2026-01-02")),
            Some("Dec 30 2025 - Jan 2 2026".to_owned())
        );
    }

    #[test]
    fn format_roll_card_dates_rejects_malformed_input() {
        let months = months_jan_to_dec();
        assert_eq!(format_roll_card_dates(&months, "not-a-date", None), None);
        assert_eq!(
            format_roll_card_dates(&months, "2026-05-03", Some("not-a-date")),
            None
        );
        assert_eq!(format_roll_card_dates(&months, "2026-02-30", None), None);
    }

    #[test]
    fn valid_iso_date_rejects_malformed_input() {
        assert!(!valid_iso_date("2024-5-9")); // not zero-padded
        assert!(!valid_iso_date("2024/05/09")); // wrong separator
        assert!(!valid_iso_date("may 9 2024"));
        assert!(!valid_iso_date("2024-05-09-10")); // trailing noise
        assert!(!valid_iso_date(""));
    }

    #[test]
    fn roll_dates_require_start_before_end_when_both_set() {
        // Either date alone is always coherent.
        assert!(roll_dates_valid(None, None));
        assert!(roll_dates_valid(Some("2024-05-09"), None));
        assert!(roll_dates_valid(None, Some("2024-05-09")));
        // Equal dates are a valid single-day roll.
        assert!(roll_dates_valid(Some("2024-05-09"), Some("2024-05-09")));
        // A start before its end is the healthy case.
        assert!(roll_dates_valid(Some("2024-05-09"), Some("2024-05-11")));
        // The roll must not end before it starts.
        assert!(!roll_dates_valid(Some("2024-05-11"), Some("2024-05-09")));
        // Cross-year comparisons are plain ISO ordering, not calendar math.
        assert!(roll_dates_valid(Some("2023-12-31"), Some("2024-01-01")));
    }

    #[test]
    fn filtered_rolls_matches_case_insensitive_substring() {
        let rolls = vec![roll("/a", "Alpha"), roll("/b", "chicago")];

        let matched = filtered_rolls(&rolls, "CHI");

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "chicago");
    }

    #[test]
    fn filtered_rolls_returns_all_on_empty_query() {
        let rolls = vec![roll("/a", "Alpha"), roll("/b", "Beta")];

        assert_eq!(filtered_rolls(&rolls, "").len(), 2);
        assert_eq!(filtered_rolls(&rolls, "   ").len(), 2);
    }

    #[test]
    fn library_cells_keep_the_add_tile_first() {
        let rolls = vec![roll("/a", "Alpha"), roll("/b", "Beta")];

        let cells = library_cells(&rolls, "");

        assert_eq!(cells.len(), 3);
        assert!(matches!(cells[0], LibraryCell::AddRoll));
        // Rolls follow in filtered order.
        assert!(matches!(cells[1], LibraryCell::Roll(r) if r.name == "Alpha"));
        assert!(matches!(cells[2], LibraryCell::Roll(r) if r.name == "Beta"));
        // A whitespace-only query is an inactive search: the add tile stays.
        assert_eq!(library_cells(&rolls, "   ").len(), 3);
    }

    #[test]
    fn library_cells_hide_the_add_tile_while_searching() {
        let rolls = vec![roll("/a", "Alpha"), roll("/b", "Beta")];

        // A non-empty query drops the Add Roll tile: only matches remain.
        let cells = library_cells(&rolls, "beta");
        assert_eq!(cells.len(), 1);
        assert!(matches!(cells[0], LibraryCell::Roll(r) if r.name == "Beta"));

        // A query matching nothing yields an entirely empty cell set.
        let cells = library_cells(&rolls, "zzz");
        assert!(cells.is_empty());
        // No rolls at all: still the add tile while idle.
        assert_eq!(library_cells(&[], "").len(), 1);
    }

    #[test]
    fn library_cells_put_undated_first_then_newest_dated_rolls() {
        let mut newest = roll("/a", "Started");
        newest.start_date = Some("2024-06-01".into());
        let mut older = roll("/b", "Winter");
        older.start_date = Some("2023-12-31".into());
        let mut same_date_zeta = roll("/c", "Zulu");
        same_date_zeta.start_date = Some("2024-05-09".into());
        let mut same_date_alpha = roll("/d", "Alpha");
        same_date_alpha.start_date = Some("2024-05-09".into());
        let undated = roll("/e", "No-roll");

        let mixed = [
            same_date_alpha.clone(),
            older.clone(),
            same_date_zeta.clone(),
            newest.clone(),
            undated.clone(),
        ];
        let cells = library_cells(&mixed, "");

        // Add Roll cell 0, then undated roll, then dated newest-first, with
        // same-day rolls tied by name.
        assert!(matches!(cells[0], LibraryCell::AddRoll));
        assert!(matches!(cells[1], LibraryCell::Roll(r) if r.name == "No-roll"));
        assert!(matches!(cells[2], LibraryCell::Roll(r) if r.name == "Started"));
        assert!(matches!(cells[3], LibraryCell::Roll(r) if r.name == "Alpha"));
        assert!(matches!(cells[4], LibraryCell::Roll(r) if r.name == "Zulu"));
        assert!(matches!(cells[5], LibraryCell::Roll(r) if r.name == "Winter"));

        // The sort is derived: committing a date to the undated roll reorders
        // it on the next render without touching the backing slice.
        let mut undated = undated.clone();
        undated.start_date = Some("2025-01-01".into());
        let reordered = [newest.clone(), undated.clone(), same_date_alpha];
        let cells = library_cells(&reordered, "");
        assert!(matches!(cells[1], LibraryCell::Roll(r) if r.name == "No-roll"));
        assert!(matches!(cells[2], LibraryCell::Roll(r) if r.name == "Started"));
    }

    #[test]
    fn library_cell_index_finds_add_and_roll_slots() {
        let rolls = vec![roll("/a", "Alpha"), roll("/b", "Beta")];
        let cells = library_cells(&rolls, "");

        assert_eq!(
            library_cell_index(Some(&LibrarySelection::AddRoll), &cells),
            Some(0)
        );
        assert_eq!(
            library_cell_index(Some(&LibrarySelection::Roll(PathBuf::from("/b"))), &cells),
            Some(2)
        );
        // No selection, or one not in the (filtered) set, yields None.
        assert_eq!(library_cell_index(None, &cells), None);
        assert_eq!(
            library_cell_index(Some(&LibrarySelection::Roll(PathBuf::from("/x"))), &cells),
            None
        );
    }

    #[test]
    fn nav_can_land_on_and_leave_the_add_tile() {
        let rolls = vec![roll("/a", "Alpha"), roll("/b", "Beta")];
        let cells = library_cells(&rolls, "");

        // From nothing, Down selects cell 0 (the add tile).
        let target = nav_target(None, cells.len(), 2, MoveDir::Down).unwrap();
        assert_eq!(target, 0);
        assert!(matches!(&cells[target], LibraryCell::AddRoll));

        // From the add tile, Right moves to the first roll (cell 1).
        let target = nav_target(Some(0), cells.len(), 2, MoveDir::Right).unwrap();
        assert_eq!(target, 1);
        assert!(matches!(&cells[target], LibraryCell::Roll(r) if r.name == "Alpha"));

        // From cell 1, Left returns to the add tile.
        let target = nav_target(Some(1), cells.len(), 2, MoveDir::Left).unwrap();
        assert_eq!(target, 0);
        assert!(matches!(&cells[target], LibraryCell::AddRoll));
        // Left on the add tile does not wrap.
        assert_eq!(nav_target(Some(0), cells.len(), 2, MoveDir::Left), Some(0));
    }

    #[test]
    fn nav_target_steps_within_the_row() {
        assert_eq!(nav_target(Some(1), 8, 3, MoveDir::Left), Some(0));
        assert_eq!(nav_target(Some(6), 8, 3, MoveDir::Right), Some(7));
        // No wrap at the last item.
        assert_eq!(nav_target(Some(7), 8, 3, MoveDir::Right), Some(7));
    }

    #[test]
    fn nav_target_jumps_full_rows_vertically() {
        assert_eq!(nav_target(Some(4), 8, 3, MoveDir::Up), Some(1));
        assert_eq!(nav_target(Some(4), 8, 3, MoveDir::Down), Some(7));
    }

    #[test]
    fn nav_target_clamps_to_grid_edges() {
        // First row: Up clamps to the top item.
        assert_eq!(nav_target(Some(1), 8, 3, MoveDir::Up), Some(0));
        // Last row: Down clamps to the last item.
        assert_eq!(nav_target(Some(7), 8, 3, MoveDir::Down), Some(7));
    }

    #[test]
    fn nav_target_selects_the_first_item_from_no_selection() {
        assert_eq!(nav_target(None, 4, 3, MoveDir::Down), Some(0));
    }

    #[test]
    fn nav_target_handles_a_single_column_grid() {
        assert_eq!(nav_target(Some(1), 4, 1, MoveDir::Up), Some(0));
        assert_eq!(nav_target(Some(1), 4, 1, MoveDir::Down), Some(2));
    }

    #[test]
    fn nav_target_empty_list_has_no_target() {
        assert_eq!(nav_target(Some(0), 0, 3, MoveDir::Left), None);
    }

    #[test]
    fn filtered_tiles_matches_case_insensitive_substring() {
        let tiles = vec![tile("DSC_0001.CR2"), tile("scan-roll2.tif")];

        let matched = filtered_tiles(&tiles, "dsc");

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "DSC_0001.CR2");
    }

    #[test]
    fn filtered_tiles_returns_all_on_empty_query() {
        let tiles = vec![tile("a"), tile("b")];

        assert_eq!(filtered_tiles(&tiles, "").len(), 2);
        assert_eq!(filtered_tiles(&tiles, "   ").len(), 2);
    }

    #[test]
    fn paginate_steps_left_and_right_within_visible_set() {
        assert_eq!(paginate(1, 5, MoveDir::Left), Some(0));
        assert_eq!(paginate(1, 5, MoveDir::Right), Some(2));
    }

    #[test]
    fn paginate_clamps_at_both_ends() {
        assert_eq!(paginate(0, 5, MoveDir::Left), Some(0));
        assert_eq!(paginate(4, 5, MoveDir::Right), Some(4));
    }

    #[test]
    fn paginate_never_moves_vertically() {
        assert_eq!(paginate(2, 5, MoveDir::Up), None);
        assert_eq!(paginate(2, 5, MoveDir::Down), None);
    }

    #[test]
    fn paginate_empty_list_has_no_target() {
        assert_eq!(paginate(0, 0, MoveDir::Right), None);
    }

    #[test]
    fn grid_num_cols_matches_iced_ceil_math() {
        // (1200 + 16) / (384 + 16) = 3.04 → ceil 4
        assert_eq!(grid_num_cols(1200.0, 16.0), 4);
        // (400 + 16) / (384 + 16) = 1.04 → ceil 2
        assert_eq!(grid_num_cols(400.0, 16.0), 2);
    }

    #[test]
    fn reveal_target_y_no_movement_when_visible() {
        // 3-column grid, tile 4 sits in row 1 (top = 16 + 400*1 = 416) inside a
        // 900-high viewport scrolled to 0.
        assert_eq!(
            reveal_target_y(3, 4, 16.0, 16.0, 384.0, 900.0, 0.0, 3000.0),
            None
        );
    }

    #[test]
    fn reveal_target_y_scrolls_down_below_the_fold() {
        // tile 30 in row 10: top = 16 + 400*10 = 4016, bottom = 4400. Viewport
        // is 900 tall, scrolled to 1000 → fold at 1900. Scroll to 4400 - 900.
        assert_eq!(
            reveal_target_y(3, 30, 16.0, 16.0, 384.0, 900.0, 1000.0, 30000.0),
            Some(3500.0)
        );
    }

    #[test]
    fn reveal_target_y_scrolls_up_above_the_viewport() {
        // Tile 2 in row 0 (top = 16) is above a viewport scrolled to 300 —
        // scroll back to 16.
        assert_eq!(
            reveal_target_y(3, 2, 16.0, 16.0, 384.0, 900.0, 300.0, 3000.0),
            Some(16.0)
        );
    }

    #[test]
    fn reveal_target_y_clamps_to_content() {
        // tile 30 in row 10 wants 4400 - 1000 = 3400, but content is only 2700
        // tall, so the scroll clamps to 2700 - 1000 = 1700.
        assert_eq!(
            reveal_target_y(3, 30, 16.0, 16.0, 384.0, 1000.0, 0.0, 2700.0),
            Some(1700.0)
        );
    }

    #[test]
    fn reveal_target_y_zero_columns_is_no_op() {
        assert_eq!(
            reveal_target_y(0, 0, 16.0, 16.0, 384.0, 900.0, 0.0, 3000.0),
            None
        );
    }

    #[test]
    fn detail_result_applies_to_its_own_selection() {
        assert!(detail_result_is_current(Some("a"), Some("a"), "a"));
    }

    #[test]
    fn detail_result_rejected_for_replaced_selection() {
        // "a" finished, but the user already moved on to "b".
        assert!(!detail_result_is_current(Some("b"), Some("a"), "a"));
    }

    #[test]
    fn detail_result_rejected_after_close() {
        assert!(!detail_result_is_current(None, Some("a"), "a"));
    }

    #[test]
    fn detail_result_rejected_without_inflight_request() {
        assert!(!detail_result_is_current(Some("a"), None, "a"));
    }

    #[test]
    fn srgb_encode_matches_reference_values() {
        let assert_close = |value: f32, expected: f32| {
            assert!((srgb_encode(value) - expected).abs() < 1e-6);
        };

        assert_close(0.0, 0.0);
        assert_close(1.0, 1.0);
        assert_close(0.5, 0.735_356_98);
        assert!(srgb_encode(0.25) < srgb_encode(0.5));
        assert_close(-0.5, 0.0);
        assert_close(1.5, 1.0);
    }

    #[test]
    fn flatten_bayer_scales_classes_to_a_common_base() {
        // Neutral film at transmission 0.5 seen through per-class sensor casts
        // (RGGB layout: one R, two G, one B site).
        let cfa = rawloader::CFA::new("RGGB");
        let samples = [0.60, 0.50, 0.50, 0.40]; // R, G, G, B sites

        let mono = flatten_bayer(&samples, 2, 2, &cfa);

        assert!(mono.iter().all(|value| (value - 0.5).abs() < 1e-6));
    }

    #[test]
    fn flatten_bayer_respects_cfa_phase() {
        // GBRG layout: blue sits top-left; only that site carries the cast.
        let cfa = rawloader::CFA::new("GBRG");
        let samples = [0.25, 0.20, 0.30, 0.25]; // G, B, R, G sites

        let mono = flatten_bayer(&samples, 2, 2, &cfa);

        assert!(mono.iter().all(|value| (value - 0.25).abs() < 1e-6));
    }

    #[test]
    fn flatten_bayer_handles_fourth_color_sites() {
        // Emerald-class sites get their own measured base like any other.
        let cfa = rawloader::CFA::new("RGBE");
        let samples = [0.45, 0.50, 0.55, 0.50]; // R, G, B, E sites

        let mono = flatten_bayer(&samples, 2, 2, &cfa);

        assert!(mono.iter().all(|value| (value - 0.5).abs() < 1e-6));
    }

    #[test]
    fn unsharp_mask_leaves_flat_buffers_unchanged() {
        let mut flat = vec![0.42_f32; 16];

        unsharp_mask(&mut flat, 4, 4);

        assert!(flat.iter().all(|value| (value - 0.42).abs() < 1e-6));
    }

    #[test]
    fn unsharp_mask_increases_local_contrast() {
        // Horizontal mid-gray step edge across a single row.
        let mut edge = vec![0.2_f32, 0.2, 0.2, 0.8, 0.8, 0.8];

        unsharp_mask(&mut edge, 6, 1);

        assert!(edge[2] < 0.2); // dark side dips further
        assert!(edge[3] > 0.8); // bright side overshoots
    }

    #[test]
    fn unsharp_mask_clamps_to_unit_range() {
        // A lone bright spike would overshoot past white without clamping.
        let mut spike = vec![0.0_f32; 9];
        spike[4] = 1.0;

        unsharp_mask(&mut spike, 3, 3);

        assert!(spike.iter().all(|value| (0.0..=1.0).contains(value)));
        assert_eq!(spike[4], 1.0);
    }

    #[test]
    fn luma_preserves_neutral_levels() {
        assert!((luma(&[1.0, 1.0, 1.0])[0] - 1.0).abs() < 1e-6);
        assert!((luma(&[0.25, 0.25, 0.25])[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn luma_weights_green_over_blue() {
        let green = luma(&[0.0, 1.0, 0.0])[0];
        let blue = luma(&[0.0, 0.0, 1.0])[0];

        assert!(green > blue);
        assert!((green - 0.715_2).abs() < 1e-6);
        assert!((blue - 0.072_2).abs() < 1e-6);
    }

    #[test]
    fn apply_exposure_scales_in_linear_light() {
        let mut mono = [0.5_f32; 8];

        apply_exposure(&mut mono, 1.0);
        assert!(mono.iter().all(|value| (*value - 1.0).abs() < 1e-6));

        apply_exposure(&mut mono, -1.0);
        assert!(mono.iter().all(|value| (*value - 0.5).abs() < 1e-6));

        // EV=0 → 2^0 = 1.0, identity.
        apply_exposure(&mut mono, 0.0);
        assert!(mono.iter().all(|value| (*value - 0.5).abs() < 1e-6));

        // 1 EV doubles linear light at any level.
        let mut dim = [0.125_f32; 4];
        apply_exposure(&mut dim, 1.0);
        assert!(dim.iter().all(|value| (*value - 0.25).abs() < 1e-6));
    }

    #[test]
    fn exposure_math_ev_zero_is_identity() {
        // EV=0 → 2^0 = 1.0, so exposed = mono × 1.0 = mono.
        let mono = 0.5_f32;
        let exposed = mono * 2.0_f32.powf(0.0);
        assert!((exposed - 0.5).abs() < 1e-6);

        // sRGB(0.5) ≈ 188/255.
        let expected_level = (srgb_encode(0.5) * 255.0).round() as u8;
        assert_eq!(expected_level, 188);

        // EV +1 → 0.5 × 2 = 1.0 → sRGB(1.0) = 255.
        let bright = mono * 2.0_f32.powf(1.0);
        let bright_level = (srgb_encode(bright) * 255.0).round() as u8;
        assert_eq!(bright_level, 255);

        // EV −1 → 0.5 × 0.5 = 0.25 → sRGB(0.25) ≈ 137.
        let dark = mono * 2.0_f32.powf(-1.0);
        let dark_level = (srgb_encode(dark) * 255.0).round() as u8;
        assert_eq!(dark_level, 137);

        assert!(bright_level > dark_level);
    }

    #[test]
    fn crop_extracts_inner_region() {
        let samples: Vec<f32> = (0_u16..12).map(f32::from).collect();

        let (cropped, width, height) = crop_samples(&samples, 4, 3, [1, 1, 1, 2]).unwrap();

        assert_eq!((width, height), (1, 1));
        assert_eq!(cropped, vec![6.0]);
    }

    #[test]
    fn crop_rejects_degenerate_regions() {
        assert!(crop_samples(&[0.0; 4], 2, 2, [2, 0, 0, 0]).is_none());
        assert!(crop_samples(&[0.0; 4], 2, 2, [0, 1, 0, 2]).is_none());
    }

    #[test]
    fn resize_area_averages_source_blocks() {
        // Gray ramp 1.0..16.0 over a 4x4 buffer.
        let rgb: Vec<f32> = (1_u16..=16)
            .flat_map(|value| [f32::from(value); 3])
            .collect();

        let (out, width, height) = resize_area(&rgb, 4, 4, 2, 3);

        assert_eq!((width, height), (2, 2));
        assert_eq!(
            out,
            vec![
                3.5, 3.5, 3.5, // mean of {1, 2, 5, 6}
                5.5, 5.5, 5.5, // mean of {3, 4, 7, 8}
                11.5, 11.5, 11.5, // mean of {9, 10, 13, 14}
                13.5, 13.5, 13.5, // mean of {11, 12, 15, 16}
            ]
        );
    }

    #[test]
    fn resize_area_is_identity_without_shrink() {
        let rgb: Vec<f32> = (0_u16..4).flat_map(|value| [f32::from(value); 3]).collect();

        let (out, width, height) = resize_area(&rgb, 2, 2, 384, 3);

        assert_eq!((width, height), (2, 2));
        assert_eq!(out, rgb);
    }

    #[test]
    fn resize_area_handles_single_channel_samples() {
        let samples: Vec<f32> = (1_u16..=4).map(f32::from).collect();

        let (out, width, height) = resize_area(&samples, 2, 2, 1, 1);

        assert_eq!((width, height), (1, 1));
        assert_eq!(out, vec![2.5]); // mean of {1, 2, 3, 4}
    }

    /// Builds an Integer RAW whose per-class normalization yields the supplied
    /// per-site transmissions: raw = transmission × 1000, white = 1000,
    /// black = 0, so cast compression happens exactly at sample time.
    fn raw_with_transmissions(
        width: usize,
        height: usize,
        pattern: &str,
        transmissions: Vec<u16>, // raw codes, one per site
        crops: [usize; 4],
    ) -> rawloader::RawImage {
        rawloader::RawImage {
            make: String::new(),
            model: String::new(),
            clean_make: String::new(),
            clean_model: String::new(),
            width,
            height,
            cpp: 1,
            wb_coeffs: [1.0; 4],
            whitelevels: [1000; 4],
            blacklevels: [0; 4],
            xyz_to_cam: [[0.0; 3]; 4],
            cfa: rawloader::CFA::new(pattern),
            crops,
            blackareas: Vec::new(),
            orientation: rawloader::Orientation::Normal,
            data: rawloader::RawImageData::Integer(transmissions),
        }
    }

    #[test]
    fn downsample_thumbnail_removes_casts_at_full_scale() {
        // Neutral film at normalized transmission 0.5 with per-class captures
        // (RGGB: one R, two G, one B site): the fused downscaler must rescale
        // each class onto the green-anchored reference.
        let image = raw_with_transmissions(
            2,
            2,
            "RGGB",
            vec![600, 500, 500, 400], // 0.6, 0.5, 0.5, 0.4 after normalization
            [0, 0, 0, 0],
        );

        let (mono, width, height) = downsample_thumbnail(&image, 2).unwrap();

        assert_eq!((width, height), (2, 2));
        assert!(mono.iter().all(|value| (value - 0.5).abs() < 1e-5));
    }

    #[test]
    fn downsample_thumbnail_preserves_phase_after_averaging() {
        // Same class casts compressed into one 1x1 output pixel: the per-class
        // averages must be rescalled before combining, or the cast survives.
        let image = raw_with_transmissions(2, 2, "RGGB", vec![600, 500, 500, 400], [0, 0, 0, 0]);

        let (mono, width, height) = downsample_thumbnail(&image, 1).unwrap();

        assert_eq!((width, height), (1, 1));
        assert!((mono[0] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn downsample_thumbnail_handles_fourth_color_sites() {
        let image = raw_with_transmissions(
            2,
            2,
            "RGBE",
            vec![450, 500, 550, 500], // R, G, B, E sites
            [0, 0, 0, 0],
        );

        let (mono, width, height) = downsample_thumbnail(&image, 1).unwrap();

        assert_eq!((width, height), (1, 1));
        assert!((mono[0] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn downsample_thumbnail_respects_crops_and_phase() {
        // 4x4 RGGB with per-class captures, cropped off the left border. The
        // surviving region must hit the absolute CFA (not rephase against the
        // crop origin), so the class casts compress to uniform 0.5 as usual.
        let image = raw_with_transmissions(
            4,
            4,
            "RGGB",
            vec![
                600, 500, 600, 500, // R, G, R, G
                500, 400, 500, 400, // G, B, G, B
                600, 500, 600, 500, //
                500, 400, 500, 400, //
            ],
            [0, 0, 0, 1],
        );

        let (mono, width, height) = downsample_thumbnail(&image, 2).unwrap();

        assert_eq!((width, height), (1, 2));
        assert!(mono.iter().all(|value| (value - 0.5).abs() < 1e-5));
    }

    #[test]
    fn downsample_thumbnail_rejects_degenerate_crops() {
        let image = raw_with_transmissions(2, 2, "RGGB", vec![600, 500, 500, 400], [2, 0, 0, 0]);

        assert!(downsample_thumbnail(&image, 1).is_none());
    }

    #[test]
    fn downsample_rgb_is_luma_of_block_means() {
        // Float data is normalized by the global maximum before luma collapse;
        // the block mean of luma equals luma of the block means (linearity).
        let values: Vec<f32> = vec![
            0.5, 0.5, 0.5, //
            0.6, 0.4, 0.2, //
            0.2, 0.4, 0.6, //
            0.8, 0.1, 0.1, //
        ];
        let image = rawloader::RawImage {
            make: String::new(),
            model: String::new(),
            clean_make: String::new(),
            clean_model: String::new(),
            width: 2,
            height: 2,
            cpp: 3,
            wb_coeffs: [1.0; 4],
            whitelevels: [0; 4],
            blacklevels: [0; 4],
            xyz_to_cam: [[0.0; 3]; 4],
            cfa: rawloader::CFA::new("RGGB"),
            crops: [0, 0, 0, 0],
            blackareas: Vec::new(),
            orientation: rawloader::Orientation::Normal,
            data: rawloader::RawImageData::Float(values),
        };

        let (mono, width, height) = downsample_thumbnail(&image, 1).unwrap();

        assert_eq!((width, height), (1, 1));
        // Normalized by global max 0.8 → gain 1.25; then channel means and
        // Rec.709 luma.
        let expected = {
            let r = (0.625 + 0.75 + 0.25 + 1.0) / 4.0;
            let g = (0.625 + 0.5 + 0.5 + 0.125) / 4.0;
            let b = (0.625 + 0.25 + 0.75 + 0.125) / 4.0;
            0.212_6 * r + 0.715_2 * g + 0.072_2 * b
        };
        assert!((mono[0] - expected).abs() < 1e-5);
    }

    #[test]
    fn class_gains_anchors_to_green_when_present() {
        let gains = class_gains([0.6, 0.5, 0.4, 0.9], [false; 4]);

        assert!((gains[0] - 0.5 / 0.6).abs() < 1e-6); // R pulled up to green
        assert!((gains[1] - 1.0).abs() < 1e-6); // green is the reference
        assert!((gains[2] - 0.5 / 0.4).abs() < 1e-6); // B pulled down to green
        assert!((gains[3] - 0.5 / 0.9).abs() < 1e-6);
    }

    #[test]
    fn class_gains_falls_back_to_dimmest_class() {
        // Green absent: anchor onto the dimmest measurable class.
        let gains = class_gains([0.6, 0.5, 0.4, 0.9], [false, true, false, true]);

        assert!((gains[2] - 1.0).abs() < 1e-6); // dimmest (B) is the reference
        assert!((gains[0] - 0.4 / 0.6).abs() < 1e-6);
        assert!((gains[3] - 0.4 / 0.9).abs() < 1e-6);
    }

    #[test]
    fn class_gains_keeps_defaults_without_measurable_class() {
        assert_eq!(class_gains([0.1; 4], [true; 4]), [1.0; 4]);
    }

    #[test]
    fn none_preset_does_not_rescale_midtones_or_clamp_highlights() {
        // Regression: the `None` preset (already-positive RAW) previously ran
        // `normalize_positive`, which divided the whole frame by its own 95th
        // percentile and clamped everything above it to pure white. That baked
        // an auto-expose into the data before the EV slider and permanently
        // destroyed highlight detail. With the auto-expose removed, a frame
        // whose midtones sit at 0.5 relative to the sensor white point must
        // render those midtones as a real mid-gray — not scaled up to white —
        // and speculars above the 95th percentile must survive at the peak.
        //
        // 10x10 RGB Integer RAW, white = 1000: 97 sites at 0.5 (raw 500) and
        // 3 speculars at 1.0 (raw 1000), so the 95th percentile ≈ 0.5. Under
        // the old `normalize_positive` that anchor scaled the 0.5 midtones to
        // 1.0 and clamped the speculars — a uniform white frame. Today the
        // midtones must stay a mid-gray (~sRGB(0.5) ≈ 188).
        let mut values = Vec::with_capacity(10 * 10 * 3);
        for y in 0..10 {
            for x in 0..10 {
                let site = if (x == 0 && y == 0) || (x == 9 && y == 0) || (x == 4 && y == 9) {
                    1000
                } else {
                    500
                };
                values.extend_from_slice(&[site, site, site]);
            }
        }
        let image = rawloader::RawImage {
            make: String::new(),
            model: String::new(),
            clean_make: String::new(),
            clean_model: String::new(),
            width: 10,
            height: 10,
            cpp: 3,
            wb_coeffs: [1.0; 4],
            whitelevels: [1000; 4],
            blacklevels: [0; 4],
            xyz_to_cam: [[0.0; 3]; 4],
            cfa: rawloader::CFA::new("RGGB"),
            crops: [0, 0, 0, 0],
            blackareas: Vec::new(),
            orientation: rawloader::Orientation::Normal,
            data: rawloader::RawImageData::Integer(values),
        };

        // Identity curve + EV 0 so the sampled mono value is preserved exactly
        // through sRGB with no user gain.
        let tone = edit_manifest::ToneEdit {
            exposure_ev: 0.0,
            ..edit_manifest::ToneEdit::default()
        };
        let handle = convert_thumbnail(
            &image,
            10.0,
            tone,
            Default::default(),
            0,
            FilmPreset::None,
            BaseConfig::default(),
        )
        .expect("decode succeeds");
        let (width, height, pixels) = match &handle {
            cosmic::widget::image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            } => (*width, *height, pixels.as_ref()),
            _ => panic!("expected an RGBA handle"),
        };
        assert_eq!((width, height), (10, 10));
        // A midtone site far from any specular must render as a genuine
        // mid-gray, NOT a scaled-to-white 255 that `normalize_positive`
        // produced. Site (5,5) sits in the 0.5 bulk.
        let midtone = pixels[(5 * 10 + 5) * 4];
        assert!(midtone < 200, "midtone crushed too bright: {midtone}");
        assert!(midtone > 100, "midtone too dark: {midtone}");
        // The specular sites stay at the sensor peak (much brighter than the
        // bulk), proving highlight detail is preserved rather than crushed.
        let specular = pixels[(0 * 10 + 0) * 4];
        assert!(specular as i32 > midtone as i32 + 40, "specular crushed");
    }

    #[test]
    fn inverted_preset_brightens_on_positive_ev() {
        // User-facing EV means "brightness" for BOTH presets: +EV brightens a
        // film negative's positive exactly as it brightens an already-positive
        // scan. Since the rework moved the exposure gain onto the TRUE sensor
        // data (multiplied by 2^-EV there) and then per-fragment density-
        // inverts it, +EV must raise the density and therefore brighten the
        // positive — NOT darken it.
        //
        // A uniform frame equal to its own measured clear-film base prints all
        // black at EV 0 (every site sits at the measured black point). Raising
        // EV to +1 halves the transmission (2^-1), lifting the density to
        // log10(2), which must brighten the frame measurably.
        let values: Vec<u16> = vec![900; 10 * 10 * 3];
        let image = rawloader::RawImage {
            make: String::new(),
            model: String::new(),
            clean_make: String::new(),
            clean_model: String::new(),
            width: 10,
            height: 10,
            cpp: 3,
            wb_coeffs: [1.0; 4],
            whitelevels: [1000; 4],
            blacklevels: [0; 4],
            xyz_to_cam: [[0.0; 3]; 4],
            cfa: rawloader::CFA::new("RGGB"),
            crops: [0, 0, 0, 0],
            blackareas: Vec::new(),
            orientation: rawloader::Orientation::Normal,
            data: rawloader::RawImageData::Integer(values),
        };

        let bake = |ev: f32| {
            let tone = edit_manifest::ToneEdit {
                exposure_ev: ev,
                ..edit_manifest::ToneEdit::default()
            };
            let handle = convert_thumbnail(
                &image,
                10.0,
                tone,
                Default::default(),
                0,
                FilmPreset::Hp5Plus,
                BaseConfig::default(),
            )
            .expect("decode succeeds");
            let (width, height, pixels) = match &handle {
                cosmic::widget::image::Handle::Rgba {
                    width,
                    height,
                    pixels,
                    ..
                } => (*width, *height, pixels.as_ref()),
                _ => panic!("expected an RGBA handle"),
            };
            assert_eq!((width, height), (10, 10));
            let count = pixels.len() / 4;
            let sum: u32 = pixels.chunks_exact(4).map(|p| u32::from(p[0])).sum::<u32>();
            sum as f32 / count as f32
        };

        let ev0 = bake(0.0);
        let ev1 = bake(1.0);
        assert!(
            ev1 > ev0 + 20.0,
            "+1EV must brighten an inverted preset: {ev0} → {ev1}"
        );
    }

    #[test]
    fn apply_detail_zoom_clamps_at_both_ends() {
        let (zoom, _) = apply_detail_zoom(1.0, (0.0, 0.0), None, -1.0, MAX_DETAIL_ZOOM);
        assert_eq!(zoom, 1.0);
        let (zoom, _) = apply_detail_zoom(MAX_DETAIL_ZOOM, (0.0, 0.0), None, 9.0, MAX_DETAIL_ZOOM);
        assert_eq!(zoom, MAX_DETAIL_ZOOM);
    }

    #[test]
    fn apply_detail_zoom_clamps_at_the_hundred_percent_cap() {
        // The 1:1 cap is the maximum: zooming past it stops there, and the cap
        // itself is respected even when below MAX_DETAIL_ZOOM.
        let cap = 4.25;
        let (zoom, _) = apply_detail_zoom(4.0, (0.0, 0.0), None, 9.0, cap);
        assert_eq!(zoom, cap);
        // A cap below the current zoom clamps back down to it.
        let (zoom, _) = apply_detail_zoom(6.0, (0.0, 0.0), None, 0.0, cap);
        assert_eq!(zoom, cap);
    }

    #[test]
    fn apply_detail_zoom_returns_unchanged_pan_without_cursor() {
        let (zoom, pan) = apply_detail_zoom(2.0, (13.0, -7.0), None, 0.5, 8.0);
        assert!((zoom - 2.5).abs() < 1e-6);
        assert_eq!(pan, (13.0, -7.0));
    }

    #[test]
    fn apply_detail_zoom_recenters_when_back_to_contain_fit() {
        // Zooming all the way out must give the centered contain view.
        let (zoom, pan) =
            apply_detail_zoom(3.0, (50.0, -30.0), Some(Point::new(0.0, 0.0)), -2.0, 8.0);
        assert_eq!(zoom, 1.0);
        assert_eq!(pan, (0.0, 0.0));
    }

    #[test]
    fn zoom_about_anchor_round_trips() {
        // Zooming in then back out at the same cursor must return the exact
        // original pan (the anchored image point is pinned in both steps).
        let pan = (5.0, 6.0);
        let cursor = Point::new(-9.0, 4.0);
        let zoomed = zoom_about_anchor(2.0, 3.0, pan, cursor);
        let back = zoom_about_anchor(3.0, 2.0, zoomed, cursor);
        assert!((back.0 - pan.0).abs() < 1e-5);
        assert!((back.1 - pan.1).abs() < 1e-5);
    }

    #[test]
    fn zoom_about_anchor_keeps_center_pinned_when_cursor_is_center() {
        // Zooming about the image center (cursor == pan) leaves the pan
        // unchanged: center of the frame stays center of the widget.
        let pan = (12.0, -8.0);
        let out = zoom_about_anchor(2.0, 4.0, pan, Point::new(12.0, -8.0));
        assert!((out.0 - pan.0).abs() < 1e-5);
        assert!((out.1 - pan.1).abs() < 1e-5);
    }

    #[test]
    fn zoom_about_anchor_is_identity_at_delta_zero() {
        let pan = (5.0, 6.0);
        let out = zoom_about_anchor(2.0, 2.0, pan, Point::new(-9.0, 4.0));
        assert!((out.0 - pan.0).abs() < 1e-6);
        assert!((out.1 - pan.1).abs() < 1e-6);
    }

    #[test]
    fn detail_zoom_delta_converts_scroll_units() {
        let lines = cosmic::iced::mouse::ScrollDelta::Lines { x: 0.0, y: 2.0 };
        assert!((detail_zoom_delta(lines) - 1.0).abs() < 1e-6);
        let pixels = cosmic::iced::mouse::ScrollDelta::Pixels { x: 0.0, y: 400.0 };
        assert!((detail_zoom_delta(pixels) - 1.0).abs() < 1e-6);
        let up = cosmic::iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 };
        assert!((detail_zoom_delta(up) + 0.5).abs() < 1e-6);
    }

    #[test]
    fn lru_cache_get_marks_most_recently_used() {
        let mut cache = LruCache::new(2);
        assert!(cache.insert("a", 1).is_none());
        assert!(cache.insert("b", 2).is_none());
        // Touching "a" makes it most-recently used, so "b" is evicted next.
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.insert("c", 3), Some(2));
        assert!(cache.contains(&"a"));
        assert!(!cache.contains(&"b"));
        assert!(cache.contains(&"c"));
    }

    #[test]
    fn lru_cache_evicts_least_recently_used_at_capacity() {
        let mut cache = LruCache::new(3);
        assert!(cache.insert("a", 1).is_none());
        assert!(cache.insert("b", 2).is_none());
        assert!(cache.insert("c", 3).is_none());
        assert_eq!(cache.len(), 3);
        // Full: inserting "d" evicts the least-recently-used "a".
        assert_eq!(cache.insert("d", 4), Some(1));
        assert_eq!(cache.len(), 3);
        assert!(!cache.contains(&"a"));
        for key in ["b", "c", "d"] {
            assert!(cache.contains(&key));
        }
    }

    #[test]
    fn lru_cache_insert_same_key_updates_in_place() {
        let mut cache = LruCache::new(2);
        assert!(cache.insert("a", 1).is_none());
        // Re-inserting an existing key updates the value and never evicts.
        assert!(cache.insert("a", 10).is_none());
        assert_eq!(cache.get(&"a"), Some(&10));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn lru_cache_clear_drops_everything_and_reuse() {
        let mut cache = LruCache::new(2);
        assert!(cache.insert("a", 1).is_none());
        assert!(cache.insert("b", 2).is_none());
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(!cache.contains(&"a"));
        // The cache is usable again after clearing.
        assert!(cache.insert("c", 3).is_none());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn lru_cache_zero_capacity_insert_evicts_every_new_entry() {
        let mut cache = LruCache::new(0);
        assert_eq!(cache.insert("a", 1), Some(1));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn lru_cache_preset_keyed_entries_do_not_collide() {
        // Two presets for the same roll+frame are distinct cache entries, so a
        // preset change can never serve a stale inversion.
        let mut cache = LruCache::new(8);
        let dir = PathBuf::from("/rolls/a");
        let key_alpha = (dir.clone(), FilmPreset::Hp5Plus, "frame.DNG".to_string());
        let key_neutral = (dir, FilmPreset::None, "frame.DNG".to_string());
        assert!(cache.insert(key_alpha.clone(), 1).is_none());
        assert!(cache.insert(key_neutral.clone(), 2).is_none());
        assert_eq!(cache.get(&key_alpha), Some(&1));
        assert_eq!(cache.get(&key_neutral), Some(&2));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn lru_cache_retain_drops_only_matching_dir_and_preserves_recency() {
        let mut cache = LruCache::new(8);
        let roll_a = PathBuf::from("/rolls/a");
        let roll_b = PathBuf::from("/rolls/b");
        let key_a_none = (roll_a.clone(), FilmPreset::None, "1.DNG".to_string());
        let key_a_hp5 = (roll_a.clone(), FilmPreset::Hp5Plus, "2.DNG".to_string());
        let key_b = (roll_b.clone(), FilmPreset::None, "3.DNG".to_string());
        cache.insert(key_a_none.clone(), 1);
        cache.insert(key_a_hp5.clone(), 2);
        cache.insert(key_b.clone(), 3);
        // Touch the other roll so its recency stays intact through the retain.
        assert_eq!(cache.get(&key_b), Some(&3));

        // Dropping every entry for roll `a` (any preset) leaves roll `b` alone.
        let dropped = cache.retain(|(dir, _, _)| dir != &roll_a);
        assert_eq!(dropped.len(), 2);

        assert!(!cache.contains(&key_a_none));
        assert!(!cache.contains(&key_a_hp5));
        assert!(cache.contains(&key_b));
        assert_eq!(cache.get(&key_b), Some(&3));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clamp_ev_bounds_to_the_slider_range() {
        assert_eq!(clamp_ev(-99.0), -3.0);
        assert_eq!(clamp_ev(99.0), 4.0);
        assert_eq!(clamp_ev(0.5), 0.5);
    }

    #[test]
    fn clamp_curve_power_bounds_to_the_slider_range() {
        assert_eq!(clamp_curve_power(0.0), 0.2);
        assert_eq!(clamp_curve_power(9.0), 3.0);
        assert_eq!(clamp_curve_power(1.0), 1.0);
    }

    #[test]
    fn edit_adjust_maps_bare_and_shifted_keys() {
        use edit_manifest::CropDirection::{Bottom, Left, Right, Top};
        // Exposure pair: `-`/`=` coarse, Shift nudge.
        assert_eq!(
            edit_adjust_for("-", false, false),
            Some(EditAdjust::Exposure(-EDIT_STEP_EV))
        );
        assert_eq!(
            edit_adjust_for("=", false, false),
            Some(EditAdjust::Exposure(EDIT_STEP_EV))
        );
        assert_eq!(
            edit_adjust_for("-", false, true),
            Some(EditAdjust::Exposure(-EDIT_NUDGE_EV))
        );
        assert_eq!(
            edit_adjust_for("=", false, true),
            Some(EditAdjust::Exposure(EDIT_NUDGE_EV))
        );
        // Contrast pair: `[`/`]` coarse, Shift nudge.
        assert_eq!(
            edit_adjust_for("[", false, false),
            Some(EditAdjust::Contrast(-EDIT_STEP_CURVE))
        );
        assert_eq!(
            edit_adjust_for("]", false, false),
            Some(EditAdjust::Contrast(EDIT_STEP_CURVE))
        );
        assert_eq!(
            edit_adjust_for("[", false, true),
            Some(EditAdjust::Contrast(-EDIT_NUDGE_CURVE))
        );
        assert_eq!(
            edit_adjust_for("]", false, true),
            Some(EditAdjust::Contrast(EDIT_NUDGE_CURVE))
        );
        // Rolloff pair: `;`/`'` coarse, Shift nudge.
        assert_eq!(
            edit_adjust_for(";", false, false),
            Some(EditAdjust::Rolloff(-EDIT_STEP_CURVE))
        );
        assert_eq!(
            edit_adjust_for("'", false, false),
            Some(EditAdjust::Rolloff(EDIT_STEP_CURVE))
        );
        assert_eq!(
            edit_adjust_for(";", false, true),
            Some(EditAdjust::Rolloff(-EDIT_NUDGE_CURVE))
        );
        assert_eq!(
            edit_adjust_for("'", false, true),
            Some(EditAdjust::Rolloff(EDIT_NUDGE_CURVE))
        );
        // Shadows pair: `,`/`.` coarse, Shift nudge.
        assert_eq!(
            edit_adjust_for(",", false, false),
            Some(EditAdjust::Shadows(-EDIT_STEP_CURVE))
        );
        assert_eq!(
            edit_adjust_for(".", false, false),
            Some(EditAdjust::Shadows(EDIT_STEP_CURVE))
        );
        assert_eq!(
            edit_adjust_for(",", false, true),
            Some(EditAdjust::Shadows(-EDIT_NUDGE_CURVE))
        );
        assert_eq!(
            edit_adjust_for(".", false, true),
            Some(EditAdjust::Shadows(EDIT_NUDGE_CURVE))
        );
        // Crop edges: `h`/`j`/`k`/`l` = Left/Bottom/Top/Right. A bare key trims
        // more (+step); Alt trims less (−step); Shift(+Alt) nudges by 1px.
        assert_eq!(
            edit_adjust_for("h", false, false),
            Some(EditAdjust::Crop {
                direction: Left,
                delta: CROP_STEP_PX
            })
        );
        assert_eq!(
            edit_adjust_for("j", false, false),
            Some(EditAdjust::Crop {
                direction: Bottom,
                delta: CROP_STEP_PX
            })
        );
        assert_eq!(
            edit_adjust_for("k", false, false),
            Some(EditAdjust::Crop {
                direction: Top,
                delta: CROP_STEP_PX
            })
        );
        assert_eq!(
            edit_adjust_for("l", false, false),
            Some(EditAdjust::Crop {
                direction: Right,
                delta: CROP_STEP_PX
            })
        );
        assert_eq!(
            edit_adjust_for("h", true, false),
            Some(EditAdjust::Crop {
                direction: Left,
                delta: -CROP_STEP_PX
            })
        );
        assert_eq!(
            edit_adjust_for("h", false, true),
            Some(EditAdjust::Crop {
                direction: Left,
                delta: CROP_NUDGE_PX
            })
        );
        assert_eq!(
            edit_adjust_for("h", true, true),
            Some(EditAdjust::Crop {
                direction: Left,
                delta: -CROP_NUDGE_PX
            })
        );
    }

    #[test]
    fn edit_adjust_ignores_unbound_keys() {
        assert_eq!(edit_adjust_for("a", false, false), None);
        assert_eq!(edit_adjust_for(" ", false, false), None);
        assert_eq!(edit_adjust_for("p", false, true), None);
        assert_eq!(edit_adjust_for("_", false, true), None);
    }

    #[test]
    fn plain_click_selects_a_single_frame() {
        let tiles = vec![tile("a"), tile("b"), tile("c")];
        let ord: Vec<&Tile> = tiles.iter().collect();
        let (set, anchor) = apply_frame_click(HashSet::new(), "b", false, false, None, &ord);
        let mut v: Vec<_> = set.into_iter().collect();
        v.sort();
        assert_eq!(v, vec!["b".to_string()]);
        assert_eq!(anchor.as_deref(), Some("b"));
    }

    #[test]
    fn ctrl_click_toggles_membership() {
        let tiles = vec![tile("a"), tile("b"), tile("c")];
        let ord: Vec<&Tile> = tiles.iter().collect();
        let start: HashSet<String> = ["a", "b"].into_iter().map(str::to_owned).collect();
        // Toggle "b" off.
        let (set, anchor) = apply_frame_click(start.clone(), "b", true, false, Some("a"), &ord);
        assert_eq!(set.len(), 1);
        assert!(set.contains("a"));
        assert!(!set.contains("b"));
        assert_eq!(anchor.as_deref(), Some("a"));
        // Toggle "c" on.
        let (set, _) = apply_frame_click(start, "c", true, false, Some("a"), &ord);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn shift_click_selects_range_anchor_to_clicked() {
        let tiles = vec![tile("a"), tile("b"), tile("c"), tile("d")];
        let ord: Vec<&Tile> = tiles.iter().collect();
        // Anchor "a", click "c" → selects a..=c.
        let (set, anchor) = apply_frame_click(HashSet::new(), "c", false, true, Some("a"), &ord);
        let mut v: Vec<_> = set.into_iter().collect();
        v.sort();
        assert_eq!(v, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(anchor.as_deref(), Some("c"));
        // Reverse range: anchor "d", click "b" → selects b..=d.
        let (set, _) = apply_frame_click(HashSet::new(), "b", false, true, Some("d"), &ord);
        assert_eq!(set.len(), 3);
        assert!(set.contains("b") && set.contains("c") && set.contains("d"));
    }

    #[test]
    fn shift_click_without_anchor_collapses_to_single() {
        let tiles = vec![tile("a"), tile("b"), tile("c")];
        let ord: Vec<&Tile> = tiles.iter().collect();
        let (set, _) = apply_frame_click(HashSet::new(), "b", false, true, None, &ord);
        assert_eq!(set.len(), 1);
        assert!(set.contains("b"));
    }

    use edit_manifest::CropDirection as Dir;
    use edit_manifest::CropMargins as Marg;

    fn assert_aspects(m: Marg, w: u32, h: u32) {
        // (w − left − right) / (h − top − bottom) must equal w / h, non-degenerate.
        let cw = m.cropped_width(w);
        let ch = m.cropped_height(h);
        assert!(
            cw > 0 && ch > 0,
            "crop collapses to a degenerate frame: {m:?}"
        );
        let left = f64::from(m.left);
        let right = f64::from(m.right);
        let top = f64::from(m.top);
        let bottom = f64::from(m.bottom);
        let lhs = f64::from(w) - left - right;
        let rhs = f64::from(h) - top - bottom;
        let lhs_ratio = lhs / rhs;
        let src_ratio = f64::from(w) / f64::from(h);
        // Margins are integer pixels; the perpendicular pair rounds the exact
        // `ar·(parallel sum)` to the nearest pixel, so a sub-percent drift is
        // inherent and acceptable for frames that keep any real size. A genuine
        // aspect violation (or a collapse) moves the ratio far more than this.
        assert!(
            (lhs_ratio - src_ratio).abs() < 0.01,
            "aspect drifted: {lhs_ratio} != {src_ratio} for {m:?}"
        );
    }

    #[test]
    fn crop_bottom_anchors_top_and_preserves_aspect() {
        let m = crop_margins(Dir::Bottom, 100, 400, 300);
        assert_eq!(m.top, 0, "bottom trim anchors the top edge");
        assert_eq!(m.bottom, 100);
        // Horizontal total must equal ar·R = (400/300)·100 ≈ 133.
        assert_eq!(m.left + m.right, 133);
        assert_aspects(m, 400, 300);
    }

    #[test]
    fn crop_top_anchors_bottom_and_preserves_aspect() {
        let m = crop_margins(Dir::Top, 100, 400, 300);
        assert_eq!(m.bottom, 0, "top trim anchors the bottom edge");
        assert_eq!(m.top, 100);
        assert_eq!(m.left + m.right, 133);
        assert_aspects(m, 400, 300);
    }

    #[test]
    fn crop_left_and_right_anchor_opposite_edge() {
        let m = crop_margins(Dir::Left, 100, 400, 300);
        assert_eq!(m.right, 0, "left trim anchors the right edge");
        assert_eq!(m.left, 100);
        assert_eq!(m.top + m.bottom, 75); // (100)/ar = 100·300/400 = 75
        assert_aspects(m, 400, 300);

        let m = crop_margins(Dir::Right, 100, 400, 300);
        assert_eq!(m.left, 0, "right trim anchors the left edge");
        assert_eq!(m.right, 100);
        assert_aspects(m, 400, 300);
    }

    #[test]
    fn crop_zero_amount_is_identity() {
        for dir in [Dir::Top, Dir::Bottom, Dir::Left, Dir::Right] {
            assert_eq!(crop_margins(dir, 0, 400, 300), Marg::default());
        }
    }

    #[test]
    fn crop_amount_clamps_to_valid_extent() {
        // A bottom trim far larger than the frame must clamp: never invert, and
        // never collapse to a zero-area (or overrun) region.
        let m = crop_margins(Dir::Bottom, 10_000, 400, 300);
        assert!(m.cropped_height(300) > 0, "must not collapse the frame");
        assert!(m.cropped_width(400) > 0, "must not overrun the frame");
        assert_eq!(m.top, 0, "bottom trim anchors the top");
        assert!(m.bottom < 300);
        assert!(m.left + m.right < 400);
    }

    #[test]
    fn crop_tiny_sources_do_not_collapse() {
        for dir in [Dir::Top, Dir::Bottom, Dir::Left, Dir::Right] {
            let m = crop_margins(dir, 50, 8, 8);
            assert!(m.cropped_width(8) > 0);
            assert!(m.cropped_height(8) > 0);
            assert_aspects(m, 8, 8);
        }
    }

    #[test]
    fn crop_margins_cropped_dims_are_saturating() {
        let m = Marg {
            top: 5,
            right: 5,
            bottom: 5,
            left: 5,
        };
        assert_eq!(m.cropped_width(12), 2);
        assert_eq!(m.cropped_height(12), 2);
        // Oversized margins clamp to zero rather than wrapping.
        assert_eq!(m.cropped_width(4), 0);
        assert_eq!(m.cropped_height(4), 0);
    }

    #[test]
    fn crop_amount_accumulates_per_edge_and_recenters_perpendicular() {
        // Trim bottom by 100 twice → bottom grows, top stays anchored, left+right
        // re-derive from the total vertical trim to keep aspect.
        let one = apply_crop_amount(Marg::default(), Dir::Bottom, 100, 400, 300);
        assert_eq!(one.top, 0);
        assert_eq!(one.bottom, 100);
        assert_eq!(one.left + one.right, 133);
        assert_aspects(one, 400, 300);

        let two = apply_crop_amount(one, Dir::Bottom, 100, 400, 300);
        assert_eq!(two.top, 0, "bottom trim keeps the top anchored");
        assert_eq!(two.bottom, 200);
        assert!(two.left + two.right >= one.left + one.right);
        assert_aspects(two, 400, 300);
    }

    #[test]
    fn crop_grow_recedes_the_chosen_edge_and_never_goes_negative() {
        let base = apply_crop_amount(Marg::default(), Dir::Bottom, 100, 400, 300);
        // Grow (negative delta) back toward no-crop.
        let grown = apply_crop_amount(base, Dir::Bottom, -100, 400, 300);
        assert_eq!(grown.bottom, 0);
        assert_eq!(grown, Marg::default());
        // Overshooting growth clamps to zero margins.
        let flat = apply_crop_amount(base, Dir::Bottom, -10_000, 400, 300);
        assert_eq!(flat, Marg::default());
    }

    #[test]
    fn crop_switching_axes_recenters_perpendicular_and_keeps_aspect() {
        // Trim bottom then left. The most recent (horizontal) trim becomes the
        // authoritative axis: left is explicit, and the vertical pair re-derives
        // (equal + centered) from left+right — the bottom value set earlier is
        // superseded. Aspect stays locked throughout.
        let bottom = apply_crop_amount(Marg::default(), Dir::Bottom, 100, 400, 300);
        assert_eq!(bottom.top, 0);
        assert_aspects(bottom, 400, 300);

        let switched = apply_crop_amount(bottom, Dir::Left, 80, 400, 300);
        // left was 67 from the bottom trim's perpendicular pair, now +80.
        assert_eq!(switched.left, 147);
        // The vertical pair re-derives from (left + right):
        // round((147 + 66) / (400/300)) = round(159.75) = 160, split 80/80.
        assert_eq!(switched.top, switched.bottom);
        assert_eq!(switched.top + switched.bottom, 160);
        assert_aspects(switched, 400, 300);
        assert!(switched.left > 0);
    }

    #[test]
    fn set_crop_edge_sets_the_edge_and_recenters_perpendicular() {
        // Setting bottom to 100 absolutely is the typed-field equivalent of the
        // keyboard trim: the top stays anchored and left+right re-derive.
        let m = set_crop_edge(Marg::default(), Dir::Bottom, 100, 400, 300);
        assert_eq!(m.top, 0);
        assert_eq!(m.bottom, 100);
        assert_eq!(m.left + m.right, 133);
        assert_aspects(m, 400, 300);
    }

    #[test]
    fn set_crop_edge_left_recenters_the_vertical_pair() {
        let m = set_crop_edge(Marg::default(), Dir::Left, 80, 400, 300);
        assert_eq!(m.right, 0, "left trim anchors the right edge");
        assert_eq!(m.left, 80);
        assert_eq!(m.top + m.bottom, 60); // 80/ar = 80·300/400
        assert_aspects(m, 400, 300);
    }

    #[test]
    fn set_crop_edge_recenters_when_applied_to_a_cropped_frame() {
        // A frame already trimmed on the vertical axis: setting the left edge
        // absolutely makes horizontal the authoritative axis, and the vertical
        // pair re-derives equal + centered from it.
        let base = apply_crop_amount(Marg::default(), Dir::Bottom, 100, 400, 300);
        let m = set_crop_edge(base, Dir::Left, 80, 400, 300);
        assert_eq!(m.left, 80);
        assert_eq!(m.top, m.bottom, "perpendicular pair re-centers");
        assert_aspects(m, 400, 300);
    }

    #[test]
    fn set_crop_edge_clamps_to_valid_extent() {
        // An oversized absolute margin must clamp: never invert or collapse.
        // (Like `crop_amount_clamps_to_valid_extent`, a near-max vertical trim
        // makes the exact aspect inexpressible, so only non-degeneracy is
        // asserted here.)
        let m = set_crop_edge(Marg::default(), Dir::Bottom, 10_000, 400, 300);
        assert!(m.cropped_height(300) > 0, "must not collapse the frame");
        assert!(m.cropped_width(400) > 0, "must not overrun the frame");
        assert_eq!(m.top, 0, "bottom trim anchors the top");
        assert!(m.bottom < 300);
        assert!(m.left + m.right < 400);
    }

    #[test]
    fn set_crop_edge_zeroes_to_identity() {
        for dir in [Dir::Top, Dir::Bottom, Dir::Left, Dir::Right] {
            assert_eq!(
                set_crop_edge(Marg::default(), dir, 0, 400, 300),
                Marg::default()
            );
        }
    }

    #[test]
    fn scale_crop_scales_margins_onto_the_print() {
        // 4000x2000 source printed at 400x200 scales each axis by 1/10.
        let crop = Marg {
            top: 10,
            right: 20,
            bottom: 30,
            left: 40,
        };
        assert_eq!(
            scale_crop(crop, 4000, 2000, 400, 200),
            Marg {
                top: 1,
                right: 2,
                bottom: 3,
                left: 4
            }
        );
    }

    #[test]
    fn scale_crop_scales_a_rotated_print_from_display_source_dims() {
        // A Rotate90 sensor of masked dims (2000 wide, 400 tall) displays as
        // (400 wide, 2000 tall): the caller resolves those display dims before
        // scaling, so the print's horizontal axis (400) maps to the source
        // horizontal and the vertical margins compress by 400→200 / 2000.
        let crop = Marg {
            top: 10,
            right: 20,
            bottom: 30,
            left: 40,
        };
        let (disp_w, disp_h) = display_source_dims(2000, 400, rawloader::Orientation::Rotate90);
        assert_eq!((disp_w, disp_h), (400, 2000));
        assert_eq!(
            scale_crop(crop, disp_w, disp_h, 400, 200),
            Marg {
                top: 1,
                right: 20,
                bottom: 3,
                left: 40
            }
        );
    }

    #[test]
    fn display_source_dims_swaps_axes_for_rotated_sensors() {
        use rawloader::Orientation;
        for o in [
            Orientation::Rotate90,
            Orientation::Rotate270,
            Orientation::Transpose,
            Orientation::Transverse,
        ] {
            assert_eq!(display_source_dims(6000, 4000, o), (4000, 6000));
        }
        for o in [
            Orientation::Normal,
            Orientation::Unknown,
            Orientation::HorizontalFlip,
            Orientation::VerticalFlip,
            Orientation::Rotate180,
        ] {
            assert_eq!(display_source_dims(6000, 4000, o), (6000, 4000));
        }
    }

    #[test]
    fn crop_bake_resolves_display_dims_before_scaling_to_the_thumb() {
        // The thumbnail bake must interpret the stored crop in the same
        // full-resolution display-source frame the detail view authors it in.
        // A 6000x4000 landscape sensor at THUMB_SIZE (384 long edge) prints
        // 384x256; a 100 source-px left+right trim ≈ 1.67% of the frame must
        // remove ≈ 6.4 print px per side — NOT ~1 px, which is what happens
        // when the crop is instead treated as overview-texture pixels and the
        // bake scales it against the full sensor.
        let crop = Marg {
            top: 0,
            right: 100,
            bottom: 0,
            left: 100,
        };
        let (disp_w, disp_h) = display_source_dims(6000, 4000, rawloader::Orientation::Normal);
        let scaled = scale_crop(crop, disp_w, disp_h, 384, 256);
        assert_eq!(
            (scaled.left, scaled.right, scaled.top, scaled.bottom),
            (6, 6, 0, 0)
        );
    }

    #[test]
    fn crop_bake_matches_a_portrait_rotated_frame() {
        // Rotate90: crop left/right margins in the portrait display are
        // horizontal in source vertical terms — resolved display dims handle
        // the swap, so the same 100-px side crop and a 100-px top trim land at
        // the same print fractions as the landscape case.
        let crop = Marg {
            top: 100,
            right: 100,
            bottom: 0,
            left: 0,
        };
        let (disp_w, disp_h) = display_source_dims(6000, 4000, rawloader::Orientation::Rotate90);
        assert_eq!((disp_w, disp_h), (4000, 6000));
        // Portrait print of a 6000-long sensor at THUMB_SIZE: 256x384.
        let scaled = scale_crop(crop, disp_w, disp_h, 256, 384);
        assert_eq!(
            (scaled.left, scaled.right, scaled.top, scaled.bottom),
            (0, 6, 6, 0)
        );
    }

    #[test]
    fn crop_rgba_slices_the_frame_to_the_margins() {
        // A 4x4 RGBA grid; each pixel gray = row-major pixel index.
        let mut rgba = Vec::new();
        for p in 0..16u8 {
            rgba.extend_from_slice(&[p, p, p, 255]);
        }
        let crop = Marg {
            top: 1,
            right: 1,
            bottom: 1,
            left: 1,
        };
        let (out, w, h) = crop_rgba(rgba, 4, 4, crop);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[0..4], &[5, 5, 5, 255], "top-left = source row1 col1");
        assert_eq!(&out[4..8], &[6, 6, 6, 255]);
        assert_eq!(&out[8..12], &[9, 9, 9, 255]);
        assert_eq!(&out[12..16], &[10, 10, 10, 255]);
    }

    #[test]
    fn crop_rgba_is_identity_with_no_margins() {
        let rgba: Vec<u8> = (0..4u8).flat_map(|p| [p, p, p, 255]).collect();
        let (out, w, h) = crop_rgba(rgba.clone(), 2, 2, Marg::default());
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, rgba);
    }

    #[test]
    fn crop_rgba_rejects_an_overrunning_crop_unchanged() {
        let rgba: Vec<u8> = (0..4u8).flat_map(|p| [p, p, p, 255]).collect();
        let crop = Marg {
            top: 0,
            right: 0,
            bottom: 0,
            left: 100,
        };
        let (out, w, h) = crop_rgba(rgba.clone(), 2, 2, crop);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, rgba);
    }

    #[test]
    fn rotate_quarters_quarter_turns_a_square_grid_ccw() {
        // A 2x2 RGBA grid; each pixel gray = its position (0..3).
        let rgba: Vec<u8> = (0..4u8).flat_map(|p| [p, p, p, 255]).collect();
        // One CCW turn keeps a square's dims but moves the original TOP edge
        // to the display's LEFT column (shader `rot == 1`: (1-v, u) puts the
        // display top-left at texture top-right, and the display left column
        // walks the original top row right→left as it goes down).
        let (out, w, h) = rotate_quarters(rgba.clone(), 2, 2, 1);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[0..4], &[1, 1, 1, 255], "top-left = source top-right");
        assert_eq!(
            &out[8..12],
            &[0, 0, 0, 255],
            "bottom-left = source top-left"
        );
        // Two turns: pure 180° reversal, dims unchanged.
        let (out, w, h) = rotate_quarters(rgba.clone(), 2, 2, 2);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[0..4], &[3, 3, 3, 255]);
        assert_eq!(&out[12..16], &[0, 0, 0, 255]);
        // Three turns (one CW): the original TOP edge lands on the display's
        // RIGHT column, so the display top-left samples the source BOTTOM-left.
        let (out, w, h) = rotate_quarters(rgba.clone(), 2, 2, 3);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[0..4], &[2, 2, 2, 255], "top-left = source bottom-left");
        // Four turns wrap to the identity (mirrors `& 3` in the shader).
        let (out, w, h) = rotate_quarters(rgba.clone(), 2, 2, 4);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, rgba);
    }

    #[test]
    fn rotate_quarters_swaps_dims_on_an_odd_turn_and_composes_crop() {
        // A 3-wide × 2-tall grid; pixel value = row-major index.
        let mut rgba = Vec::new();
        for p in 0..6u8 {
            rgba.extend_from_slice(&[p, p, p, 255]);
        }
        let (out, w, h) = rotate_quarters(rgba.clone(), 3, 2, 1);
        assert_eq!((w, h), (2, 3), "odd turn swaps the print dims");
        // Output grid (2 wide × 3 tall): source 0 1 2 / 3 4 5 rotates CCW to
        // 2 5 / 1 4 / 0 3. Mapping: source (x,y) → output (ox=y, oy=w−1−x).
        assert_eq!(
            &out[0..4],
            &[2, 2, 2, 255],
            "output top-left = source top-right"
        );
        assert_eq!(
            &out[4..8],
            &[5, 5, 5, 255],
            "output top-right = source bottom-right"
        );
        assert_eq!(
            &out[8..12],
            &[1, 1, 1, 255],
            "output middle-left = source mid top"
        );
        assert_eq!(
            &out[16..20],
            &[0, 0, 0, 255],
            "output bottom-left = source top-left"
        );
        // Compose with a crop: quarter-turn the result of cropping a 3×3 frame
        // to its 1-px margins (a 1×1 center pixel), like the bake's
        // crop-then-rotate ordering.
        let mut rgba = Vec::new();
        for p in 0..9u8 {
            rgba.extend_from_slice(&[p, p, p, 255]);
        }
        let (cropped, cw, ch) = crop_rgba(
            rgba.clone(),
            3,
            3,
            Marg {
                top: 1,
                right: 1,
                bottom: 1,
                left: 1,
            },
        );
        assert_eq!((cw, ch), (1, 1));
        let (out, w, h) = rotate_quarters(cropped, cw, ch, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[0..4], &[4, 4, 4, 255], "crop center pixel survives");
    }

    #[test]
    fn export_name_replaces_the_source_extension() {
        assert_eq!(
            export_name("img_0001.cr2", ExportFormat::Jpeg).to_string_lossy(),
            "img_0001.jpg"
        );
        assert_eq!(
            export_name("img_0001.cr2", ExportFormat::Png).to_string_lossy(),
            "img_0001.png"
        );
        assert_eq!(
            export_name("img_0001.nef", ExportFormat::Jpeg).to_string_lossy(),
            "img_0001.jpg"
        );
        // No extension in the name: the format's extension is simply appended.
        assert_eq!(
            export_name("IMG_0001", ExportFormat::Jpeg).to_string_lossy(),
            "IMG_0001.jpg"
        );
        // A dotfile: the hidden leading dot is part of the stem.
        assert_eq!(
            export_name(".hidden.cr2", ExportFormat::Jpeg).to_string_lossy(),
            ".hidden.jpg"
        );
    }

    #[test]
    fn roll_hash_is_deterministic_6_hex() {
        let a = std::path::Path::new("/home/user/Films/2024 May Costa Rica");
        let b = std::path::Path::new("/home/user/Films/2024 May Portugal");
        let (ha, ha2, hb) = (roll_hash(a), roll_hash(a), roll_hash(b));
        // Deterministic, exact width, lowercase hex regardless of path content.
        assert_eq!(ha, ha2);
        assert_eq!(ha.len(), 6);
        assert!(ha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        // Different rolls must (almost always) differ.
        assert_ne!(ha, hb);
    }

    #[test]
    fn dated_export_name_builds_the_scheme() {
        // `YYYYMMDD-<roll-hash>-<frame>.<ext>`; frame is the 1-based full-roll
        // position padded to two digits; the source stem is deliberately gone.
        let name = |index, hash| {
            dated_export_name(ExportFormat::Jpeg, "2024-05-09", index, hash)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name(0, "1a2b3c"), "20240509-1a2b3c-01.jpg");
        assert_eq!(name(9, "1a2b3c"), "20240509-1a2b3c-10.jpg");
        assert_eq!(name(99, "1a2b3c"), "20240509-1a2b3c-100.jpg");
        assert_eq!(
            dated_export_name(ExportFormat::Png, "2024-05-09", 1, "1a2b3c")
                .unwrap()
                .to_string_lossy(),
            "20240509-1a2b3c-02.png"
        );
    }

    #[test]
    fn dated_names_group_by_roll_when_sorted() {
        // Hash-first ordering means a same-day multi-roll folder sorts into
        // roll groups, each roll's frames in capture order — the property that
        // put the hash before the frame number.
        let name = |hash: &str, index: usize| {
            dated_export_name(ExportFormat::Jpeg, "2024-05-09", index, hash)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let mut names = vec![
            name("bbbbbb", 1),
            name("aaaaaa", 1),
            name("aaaaaa", 0),
            name("bbbbbb", 0),
        ];
        names.sort();
        assert_eq!(
            names,
            vec![
                "20240509-aaaaaa-01.jpg",
                "20240509-aaaaaa-02.jpg",
                "20240509-bbbbbb-01.jpg",
                "20240509-bbbbbb-02.jpg",
            ]
        );
    }

    #[test]
    fn dated_export_name_rejects_a_malformed_start_date() {
        assert!(dated_export_name(ExportFormat::Jpeg, "2024-5-09", 0, "1a2b3c").is_none());
        assert!(dated_export_name(ExportFormat::Jpeg, "2024/05/09", 0, "1a2b3c").is_none());
        assert!(dated_export_name(ExportFormat::Jpeg, "", 0, "1a2b3c").is_none());
    }

    #[test]
    fn export_presets_define_the_expected_defaults() {
        // Cloud: JPEG q90 at native resolution, tagged 300 dpi for print.
        let cloud = ExportOptions::for_preset(ExportPreset::Cloud);
        assert_eq!(cloud.format, ExportFormat::Jpeg);
        assert_eq!(cloud.quality, 90);
        assert_eq!(cloud.size, ExportSize::Original);
        assert_eq!(cloud.ppi, 300);
        // Master: lossless 16-bit PNG at native resolution, tagged 300 dpi.
        let master = ExportOptions::for_preset(ExportPreset::Master);
        assert_eq!(master.format, ExportFormat::Png);
        assert_eq!(master.size, ExportSize::Original);
        assert_eq!(master.ppi, 300);
        // Web: JPEG q82 downscaled to a 2048px long edge, tagged 72 dpi.
        let web = ExportOptions::for_preset(ExportPreset::Web);
        assert_eq!(web.format, ExportFormat::Jpeg);
        assert_eq!(web.quality, 82);
        assert_eq!(web.size, ExportSize::LongEdge2048);
        assert_eq!(web.ppi, 72);
        // Overwriting is off by default (the dialog checkbox defaults to "no").
        assert!(!cloud.overwrite && !master.overwrite && !web.overwrite);
    }

    #[test]
    fn export_artifacts_are_identified_by_extension() {
        // Outputs of this app's exporter must never be mistaken for negatives.
        assert!(is_export_artifact("img_0001.jpg"));
        assert!(is_export_artifact("IMG_0002.JPEG"));
        assert!(is_export_artifact("scan.png"));
        assert!(!is_export_artifact("img_0001.cr2"));
        assert!(!is_export_artifact("IMG_0002.nef"));
        assert!(!is_export_artifact("scan.dng"));
        assert!(!is_export_artifact(".hidden"));
        assert!(!is_export_artifact("no_extension"));
    }

    #[test]
    fn options_for_choice_resolves_the_dropdown_keys() {
        // Each file-picker format choice key maps to its preset's options.
        let cloud = options_for_choice("jpeg-90");
        assert_eq!(cloud.format, ExportFormat::Jpeg);
        assert_eq!(cloud.quality, 90);
        let web = options_for_choice("jpeg-82");
        assert_eq!(web.quality, 82);
        assert_eq!(web.size, ExportSize::LongEdge2048);
        let master = options_for_choice("png-16");
        assert_eq!(master.format, ExportFormat::Png);
        // An unknown key (or a backend that dropped the choice) falls back to
        // the first preset.
        let fallback = options_for_choice("nonsense");
        assert_eq!(fallback.format, ExportFormat::Jpeg);
        assert_eq!(fallback.quality, 90);
    }

    #[test]
    fn png_export_writes_grayscale_16bit_round_trip() {
        // 2x2 solid mid-gray frame. 0.5 is exactly representable at 16-bit
        // (32768), so decoding the written PNG and checking that value pins
        // both the color type and the sample byte order: the raw `png` crate
        // writes big-endian samples as-is, and the old little-endian buffer
        // swapped every sample (0x8000 was stored and read back as 0x0080 = 128).
        let mono = vec![0.5, 0.5, 0.5, 0.5];
        let dest = std::env::temp_dir().join(format!(
            "curvectrl_png_smoke_{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        let result = export_png(
            mono,
            2,
            2,
            Marg::default(),
            0,
            &dest,
            None,
            ExportOptions {
                format: ExportFormat::Png,
                quality: 100,
                size: ExportSize::Original,
                ppi: 72,
                overwrite: false,
            },
        );
        let bytes = std::fs::read(&dest).unwrap();
        std::fs::remove_file(&dest).ok();
        assert_eq!(result, Ok(()));

        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!(image.color(), image::ColorType::L16);
        assert_eq!(
            image.as_luma16().unwrap().get_pixel(0, 0).0[0],
            32768,
            "mid-gray round-trips at 16-bit in native byte order"
        );

        // The 72 dpi request must produce a real pHYs chunk (2835 px/m). `read_info`
        // walks metadata up to the first IDAT (pHYs sits there); the bare
        // `read_header_info` stops at IHDR with the chunk fields still `None`.
        let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
        let reader = decoder.read_info().expect("png header parses");
        let dims = reader
            .info()
            .pixel_dims
            .expect("a 72 dpi request tags a pHYs chunk");
        assert_eq!(dims.xppu, 2835);
        assert_eq!(dims.yppu, 2835);
        assert_eq!(dims.unit, png::Unit::Meter);

        // The export always tags the samples as sRGB, so color-managed
        // consumers interpret them deterministically rather than guessing.
        assert_eq!(
            reader.info().srgb,
            Some(png::SrgbRenderingIntent::Perceptual)
        );
    }

    #[test]
    fn jpeg_export_tags_the_jfif_density() {
        // 2x2 solid mid-gray through `export_jpeg` with the Web preset's 72 dpi.
        // The JFIF APP0 header should read: "JFIF\0" + version 1.2 + unit 01
        // (dots per inch) + Xdensity 72 (00 48 BE) + Ydensity 72.
        let mono = vec![0.5; 4];
        let dest = std::env::temp_dir().join(format!(
            "curvectrl_jpeg_dpi_{}.jpg",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        let result = export_jpeg(
            mono,
            2,
            2,
            Marg::default(),
            0,
            &dest,
            None,
            ExportOptions {
                format: ExportFormat::Jpeg,
                quality: 82,
                size: ExportSize::LongEdge2048,
                ppi: 72,
                overwrite: false,
            },
        );
        let bytes = std::fs::read(&dest).unwrap();
        std::fs::remove_file(&dest).ok();
        assert_eq!(result, Ok(()));

        let expected = [
            b'J', b'F', b'I', b'F', 0x00, 0x01, 0x02, 0x01, 0x00, 0x48, 0x00, 0x48,
        ];
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "JFIF header must tag 72 dpi"
        );
    }

    #[test]
    fn jpeg_encoder_smoke_writes_quality_90_soi() {
        // Grayscale, matching the export path's Luma encode.
        let mut gray = Vec::with_capacity(16 * 16);
        for y in 0..16u8 {
            for x in 0..16u8 {
                gray.push(x * 16 ^ y * 16);
            }
        }

        // A unique temp path per run so parallel test threads never collide.
        let mut dest = std::env::temp_dir();
        dest.push(format!(
            "curvectrl_jpeg_smoke_{}.jpg",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));

        let encoder = jpeg_encoder::Encoder::new_file(&dest, 90).unwrap();
        encoder
            .encode(&gray, 16, 16, jpeg_encoder::ColorType::Luma)
            .unwrap();

        let bytes = std::fs::read(&dest).unwrap();
        std::fs::remove_file(&dest).ok();

        // A JPEG stream always starts with the SOI marker FF D8 FF.
        assert!(bytes.len() > 4, "encoded stream has payload");
        assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF]);
    }

    /// The GPU side of the bake-parity tests: a Rust simulation of `exposure.wgsl`
    /// `shade()` where the tone remap comes from the same 2048×1 R16Float tone
    /// LUT the shader texture-samples (decoded to f32 + linear interpolation,
    /// via [`shader::sample_tone_lut_f32`]). The CPU bakes call [`tone_model`]
    /// exactly; the only approximation anywhere is the LUT itself, so this
    /// diffing bounds that approximation rather than a duplicated expression.
    #[allow(clippy::too_many_arguments)]
    fn gpu_fragment(
        mono: f32,
        exposure: f32,
        inv: bool,
        inv_base: f32,
        inv_d_max: f32,
        inv_gamma: f32,
        tone_lut: &[f32],
    ) -> f32 {
        let mono_linear = mono;
        let v = if inv {
            let transmission = mono_linear * exposure;
            let clamped = transmission.clamp(1e-6, inv_base);
            let density = -(clamped / inv_base).ln() / 10.0_f32.ln();
            let position = (density / inv_d_max).clamp(0.0, 1.0);
            let positive = position.powf(inv_gamma);
            shader::sample_tone_lut_f32(tone_lut, positive)
        } else {
            let remapped = shader::sample_tone_lut_f32(tone_lut, mono_linear);
            (remapped * exposure).clamp(0.0, 1.0)
        };
        srgb_encode(v)
    }

    /// Decode a `build_tone_lut` half-float byte buffer back to f32, matching
    /// what the GPU's R16Float sampler would hand back (modulo the sampler's
    /// own linear interpolation).
    fn half_lut_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(2)
            .map(|c| shader::half_to_f32(u16::from_le_bytes([c[0], c[1]])))
            .collect()
    }

    /// Deterministic LCG so the differential tests are reproducible run-to-run.
    #[allow(clippy::cast_possible_truncation)]
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Top 23 mantissa bits of [1.0, 2.0), mapped to [0.0, 1.0).
            let bits = ((self.0 >> 40) as u32 & 0x007F_FFFF) | 0x3F80_0000;
            f32::from_bits(bits) - 1.0
        }
    }

    #[test]
    fn cpu_bake_matches_gpu_lut_simulation_on_random_frames() {
        // Randomize every input (frame values, EV, curve powers, preset, base
        // mode) and assert `bake_tone` (grid/export, exact `tone_model`) == the
        // GPU detail path (the same tone model delivered through a half-float
        // LUT + linear interpolation, simulated via `gpu_fragment`) — the
        // structural guarantee that the three render paths cannot drift beyond
        // the accepted LUT approximation. `pivots_for` is called on the
        // untouched frame on both sides, so they share identical pivots.
        let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
        for case in 0..64 {
            let ev = rng.next() * 7.0 - 3.0;
            let tone = edit_manifest::ToneEdit {
                exposure_ev: ev,
                curve_contrast: rng.next() * 1.5 + 0.5,
                curve_rolloff: rng.next() * 1.5 + 0.5,
                curve_shadows: rng.next() * 1.5 + 0.5,
            };
            let preset = if rng.next() < 0.5 {
                FilmPreset::None
            } else {
                FilmPreset::Hp5Plus
            };
            let base_config = BaseConfig {
                calibrated: (rng.next() < 0.4).then(|| rng.next() * 0.5 + 0.3),
                auto: rng.next() < 0.3,
            };
            let mut mono: Vec<f32> = (0..256).map(|_| rng.next()).collect();
            // Stress the clamps with hard extremes.
            mono[0] = 0.0;
            mono[1] = 1.0;
            mono[64] = 1e-6;

            let mut baked = mono.clone();
            bake_tone(&mut baked, tone, preset, base_config);

            let (stock_and_base, pivots) = pivots_for(&mono, tone, preset, base_config);
            let (shadow, mid, white) = pivots;
            // The GPU side delivers the same tone model through the LUT.
            let lut = half_lut_bytes_to_f32(&shader::build_tone_lut(
                tone.curve_contrast,
                tone.curve_rolloff,
                tone.curve_shadows,
                shadow,
                mid,
                white,
            ));
            let (exposure, inv, inv_base, inv_d_max, inv_gamma) = match stock_and_base {
                Some((stock, base)) => (
                    shader::sensor_gain(tone.exposure_ev, true),
                    true,
                    base,
                    stock.d_max,
                    stock.gamma,
                ),
                None => (
                    shader::sensor_gain(tone.exposure_ev, false),
                    false,
                    1.0,
                    1.0,
                    1.0,
                ),
            };

            for (i, &sample) in mono.iter().enumerate() {
                let reference =
                    gpu_fragment(sample, exposure, inv, inv_base, inv_d_max, inv_gamma, &lut);
                let baked_value = baked[i];
                assert!(
                    (baked_value - reference).abs() <= 5e-4,
                    "case {case} pixel {i} ({sample}): bake {baked_value} vs gpu {reference}"
                );
            }
        }
    }
}
