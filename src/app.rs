// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::Config;
use crate::detail_area::DetailArea;
use crate::edit_manifest::{self, RollManifest};
use crate::shader;
use crate::film::{ACTIVE_STOCK, MIN_PLAUSIBLE_BASE, invert_gray, measure_base};
use crate::fl;
use cosmic::Application;
use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::keyboard;
use cosmic::iced::widget::scrollable::Viewport;
use cosmic::iced::widget::{Grid, MouseArea, Stack, grid};
use cosmic::iced::{ContentFit, Length, Point, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, icon, image::Handle, menu};
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
    /// highlight, the `Ctrl+Space` metadata drawer, and Enter/arrow navigation.
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
    /// True while the detail view's editing drawer is hidden for a full-screen
    /// preview (toggled by spacebar). Only meaningful while `selected` is set.
    fullscreen: bool,
    /// LRU of decoded detail overviews keyed by (roll dir, file name), so
    /// returning to a recently-viewed frame doesn't re-decode the RAW. Survives
    /// roll switches and detail close; eviction is global (see
    /// [`DETAIL_CACHE_CAPACITY`]). Only overview (2048px) buffers are stored.
    detail_cache: LruCache<(PathBuf, String), DetailMono>,
    /// (roll dir, file name) handed to the bounded neighbor preload decodes,
    /// so a frame already being preloaded (or already cached) is never spawned
    /// twice. Independent of the single critical detail slot.
    detail_preload_inflight: Vec<(PathBuf, String)>,
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

/// The selectable cell on the library page: either the always-first Add Roll
/// tile or a real roll. Modeling both with a single type makes the selection,
/// highlight, keyboard navigation, and the open action uniform across the grid
/// — the add tile is selected and Entered exactly like a roll card.
#[derive(Debug, Clone, PartialEq)]
enum LibrarySelection {
    /// The Add Roll tile (grid cell 0). Enter / double-click opens the folder
    /// picker; it has no directory or metadata of its own.
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
    /// Decoded cover thumbnail state.
    pub thumb: Thumb,
}

/// A file entry displayed as a tile on the open roll's frame grid.
struct Tile {
    /// File name.
    name: String,
    /// Decoded thumbnail state.
    thumb: Thumb,
}

/// A decoded detail-view overview: the linear pre-sRGB mono buffer plus its
/// geometry, as delivered by [`decode_raw_detail`]. Exactly what an
/// [`shader::DetailProgram`] needs to (re)build without re-decoding
/// the RAW. Cached by the detail LRU keyed on (roll dir, file name).
#[derive(Debug, Clone)]
struct DetailMono {
    mono: Vec<f32>,
    width: u32,
    height: u32,
    /// The sensor's true long edge AFTER cropping but BEFORE the downscale, so
    /// a served cache entry can decide whether the overview was already native.
    src_long_edge: u32,
}

/// A fixed-capacity least-recently-used map keyed by (roll dir, file name).
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
    /// A hi-res decode for the detail view finished, returning the linear
    /// pre-sRGB mono buffer for the GPU shader.
    DetailReady(String, Result<(Vec<f32>, u32, u32, u32), ()>),
    /// A neighbor preload decode finished. Unlike [`Message::DetailReady`] this
    /// only lands into the detail LRU cache; it never becomes the active shader.
    DetailPreloaded(PathBuf, String, Result<(Vec<f32>, u32, u32, u32), ()>),
    /// The startup roll scan finished.
    RollsLoaded(Vec<Roll>),
    /// A single roll was scanned after being added; push it into the library.
    RollInfoLoaded(Roll),
    /// A roll cover decode finished.
    CoverReady(PathBuf, Result<Handle, ()>),
    /// The folder picker returned a roll directory to add.
    RollAdded(PathBuf),
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
    /// Toggle the full-screen preview: hide the editing drawer (spacebar) so
    /// the detail view fills the window; toggling again (or pressing Escape)
    /// restores the drawer.
    ToggleFullscreen,
    /// Wheel-scroll zoom in the detail view; payload is the change in zoom
    /// units (log2 of the scale ratio), positive = zoom in, negative = out.
    DetailZoom(f32),
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
    /// Quit the application, persisting any open edits first.
    Quit,
    ToggleContextPage(ContextPage),
    /// Toggle the current page's context drawer (Ctrl+Space). Since the
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
            about,
            key_binds: HashMap::from([
                (
                    menu::KeyBind {
                        modifiers: vec![menu::key_bind::Modifier::Ctrl],
                        key: keyboard::Key::Character(" ".into()),
                    },
                    MenuAction::RollInfo,
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
            roll: RollManifest::default(),
            detail_shader: None,
            detail_inflight: None,
            detail_native_queued: false,
            detail_thumb_opacity: 1.0,
            detail_last_frame: None,
            detail_thumb: None,
            detail_zoom: 1.0,
            detail_pan: (0.0, 0.0),
            detail_panning: false,
            detail_cursor: None,
            curve_contrast: 1.0,
            curve_rolloff: 1.0,
            curve_shadows: 1.0,
            exposure_ev: 0.0,
            crop: edit_manifest::CropMargins::default(),
            crop_drafts: CropDrafts::from_margins(edit_manifest::CropMargins::default()),
            rotation: 0,
            show_crop_mask: false,
            reset_exposure_ev: 0.0,
            reset_curve_contrast: 1.0,
            reset_curve_rolloff: 1.0,
            reset_curve_shadows: 1.0,
            reset_crop: edit_manifest::CropMargins::default(),
            reset_rotation: 0,
            clipboard: None,
            next_image_id: 0,
            fullscreen: false,
            detail_cache: LruCache::new(DETAIL_CACHE_CAPACITY),
            detail_preload_inflight: Vec::new(),
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
        // Remove roll is only actionable when a roll is selected in the
        // library; otherwise it shows disabled on the menu.
        let roll_selected = matches!(&self.library_selection, Some(LibrarySelection::Roll(_)));
        let remove_roll = if roll_selected {
            menu::Item::Button(fl!("menu-remove-roll"), None, MenuAction::RemoveRoll)
        } else {
            menu::Item::ButtonDisabled(fl!("menu-remove-roll"), None, MenuAction::RemoveRoll)
        };

        let file_menu = menu::Tree::with_children(
            menu::root(fl!("menu-file")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![
                    menu::Item::Button(fl!("menu-add-roll"), None, MenuAction::AddRoll),
                    remove_roll,
                    menu::Item::Divider,
                    menu::Item::Button(fl!("menu-quit"), None, MenuAction::Quit),
                ],
            ),
        );

        let edit_menu = menu::Tree::with_children(
            menu::root(fl!("menu-edit")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![
                    menu::Item::Button(fl!("menu-select-all"), None, MenuAction::SelectAll),
                    menu::Item::Divider,
                    menu::Item::Button(fl!("menu-copy-edits"), None, MenuAction::CopyEdits),
                    menu::Item::Button(fl!("menu-paste-edits"), None, MenuAction::PasteEdits),
                    menu::Item::Divider,
                    // Show editing panel is only actionable while a detail view
                    // (and its editing drawer) is open.
                    if self.selected.is_some() {
                        menu::Item::Button(fl!("menu-show-editing"), None, MenuAction::ShowEditing)
                    } else {
                        menu::Item::ButtonDisabled(
                            fl!("menu-show-editing"),
                            None,
                            MenuAction::ShowEditing,
                        )
                    },
                ],
            ),
        );

        let view_menu = menu::Tree::with_children(
            menu::root(fl!("menu-view")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![
                    menu::Item::Button(fl!("about"), None, MenuAction::About),
                    menu::Item::Button(fl!("menu-roll-info"), None, MenuAction::RollInfo),
                ],
            ),
        );

        // Menu popups must be backed by real Wayland surfaces (and know which
        // window to anchor to), so the bar forwards surface actions to the
        // cosmic runtime — matching how cosmic-files wires its menu bar.
        let menu_bar = menu::bar(vec![file_menu, edit_menu, view_menu])
            .window_id_maybe(self.core().main_window_id())
            .on_surface_action(Message::Surface);

        vec![menu_bar.into()]
    }

    /// Elements to pack at the end of the header bar.
    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        // The editing drawer is no longer toggled from a header button — it
        // opens automatically with the detail view (see `open_frame`) and is
        // hidden/revealed by the spacebar full-screen preview — so the header
        // end packs only the search control.

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

        vec![search]
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
                self.selected.as_ref()?;

                Some(
                    context_drawer::context_drawer(
                        editing_panel(self),
                        Message::ToggleContextPage(ContextPage::Editing),
                    )
                    .title(fl!("editing-title")),
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
                        roll_info_panel(roll),
                        Message::ToggleContextPage(ContextPage::RollInfo),
                    )
                    .title(fl!("roll-info-title")),
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

        widget::column::with_capacity(1)
            .push(content)
            .spacing(cosmic::theme::spacing().space_s)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
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
                // The spacebar carries no Named variant in this iced fork, so
                // it arrives as a character — matched by payload. A bare
                // space (no modifiers) toggles the full-screen preview; the
                // handler no-ops when no detail view is open.
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if !modifiers.control() && character == " " => Some(Message::ToggleFullscreen),
                // Ctrl+Space toggles the active page's context drawer (a
                // modifying wildcard would otherwise catch the bare-space
                // above). The page choice — editing vs roll info — is resolved
                // in the update handler, since this closure cannot capture
                // app state.
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character),
                    modifiers,
                    ..
                } if modifiers.control() && character == " " => Some(Message::ToggleContext),
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
                    && edit_adjust_for(character.as_str(), modifiers.alt(), modifiers.shift()).is_some() =>
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
                // On the library page, Escape first closes an open context
                // drawer (roll info / about); only falls through when there is
                // nothing open to close.
                if self.active.is_none() {
                    if self.selected.is_none() && self.core.window.show_context {
                        self.core_mut().set_show_context(false);
                    }
                    return Task::none();
                }
                if self.selected.is_some() {
                    // First Escape in full-screen preview only exits full-screen
                    // (restoring the editing drawer); a second Escape then closes
                    // the detail view back to the grid.
                    if self.fullscreen {
                        self.fullscreen = false;
                        self.context_page = ContextPage::Editing;
                        self.core.window.show_context = true;
                        return Task::none();
                    }
                    // Close the detail view; the frame highlight survives so
                    // the grid still shows where you were.
                    self.selected = None;
                    self.clear_detail();
                    // Without a selection the editing drawer has nothing to
                    // show; close it so it does not linger empty.
                    self.close_editing();
                } else if self.frame_selected.is_some() {
                    // No detail open: Escape first clears the grid highlight...
                    self.frame_selected = None;
                } else {
                    // ...then backs out of the roll entirely, resetting every
                    // detail- and roll-page field so nothing from the closed
                    // roll leaks into the library (edits were already flushed
                    // by `persist_roll` at the top of this arm).
                    self.active = None;
                    self.selected = None;
                    self.frame_selected = None;
                    self.selected_frames.clear();
                    self.selection_anchor = None;
                    self.grid_viewport = None;
                    self.tiles = Vec::new();
                    self.thumb_inflight.clear();
                    self.detail_inflight = None;
                    self.detail_preload_inflight.clear();
                    // The RAM manifest is dropped with the roll; re-opened
                    // rolls re-load it (see `RollOpened`). The overview LRU is
                    // deliberately kept: it survives roll switches by design.
                    self.roll = edit_manifest::RollManifest::default();
                    self.clear_detail();
                    self.close_editing();
                }
                Task::none()
            }

            Message::DetailReady(name, result) => self.handle_detail_ready(&name, result),

            Message::DetailPreloaded(dir, name, result) => {
                self.handle_detail_preloaded(&dir, &name, result)
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
                let (new_zoom, new_pan) =
                    apply_detail_zoom(self.detail_zoom, self.detail_pan, self.detail_cursor, delta);
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
                self.commit_edit()
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
                    }
                }
                self.decode_covers()
            }

            Message::RollInfoLoaded(roll) => {
                if self.rolls.iter().any(|existing| existing.dir == roll.dir) {
                    return Task::none();
                }
                self.rolls.push(roll);
                self.rolls.sort_by(|a, b| a.name.cmp(&b.name));
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

            Message::RollAdded(dir) => {
                // Persist the new roll; the library page owns the roll list.
                if self.active.is_none() && !self.rolls.iter().any(|roll| roll.dir == dir) {
                    self.config.rolls.push(dir.to_string_lossy().into_owned());
                    self.persist_config();
                }
                // Scan the chosen directory for its cover and display name.
                cosmic::task::future(async move { Message::RollInfoLoaded(load_roll(dir).await) })
            }

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
                        return self.scroll_selection_into_view("frames-grid", target, len, cols);
                    }
                    return Task::none();
                }

                // Library grid: move the selection over every visible cell —
                // the always-first Add Roll tile, then the filtered rolls — and
                // reveal it out of the viewport. The add tile is always a valid
                // destination, so there is no empty-match early return.
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
                Task::none()
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
                    })
                    .collect();

                self.decode_next()
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

            Message::ToggleFullscreen => {
                // Full-screen preview only applies to a detail view; on the
                // library page (or the bare grid) spacebar does nothing.
                if self.selected.is_none() {
                    return Task::none();
                }
                self.fullscreen = !self.fullscreen;
                if self.fullscreen {
                    // Hide the editing drawer so the preview fills the window.
                    self.core_mut().set_show_context(false);
                } else {
                    // Back to the editing drawer mode.
                    self.context_page = ContextPage::Editing;
                    self.core.window.show_context = true;
                }
                Task::none()
            }

            Message::ToggleContextPage(context_page) => {
                // The metadata drawer needs a library *roll* selection; without
                // one (no selection, or the Add Roll tile) the toggle is a no-op
                // so it never opens an empty drawer (the menu item stays enabled,
                // like the editing toggle).
                if context_page == ContextPage::RollInfo
                    && (self.active.is_some()
                        || !matches!(self.library_selection, Some(LibrarySelection::Roll(_))))
                {
                    return Task::none();
                }
                if self.context_page == context_page {
                    // Close the context drawer if the toggled context page is the same.
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    // Open the context drawer to display the requested context page.
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
                Task::none()
            }

            Message::ToggleContext => {
                // The default context-drawer toggle (Ctrl+Space) opens the
                // editing panel while a detail view is open, or the roll-info
                // drawer on the library page. Delegate to the page-specific
                // toggles so their guards (e.g. a roll selection for roll info)
                // still apply.
                let page = if self.selected.is_some() {
                    ContextPage::Editing
                } else {
                    ContextPage::RollInfo
                };
                self.update(Message::ToggleContextPage(page))
            }

            Message::UpdateConfig(config) => {
                self.config = config;
                Task::none()
            }

            Message::Ignore => Task::none(),

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
        self.clear_detail();
        self.close_editing();
        if self.context_page == ContextPage::RollInfo {
            self.core_mut().set_show_context(false);
        }
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
        // up with Ctrl+Space or the Edit → Show editing panel menu when they
        // want the controls. A same-session paging to a neighbour file likewise
        // leaves whatever context state is current untouched.

        Task::batch([
            self.decode_detail_next(),
            self.preload_detail_neighbors(&name),
        ])
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
            pending
                .iter()
                .map(|(n, ..)| n.as_str())
                .collect::<Vec<_>>()
        ));

        self.thumb_inflight
            .extend(pending.iter().map(|(name, ..)| name.clone()));

        Task::batch(pending.into_iter().map(move |(name, tone, crop, rotation)| {
            cosmic::task::future(decode_thumbnail(dir.clone(), name, tone, crop, rotation))
        }))
    }

    /// Spawns decoding of up to [`MAX_CONCURRENT_THUMBS`] roll-cover
    /// thumbnails, mirroring the frame chain's bounds and de-duplication.
    fn decode_covers(&mut self) -> Task<cosmic::Action<Message>> {
        let capacity = MAX_CONCURRENT_THUMBS.saturating_sub(self.cover_inflight.len());
        if capacity == 0 {
            return Task::none();
        }

        let pending: Vec<(PathBuf, String)> = self
            .rolls
            .iter()
            .filter(|roll| matches!(roll.thumb, Thumb::Loading))
            .filter(|roll| !self.cover_inflight.iter().any(|dir| dir == &roll.dir))
            .filter_map(|roll| roll.cover.clone().map(|name| (roll.dir.clone(), name)))
            .take(capacity)
            .collect();

        if pending.is_empty() {
            return Task::none();
        }

        self.cover_inflight
            .extend(pending.iter().map(|(dir, _)| dir.clone()));

        Task::batch(
            pending
                .into_iter()
                .map(|(dir, name)| cosmic::task::future(decode_cover(dir, name))),
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
            let Some(roll) = self.rolls.iter_mut().find(|roll| {
                roll.dir == *active && roll.cover.as_deref() == Some(name.as_str())
            }) else {
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
                .get(&(dir, name.clone()))
                .map(|c| (c.mono.clone(), c.width, c.height, c.src_long_edge))
        {
            let (mono, width, height, src_long_edge) = cached;
            detail_trace(format_args!(
                "cache hit: {name} {width}x{height} (src {src_long_edge})"
            ));
            self.install_detail_shader(mono, width, height, src_long_edge);
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

        cosmic::task::future(decode_detail(dir, name, cap))
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
        ));
        if let Some(shader) = &mut self.detail_shader {
            shader.set_view(self.detail_zoom, self.detail_pan);
            shader.set_curve(self.curve_contrast, self.curve_rolloff, self.curve_shadows);
            shader.set_crop(self.crop);
            shader.set_rotation(self.rotation);
        }
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
        result: Result<(Vec<f32>, u32, u32, u32), ()>,
    ) -> Task<cosmic::Action<Message>> {
        self.detail_preload_inflight
            .retain(|(pending_dir, pending_name)| pending_dir != dir || pending_name != name);

        if let Ok((mono, width, height, src_long_edge)) = result {
            let _evicted = self.detail_cache.insert(
                (dir.clone(), name.to_string()),
                DetailMono {
                    mono,
                    width,
                    height,
                    src_long_edge,
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

        let pending: Vec<String> = neighbors
            .into_iter()
            .filter(|n| {
                let key = (dir.clone(), n.clone());
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

        Task::batch(
            pending
                .into_iter()
                .map(|n| cosmic::task::future(preload_detail(dir.clone(), n))),
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
        self.exposure_ev = 0.0;
        self.crop = edit_manifest::CropMargins::default();
        self.crop_drafts = CropDrafts::from_margins(self.crop);
        self.rotation = 0;
        self.show_crop_mask = false;
        // The reset snapshot mirrors the live edit values' lifecycle: reset
        // to identity on close; the next `ThumbnailActivated` re-syncs it.
        self.reset_exposure_ev = 0.0;
        self.reset_curve_contrast = 1.0;
        self.reset_curve_rolloff = 1.0;
        self.reset_curve_shadows = 1.0;
        self.reset_crop = edit_manifest::CropMargins::default();
        self.reset_rotation = 0;
        // The full-screen preview dies with the detail view it belongs to.
        self.fullscreen = false;
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

    /// Closes the editing context drawer when it has nothing left to show.
    fn close_editing(&mut self) {
        if self.context_page == ContextPage::Editing && self.core.window.show_context {
            self.core_mut().set_show_context(false);
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
            }
        }
        Task::none()
    }

    /// Handle the completion of a hi-res detail decode, applying the result
    /// only if it matches the current selection and re-pumping if superseded.
    fn handle_detail_ready(
        &mut self,
        name: &str,
        result: Result<(Vec<f32>, u32, u32, u32), ()>,
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
                Ok((mono, width, height, src_long_edge)) => {
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
                            (dir.clone(), name.to_string()),
                            DetailMono {
                                mono: mono.clone(),
                                width,
                                height,
                                src_long_edge,
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
/// sorted non-dot file), and the count of frame files — with nothing decoded
/// yet.
async fn load_roll(dir: PathBuf) -> Roll {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| dir.to_string_lossy().into_owned(), str::to_string);
    let (cover, frame_count) = roll_cover_and_count(&dir).await;
    Roll {
        dir,
        name,
        cover,
        frame_count,
        thumb: Thumb::Loading,
    }
}

/// Loads roll metadata for each configured roll directory, de-duplicated and
/// sorted by display name.
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

/// Scans a roll directory once: returns the first regular non-dot file name in
/// sorted order — the roll's cover, if it has any negatives yet — alongside the
/// count of frame files (both `None`/0 for a missing or empty directory). A
/// single pass covers the cover thumbnail and the metadata-drawer frame count.
async fn roll_cover_and_count(dir: &Path) -> (Option<String>, usize) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return (None, 0);
    };

    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|ty| ty.is_file())
            && let Some(name) = entry.file_name().into_string().ok()
            && !name.starts_with('.')
        {
            files.push(name);
        }
    }

    let count = files.len();
    files.sort();
    (files.into_iter().next(), count)
}

/// Scans a roll directory for its frame files and returns their sorted names.
/// Dotfiles (including the edit manifest) are never shown as tiles.
async fn load_files_in(dir: PathBuf) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };

    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|ty| ty.is_file())
            && let Some(name) = entry.file_name().into_string().ok()
            && !name.starts_with('.')
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
/// then the rolls whose name matches the query. Since the add tile is always
/// present, the returned slice is never empty.
fn library_cells<'a>(rolls: &'a [Roll], query: &str) -> Vec<LibraryCell<'a>> {
    std::iter::once(LibraryCell::AddRoll)
        .chain(
            filtered_rolls(rolls, query)
                .into_iter()
                .map(LibraryCell::Roll),
        )
        .collect()
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

/// Opens the system folder picker, and on success emits [`Message::RollAdded`]
/// for the chosen directory (a cancel or portal failure is a no-op). Shared by
/// the double-click handler and Enter on a selected Add Roll tile.
fn open_roll_picker() -> Task<cosmic::Action<Message>> {
    cosmic::task::future(async {
        match cosmic::dialog::file_chooser::open::Dialog::new()
            .open_folder()
            .await
        {
            Ok(response) => response
                .url()
                .to_file_path()
                .map_or(Message::Ignore, Message::RollAdded),
            // Cancelled (or a portal failure) is a no-op.
            Err(_) => Message::Ignore,
        }
    })
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
    // The hint overlays only when no real roll remains (the add tile is always
    // present, so `cells` can never be empty on its own).
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

    if !empty {
        return body.into();
    }

    // Nothing matches: keep the add tile visible and hint at the result over
    // the space the grid leaves empty.
    let hint = widget::container(widget::text(if app.rolls.is_empty() {
        fl!("no-rolls")
    } else {
        fl!("no-rolls-found")
    }))
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

    let title = widget::text::heading(fl!("editing-title"));

    if app.detail_shader.is_none() {
        return widget::column::with_capacity(1)
            .push(title)
            .spacing(space_s)
            .width(Length::Fill)
            .into();
    }

    let label = widget::text(fl!("exposure-label"));
    let slider = widget::slider(-3.0..=3.0, app.exposure_ev, Message::ExposureChanged)
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
    let crop_top = crop_margin_field(fl!("crop-top"), app.crop_drafts.get(edit_manifest::CropDirection::Top), edit_manifest::CropDirection::Top);
    let crop_right = crop_margin_field(fl!("crop-right"), app.crop_drafts.get(edit_manifest::CropDirection::Right), edit_manifest::CropDirection::Right);
    let crop_bottom = crop_margin_field(fl!("crop-bottom"), app.crop_drafts.get(edit_manifest::CropDirection::Bottom), edit_manifest::CropDirection::Bottom);
    let crop_left = crop_margin_field(fl!("crop-left"), app.crop_drafts.get(edit_manifest::CropDirection::Left), edit_manifest::CropDirection::Left);
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
    let rotate_button =
        widget::button::standard(fl!("rotate-ccw")).on_press(Message::RotateCcw);
    let reset_all = widget::button::standard(fl!("reset-all")).on_press(Message::ResetAll);
    let reset_crop = widget::button::standard(fl!("reset-crop")).on_press(Message::ResetCrop);

    widget::column::with_capacity(20)
        .push(title)
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
        .push(widget::row::with_capacity(2)
            .push(reset_all)
            .push(reset_crop)
            .spacing(space_s))
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

/// Renders the roll metadata drawer: the roll name as heading, its full path,
/// frame count, and cover file. The drawer pane supplies the width/padding.
fn roll_info_panel(roll: &Roll) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let remove = widget::button::destructive(fl!("remove-roll"))
        .on_press(Message::RemoveRoll(roll.dir.clone()));

    widget::column::with_capacity(7)
        .push(widget::text::heading(&roll.name))
        .push(widget::divider::horizontal::default())
        .push(meta_row(
            fl!("roll-path-label"),
            roll.dir.display().to_string(),
        ))
        .push(meta_row(
            fl!("roll-frames-label"),
            roll.frame_count.to_string(),
        ))
        .push(meta_row(
            fl!("roll-cover-label"),
            roll.cover.clone().unwrap_or_else(|| fl!("roll-no-cover")),
        ))
        .push(widget::divider::horizontal::default())
        .push(remove)
        .spacing(space_s)
        .width(Length::Fill)
        .into()
}

/// A label-over-value row for the roll metadata drawer.
fn meta_row(label: String, value: String) -> Element<'static, Message> {
    widget::column::with_capacity(2)
        .push(widget::text(label))
        .push(widget::text(value))
        .spacing(cosmic::theme::spacing().space_xs)
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

    // Title + frame count sit in a block padded 12px (`space_xs`) away from the
    // card's edges; its top padding is also the gap below the full-bleed cover.
    let info: Element<'_, Message> = widget::container(
        widget::column::with_capacity(2)
            .push(widget::text(&roll.name))
            .push(widget::text::caption(fl!(
                "roll-frames",
                count = roll.frame_count
            )))
            .spacing(space_xs)
            .align_x(Horizontal::Center),
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
    let space_s = cosmic::theme::spacing().space_s;

    // The image keeps its square cell with ContentFit::Contain (no cropping);
    // placeholders stay centered in the same sheet.
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

    // The frame name sits below the image; its horizontal padding also keeps
    // the text clear of the selection ring.
    let info: Element<'_, Message> = widget::container(widget::text(&tile.name))
        .width(Length::Fill)
        .padding(space_s)
        .into();

    let card = widget::column::with_capacity(2)
        .push(content)
        .push(info)
        .spacing(0);

    let card: Element<'_, Message> = MouseArea::new(card)
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
                .on_press(Message::DetailPanPress)
                .on_move(Message::DetailPanMove)
                .on_release(Message::DetailPanRelease),
            )
            .push(widget::text(name))
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
/// `[1.0, MAX_DETAIL_ZOOM]`. Zooming all the way back out to contain fit
/// re-centers the image. Returns the new `(zoom, pan)`.
fn apply_detail_zoom(
    zoom: f32,
    pan: (f32, f32),
    cursor: Option<Point>,
    delta: f32,
) -> (f32, (f32, f32)) {
    let new_zoom = (zoom + delta).clamp(1.0, MAX_DETAIL_ZOOM);
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

/// Clamps an exposure adjustment in EV to the slider's range (−3.0..=+3.0).
fn clamp_ev(ev: f32) -> f32 {
    ev.clamp(-3.0, 3.0)
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
    let sum_limit_vertical = ((((f64::from(width) - f64::from(MIN_LEFT)) / ar).floor()
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
    edit_manifest::CropMargins { top: t, right: r, bottom: b, left: l }
}

/// Clamps a single edge margin after applying `delta_px`, keeping the edge in
/// `[0, sum_limit − opposite]` so a parallel-axis trim never overshoots the
/// frame (the two parallel edges sum to at most `sum_limit`).
fn clamp_axis(edge: u32, opposite: u32, delta_px: i32, sum_limit: u32) -> u32 {
    let max = sum_limit.saturating_sub(opposite);
    let raw = i64::from(edge) + i64::from(delta_px);
    raw.clamp(0, i64::from(max))
        .try_into()
        .unwrap_or(u32::MAX)
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
        let step = if shift {
            CROP_NUDGE_PX
        } else {
            CROP_STEP_PX
        };
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
) -> Message {
    let result = decode_raw(dir, name.clone(), move |image| {
        convert_thumbnail(image, THUMB_SIZE, tone, crop, rotation)
    })
    .await;

    Message::ThumbReady(name, result)
}

/// Decodes a roll's cover file into a thumbnail message, baking in the cover
/// file's stored exposure, tone curve and display rotation from the roll's
/// manifest so the roll tile preview parallels the edited frame (grid == detail
/// for covers too). A roll with no manifest (or an unedited cover) falls back
/// to identity.
async fn decode_cover(dir: PathBuf, name: String) -> Message {
    let manifest = edit_manifest::load_roll_manifest(&dir);
    let tone = manifest.tone(&name);
    let crop = manifest.crop(&name);
    let rotation = manifest.rotation(&name) & 3;
    let result = decode_raw(dir.clone(), name, move |image| {
        convert_thumbnail(image, THUMB_SIZE, tone, crop, rotation)
    })
    .await;

    Message::CoverReady(dir, result)
}

/// Decodes a RAW frame from the open roll into a hi-res message for
/// the detail view, returning the oriented linear mono data that the GPU
/// shader uploads and applies exposure to.  `max_edge` caps the long edge in
/// pixels; the overview level uses [`HI_RES_SIZE`], the native level-up
/// [`MAX_TEXTURE_EDGE`].
async fn decode_detail(dir: PathBuf, name: String, max_edge: u32) -> Message {
    let result = decode_raw_detail(dir, name.clone(), max_edge).await;
    Message::DetailReady(name, result)
}

/// Decodes a neighbor frame's overview for the preload cache. Unlike
/// [`decode_detail`] this only populates the LRU — it never becomes the active
/// detail shader — so it always decodes at the fixed overview cap.
async fn preload_detail(dir: PathBuf, name: String) -> Message {
    let result = decode_raw_detail(dir.clone(), name.clone(), HI_RES_SIZE).await;
    Message::DetailPreloaded(dir, name, result)
}

/// Runs a RAW decode plus mono reconstruction on a blocking worker thread,
/// returning linear `mono` (post-downscale, post-unsharp) oriented to display
/// upright.  The GPU shader applies exposure and sRGB encoding per frame.
/// `max_edge` is the downscale target for the long edge before unsharp.
///
/// The last tuple field is the sensor's true long edge AFTER cropping but
/// BEFORE the downscale — i.e. the real native long edge the overview was
/// scaled down from (< `max_edge` means the overview is already full-res).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn decode_raw_detail(
    dir: PathBuf,
    name: String,
    max_edge: u32,
) -> Result<(Vec<f32>, u32, u32, u32), ()> {
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

        let (mut mono, width, height) = if image.cpp >= 3 {
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

        let base = measure_base(&mono)
            .filter(|measured| *measured >= MIN_PLAUSIBLE_BASE)
            .unwrap_or(ACTIVE_STOCK.base);
        invert_gray(&mut mono, &ACTIVE_STOCK, base);

        let (mono, width, height) = resize_area(&mono, width as u32, height as u32, max_edge, 1);

        let mut mono = mono;
        unsharp_mask(&mut mono, width as usize, height as usize);

        let (oriented, width, height) = orient_mono(&mono, width, height, image.orientation);

        Ok((oriented, width, height, src_long_edge))
    })
    .await
    .unwrap_or(Err(()))
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
fn display_source_dims(
    cw: u32,
    ch: u32,
    orientation: rawloader::Orientation,
) -> (u32, u32) {
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
) -> Result<Handle, ()> {
    // One fused pass: normalize, discard masked borders, and phase-preserve
    // downscale straight from the sensor samples into a small linear negative.
    let (mut mono, width, height) = downsample_thumbnail(image, max_size as u32).ok_or(())?;

    // Anchor the black point on the frame's clearest film, then invert the
    // negative in density space.
    let base = measure_base(&mono)
        .filter(|measured| *measured >= MIN_PLAUSIBLE_BASE)
        .unwrap_or(ACTIVE_STOCK.base);
    invert_gray(&mut mono, &ACTIVE_STOCK, base);

    // Restore edge punch lost to the heavy downscale, before tone encoding so
    // overshoot stays out of the perceptually amplified display range.
    unsharp_mask(&mut mono, width as usize, height as usize);

    // Bake the stored tone curve (contrast + highlight rolloff + shadows) in
    // linear light, using the same anchor measurement + remap the detail
    // shader uses, applied BEFORE the exposure gain to mirror the shader's
    // ordering exactly (curve first, then `2^EV`, then clamp/sRGB). At
    // the identity curve this is a no-op, so untouched renders stay
    // byte-identical to pre-curve ones.
    let (shadow, mid, white) = shader::tone_anchors(&mono);
    shader::apply_curve(
        &mut mono,
        tone.curve_contrast,
        tone.curve_rolloff,
        tone.curve_shadows,
        shadow,
        mid,
        white,
    );

    // Bake the stored exposure in linear light, matching the detail shader's
    // `mono_linear * 2^EV` (applied after the curve remap), so grid tile and
    // detail view agree.
    apply_exposure(&mut mono, tone.exposure_ev);

    for value in &mut mono {
        *value = srgb_encode(*value);
    }

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    AddRoll,
    RemoveRoll,
    Quit,
    SelectAll,
    CopyEdits,
    PasteEdits,
    ShowEditing,
    About,
    RollInfo,
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
            MenuAction::ShowEditing => Message::ToggleContextPage(ContextPage::Editing),
            MenuAction::About => Message::ToggleContextPage(ContextPage::About),
            MenuAction::RollInfo => Message::ToggleContextPage(ContextPage::RollInfo),
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
            thumb: Thumb::Loading,
        }
    }

    fn tile(name: &str) -> Tile {
        Tile {
            name: name.to_string(),
            thumb: Thumb::Loading,
        }
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
    }

    #[test]
    fn library_cells_never_empty_even_with_no_matches() {
        // No rolls at all: still the add tile.
        assert_eq!(library_cells(&[], "").len(), 1);
        // A query matching nothing still yields the add tile.
        let rolls = vec![roll("/a", "Alpha")];
        assert_eq!(library_cells(&rolls, "zzz").len(), 1);
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
    fn apply_detail_zoom_clamps_at_both_ends() {
        let (zoom, _) = apply_detail_zoom(1.0, (0.0, 0.0), None, -1.0);
        assert_eq!(zoom, 1.0);
        let (zoom, _) = apply_detail_zoom(MAX_DETAIL_ZOOM, (0.0, 0.0), None, 9.0);
        assert_eq!(zoom, MAX_DETAIL_ZOOM);
    }

    #[test]
    fn apply_detail_zoom_returns_unchanged_pan_without_cursor() {
        let (zoom, pan) = apply_detail_zoom(2.0, (13.0, -7.0), None, 0.5);
        assert!((zoom - 2.5).abs() < 1e-6);
        assert_eq!(pan, (13.0, -7.0));
    }

    #[test]
    fn apply_detail_zoom_recenters_when_back_to_contain_fit() {
        // Zooming all the way out must give the centered contain view.
        let (zoom, pan) = apply_detail_zoom(3.0, (50.0, -30.0), Some(Point::new(0.0, 0.0)), -2.0);
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
    fn clamp_ev_bounds_to_the_slider_range() {
        assert_eq!(clamp_ev(-99.0), -3.0);
        assert_eq!(clamp_ev(99.0), 3.0);
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
            Some(EditAdjust::Crop { direction: Left, delta: CROP_STEP_PX })
        );
        assert_eq!(
            edit_adjust_for("j", false, false),
            Some(EditAdjust::Crop { direction: Bottom, delta: CROP_STEP_PX })
        );
        assert_eq!(
            edit_adjust_for("k", false, false),
            Some(EditAdjust::Crop { direction: Top, delta: CROP_STEP_PX })
        );
        assert_eq!(
            edit_adjust_for("l", false, false),
            Some(EditAdjust::Crop { direction: Right, delta: CROP_STEP_PX })
        );
        assert_eq!(
            edit_adjust_for("h", true, false),
            Some(EditAdjust::Crop { direction: Left, delta: -CROP_STEP_PX })
        );
        assert_eq!(
            edit_adjust_for("h", false, true),
            Some(EditAdjust::Crop { direction: Left, delta: CROP_NUDGE_PX })
        );
        assert_eq!(
            edit_adjust_for("h", true, true),
            Some(EditAdjust::Crop { direction: Left, delta: -CROP_NUDGE_PX })
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
        assert!(cw > 0 && ch > 0, "crop collapses to a degenerate frame: {m:?}");
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
            assert_eq!(set_crop_edge(Marg::default(), dir, 0, 400, 300), Marg::default());
        }
    }

    #[test]
    fn scale_crop_scales_margins_onto_the_print() {
        // 4000x2000 source printed at 400x200 scales each axis by 1/10.
        let crop = Marg { top: 10, right: 20, bottom: 30, left: 40 };
        assert_eq!(
            scale_crop(crop, 4000, 2000, 400, 200),
            Marg { top: 1, right: 2, bottom: 3, left: 4 }
        );
    }

    #[test]
    fn scale_crop_scales_a_rotated_print_from_display_source_dims() {
        // A Rotate90 sensor of masked dims (2000 wide, 400 tall) displays as
        // (400 wide, 2000 tall): the caller resolves those display dims before
        // scaling, so the print's horizontal axis (400) maps to the source
        // horizontal and the vertical margins compress by 400→200 / 2000.
        let crop = Marg { top: 10, right: 20, bottom: 30, left: 40 };
        let (disp_w, disp_h) = display_source_dims(2000, 400, rawloader::Orientation::Rotate90);
        assert_eq!((disp_w, disp_h), (400, 2000));
        assert_eq!(
            scale_crop(crop, disp_w, disp_h, 400, 200),
            Marg { top: 1, right: 20, bottom: 3, left: 40 }
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
        let crop = Marg { top: 0, right: 100, bottom: 0, left: 100 };
        let (disp_w, disp_h) =
            display_source_dims(6000, 4000, rawloader::Orientation::Normal);
        let scaled = scale_crop(crop, disp_w, disp_h, 384, 256);
        assert_eq!((scaled.left, scaled.right, scaled.top, scaled.bottom), (6, 6, 0, 0));
    }

    #[test]
    fn crop_bake_matches_a_portrait_rotated_frame() {
        // Rotate90: crop left/right margins in the portrait display are
        // horizontal in source vertical terms — resolved display dims handle
        // the swap, so the same 100-px side crop and a 100-px top trim land at
        // the same print fractions as the landscape case.
        let crop = Marg { top: 100, right: 100, bottom: 0, left: 0 };
        let (disp_w, disp_h) =
            display_source_dims(6000, 4000, rawloader::Orientation::Rotate90);
        assert_eq!((disp_w, disp_h), (4000, 6000));
        // Portrait print of a 6000-long sensor at THUMB_SIZE: 256x384.
        let scaled = scale_crop(crop, disp_w, disp_h, 256, 384);
        assert_eq!((scaled.left, scaled.right, scaled.top, scaled.bottom), (0, 6, 6, 0));
    }

    #[test]
    fn crop_rgba_slices_the_frame_to_the_margins() {
        // A 4x4 RGBA grid; each pixel gray = row-major pixel index.
        let mut rgba = Vec::new();
        for p in 0..16u8 {
            rgba.extend_from_slice(&[p, p, p, 255]);
        }
        let crop = Marg { top: 1, right: 1, bottom: 1, left: 1 };
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
        let crop = Marg { top: 0, right: 0, bottom: 0, left: 100 };
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
        assert_eq!(&out[8..12], &[0, 0, 0, 255], "bottom-left = source top-left");
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
        assert_eq!(&out[0..4], &[2, 2, 2, 255], "output top-left = source top-right");
        assert_eq!(&out[4..8], &[5, 5, 5, 255], "output top-right = source bottom-right");
        assert_eq!(&out[8..12], &[1, 1, 1, 255], "output middle-left = source mid top");
        assert_eq!(&out[16..20], &[0, 0, 0, 255], "output bottom-left = source top-left");
        // Compose with a crop: quarter-turn the result of cropping a 3×3 frame
        // to its 1-px margins (a 1×1 center pixel), like the bake's
        // crop-then-rotate ordering.
        let mut rgba = Vec::new();
        for p in 0..9u8 {
            rgba.extend_from_slice(&[p, p, p, 255]);
        }
        let (cropped, cw, ch) =
            crop_rgba(rgba.clone(), 3, 3, Marg { top: 1, right: 1, bottom: 1, left: 1 });
        assert_eq!((cw, ch), (1, 1));
        let (out, w, h) = rotate_quarters(cropped, cw, ch, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[0..4], &[4, 4, 4, 255], "crop center pixel survives");
    }
}
