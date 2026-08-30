// SPDX-License-Identifier: MPL-2.0

use crate::config::Config;
use crate::detail_area::DetailArea;
use crate::edit_manifest::{self, RollManifest};
use crate::exposure_shader;
use crate::film::{ACTIVE_STOCK, MIN_PLAUSIBLE_BASE, invert_gray, measure_base};
use crate::fl;
use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::Application;
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::keyboard;
use cosmic::iced::widget::{Grid, MouseArea, Stack, grid};
use cosmic::iced::{ContentFit, Length, Point, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, icon, image::Handle, menu};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const APP_ICON: &[u8] = include_bytes!("../resources/icons/hicolor/scalable/apps/icon.svg");

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
    /// Names handed to the bounded in-flight roll-cover decodes, so re-baked
    /// roll tiles never double-spawn against the startup chain (memory bound).
    cover_inflight: Vec<PathBuf>,
    /// Roll search term: filters roll names on the library page and frame
    /// names inside an open roll.
    query: String,
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
    detail_shader: Option<exposure_shader::ExposureProgram>,
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
    /// Exposure compensation in EV (−3.00 to +3.00).
    exposure_ev: f32,
    /// Monotonic counter incremented each time a new detail decode finishes;
    /// stamped into [`ExposureProgram::image_id`] so the GPU pipeline
    /// recognises a new image and rebuilds its texture.
    next_image_id: u64,
}

/// A film roll: a user-chosen directory of negatives, listed as a cover tile
/// on the library page. Clicking a roll drills into its frame grid.
#[derive(Debug, Clone)]
pub struct Roll {
    /// Absolute directory holding this roll's negatives.
    pub dir: PathBuf,
    /// Display name (the directory's final component).
    pub name: String,
    /// The roll's cover file name (first sorted non-dot file), if any.
    pub cover: Option<String>,
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
    /// A roll tile was clicked — drill into its frame grid.
    RollActivated(PathBuf),
    /// The frame scan for an opened roll finished.
    RollOpened(PathBuf, Vec<String>),
    /// Return from the frame grid to the library (roll grid).
    BackToRolls,
    /// The search field changed.
    SearchChanged(String),
    LaunchUrl(String),
    ThumbReady(String, Result<Handle, ()>),
    /// A thumbnail was double-clicked, opening it in the detail view.
    ThumbnailActivated(String),
    /// Animation tick driving the hi-res crossfade.
    DetailFadeTick,
    /// The user moved the exposure slider.
    ExposureChanged(f32),
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
    /// The live tone curve changed: new contrast and rolloff powers. Applies
    /// to the shader as a uniform-only remap (non-persisted).
    CurveChanged(f32, f32),
    /// Reset the tone curve back to the identity (contrast = rolloff = 1).
    CurveReset,
    /// Flush the in-memory roll edits to the manifest file on disk.
    EditSave,
    /// Consume an input event without acting on it, blocking the grid
    /// beneath the detail view's input surface.
    Ignore,
    ToggleContextPage(ContextPage),
    UpdateConfig(Config),
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
    const APP_ID: &'static str = "dev.mmurphy.Test";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
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
            key_binds: HashMap::new(),
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
            cover_inflight: Vec::new(),
            query: String::new(),
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
            exposure_ev: 0.0,
            next_image_id: 0,
        };

        // Seed the POC's original single roll directory so a fresh config
        // shows the same frames the app always has, without any user action.
        if app.config.rolls.is_empty()
            && let Some(default) = default_library_dir()
        {
            app.config.rolls.push(default.to_string_lossy().into_owned());
        }

        // Set the window title and scan the configured roll directories.
        let rolls = app.config.rolls.clone();
        let command = Task::batch([
            app.update_title(),
            cosmic::task::future(async { Message::RollsLoaded(load_rolls(rolls).await) }),
        ]);

        (app, command)
    }

    /// Elements to pack at the start of the header bar.
    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let menu_bar = menu::bar(vec![menu::Tree::with_children(
            menu::root(fl!("view")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![menu::Item::Button(fl!("about"), None, MenuAction::About)],
            ),
        )]);

        vec![menu_bar.into()]
    }

    /// Elements to pack at the end of the header bar.
    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        // Toggles the editing panel drawer for the active detail view.
        let active = self.context_page == ContextPage::Editing
            && self.core.window.show_context
            && self.selected.is_some();

        vec![widget::button::icon(icon::from_name("edit-symbolic"))
            .selected(active)
            .tooltip(fl!("editing-toggle"))
            .on_press_maybe(
                self.selected
                    .is_some()
                    .then_some(Message::ToggleContextPage(ContextPage::Editing)),
            )
            .into()]
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
        }
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// Application events will be processed through the view. Any messages emitted by
    /// events received by widgets will be passed to the update method.
    fn view(&self) -> Element<'_, Self::Message> {
        // A fixed controls row (Add roll / back + search) stays above the
        // scrollable content: the library page of roll covers, or an open
        // roll's frame grid with its detail overlay.
        let controls = controls_row(self);
        let content: Element<_> = match self.active.as_deref() {
            Some(_) => frames_view(self),
            None => library_view(self),
        };

        widget::column::with_capacity(2)
            .push(controls)
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
            // that captures the key first.
            keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                } => Some(Message::DetailClosed),
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
                if self.selected.is_some() {
                    self.selected = None;
                    self.clear_detail();
                    // Without a selection the editing drawer has nothing to
                    // show; close it so it does not linger empty.
                    self.close_editing();
                } else if self.active.is_some() {
                    // No detail open: Escape backs out of the roll entirely.
                    self.active = None;
                    self.tiles = Vec::new();
                    self.thumb_inflight.clear();
                    self.clear_detail();
                    self.close_editing();
                }
                Task::none()
            }

            Message::DetailReady(name, result) => self.handle_detail_ready(&name, result),

            Message::ThumbnailActivated(name) => {
                if self.selected.as_deref() != Some(name.as_str()) {
                    // Persist unsaved tweaks to the outgoing file first.
                    self.persist_roll();
                    // Read the stored edit before the decode builds the
                    // shader, which consumes `self.exposure_ev`.
                    let stored_ev = self.roll.exposure_ev(name.as_str());
                    self.selected = Some(name);
                    self.clear_detail();
                    self.exposure_ev = stored_ev;
                }

                self.decode_detail_next()
            }

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
                self.exposure_ev = ev;
                // RAM-only until an edit flush point; the shader stays live.
                if let Some(selected) = &self.selected {
                    self.roll.set_exposure(selected, ev);
                }
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_exposure(ev);
                }
                Task::none()
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

            Message::CurveChanged(contrast, rolloff) => {
                self.curve_contrast = contrast;
                self.curve_rolloff = rolloff;
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_curve(contrast, rolloff);
                }
                Task::none()
            }

            Message::CurveReset => {
                self.curve_contrast = 1.0;
                self.curve_rolloff = 1.0;
                if let Some(shader) = &mut self.detail_shader {
                    shader.set_curve(self.curve_contrast, self.curve_rolloff);
                }
                Task::none()
            }

            Message::RollsLoaded(rolls) => {
                self.rolls = rolls;
                // A refresh supersedes any earlier cover chain.
                self.cover_inflight.clear();
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

            Message::AddRoll => cosmic::task::future(async {
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
            }),

            Message::RollAdded(dir) => {
                // Persist the new roll; the library page owns the roll list.
                if self.active.is_none() && !self.rolls.iter().any(|roll| roll.dir == dir) {
                    self.config.rolls.push(dir.to_string_lossy().into_owned());
                    self.persist_config();
                }
                // Scan the chosen directory for its cover and display name.
                cosmic::task::future(async move { Message::RollInfoLoaded(load_roll(dir).await) })
            }

            Message::RollActivated(dir) => {
                if self.active.as_deref() == Some(dir.as_path()) {
                    return Task::none();
                }
                // The outgoing roll keeps its unsaved edits.
                self.persist_roll();
                self.active = Some(dir.clone());
                self.selected = None;
                self.clear_detail();
                self.close_editing();
                cosmic::task::future(async move {
                    let files = load_files_in(dir.clone()).await;
                    Message::RollOpened(dir, files)
                })
            }

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

            Message::BackToRolls => {
                self.persist_roll();
                self.active = None;
                self.selected = None;
                self.tiles = Vec::new();
                self.thumb_inflight.clear();
                self.detail_inflight = None;
                self.clear_detail();
                self.close_editing();
                Task::none()
            }

            Message::SearchChanged(query) => {
                self.query = query;
                Task::none()
            }

            Message::ThumbReady(name, result) => {
                if let Some(tile) = self.tiles.iter_mut().find(|tile| tile.name == name) {
                    tile.thumb = match result {
                        Ok(handle) => Thumb::Ready(handle),
                        Err(()) => Thumb::Failed,
                    };
                }

                self.thumb_inflight.retain(|pending| pending != &name);
                self.decode_next()
            }

            Message::ToggleContextPage(context_page) => {
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

            Message::UpdateConfig(config) => {
                self.config = config;
                Task::none()
            }

            Message::Ignore => Task::none(),

            Message::EditSave => {
                self.persist_roll();
                // The stored edit changed; re-bake the active file's grid
                // thumbnail so the tile reflects the exposure. The decode
                // reads the EV from the manifest when it starts, so even a
                // queued re-bake catches the latest value.
                if let Some(name) = self.selected.clone() {
                    if let Some(tile) = self.tiles.iter_mut().find(|tile| tile.name == name) {
                        tile.thumb = Thumb::Loading;
                    }
                    return self.decode_next();
                }
                Task::none()
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
            return Task::none();
        }

        let pending: Vec<(String, f32)> = self
            .tiles
            .iter()
            .filter(|tile| matches!(tile.thumb, Thumb::Loading))
            .filter(|tile| !self.thumb_inflight.iter().any(|name| name == &tile.name))
            .take(capacity)
            .map(|tile| (tile.name.clone(), self.roll.exposure_ev(&tile.name)))
            .collect();

        if pending.is_empty() {
            return Task::none();
        }

        self.thumb_inflight
            .extend(pending.iter().map(|(name, _)| name.clone()));

        Task::batch(
            pending
                .into_iter()
                .map(move |(name, ev)| cosmic::task::future(decode_thumbnail(dir.clone(), name, ev))),
        )
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

        detail_trace(format_args!(
            "trigger fire: cap={cap}, shader={}, zoom={:.3}",
            self.detail_shader.is_some(),
            self.detail_zoom
        ));

        let name = self
            .selected
            .clone()
            .expect("selected when a decode is due");
        self.detail_inflight = Some(name.clone());

        let Some(dir) = self.active.clone() else {
            return Task::none();
        };

        cosmic::task::future(decode_detail(dir, name, cap))
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
        self.curve_contrast = 1.0;
        self.curve_rolloff = 1.0;
        self.exposure_ev = 0.0;
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
                        width,
                        height,
                        self.detail_zoom
                    ));
                    // Cache the current thumbnail so it stays visible over the
                    // shader during the crossfade (first load only — a level-up
                    // swap must not re-insert the faded thumb).
                    if fresh_open
                        && let Some(tile) = self
                            .tiles
                            .iter()
                            .find(|t| t.name == name)
                            .and_then(|t| match &t.thumb {
                                Thumb::Ready(h) => Some(h.clone()),
                                _ => None,
                            })
                    {
                        self.detail_thumb = Some(tile);
                    }
                    let image_id = self.next_image_id;
                    self.next_image_id = self.next_image_id.wrapping_add(1);
                    self.detail_shader = Some(exposure_shader::ExposureProgram::new(
                        mono,
                        width,
                        height,
                        self.exposure_ev,
                        image_id,
                    ));
                    // Carry over any zoom/pan the user applied while the decode
                    // was in flight (the program starts at contain fit).
                    if let Some(shader) = &mut self.detail_shader {
                        shader.set_view(self.detail_zoom, self.detail_pan);
                        shader.set_curve(self.curve_contrast, self.curve_rolloff);
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

/// The default library directory a fresh install seeds, preserving the POC's
/// original single-roll arrangement: the user's `~/Pictures/exposure`.
fn default_library_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| Path::new(&home).join("Pictures").join("exposure"))
}

/// Loads one roll's metadata: display name (directory leaf) and cover file
/// (first sorted non-dot file), with nothing decoded yet.
async fn load_roll(dir: PathBuf) -> Roll {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| dir.to_string_lossy().into_owned(), str::to_string);
    let cover = cover_name(&dir).await;
    Roll {
        dir,
        name,
        cover,
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

/// Returns the first regular non-dot file name in `dir` in sorted order — the
/// roll's cover, if the roll has any negatives yet.
async fn cover_name(dir: &Path) -> Option<String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return None;
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
    files.into_iter().next()
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

/// Renders the fixed controls row above the content: Add roll (library page)
/// or a back button (inside a roll), with the roll/frame search beside them.
///
/// Stays outside the scrollable, so the tools remain visible while the grid
/// scrolls — and above the detail overlay, where the back button doubles as
/// an out-of-roll escape.
fn controls_row(app: &AppModel) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let mut tools = widget::row::with_capacity(2).spacing(space_s).width(Length::Fill);

    if app.active.is_some() {
        tools = tools.push(
            widget::button::standard(fl!("back-to-rolls"))
                .leading_icon(widget::icon::from_name("go-previous-symbolic"))
                .on_press(Message::BackToRolls),
        );
    } else {
        tools = tools.push(
            widget::button::standard(fl!("add-roll"))
                .leading_icon(widget::icon::from_name("list-add-symbolic"))
                .on_press(Message::AddRoll),
        );
    }

    tools = tools.push(
        widget::search_input(fl!("search-rolls"), app.query.as_str())
            .on_input(Message::SearchChanged)
            .width(Length::Fill),
    );

    widget::container(tools)
        .width(Length::Fill)
        .padding(space_s)
        .into()
}

/// Renders the library page: a responsive grid of roll cover tiles, or an
/// empty-state message when there are no rolls.
fn library_view(app: &AppModel) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let query = app.query.trim().to_lowercase();
    let matched: Vec<&Roll> = app
        .rolls
        .iter()
        .filter(move |roll| {
            query.is_empty() || roll.name.to_lowercase().contains(&query)
        })
        .collect();

    if matched.is_empty() {
        return widget::container(widget::text(if app.rolls.is_empty() {
            fl!("no-rolls")
        } else {
            fl!("no-rolls-found")
        }))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center)
        .into();
    }

    let grid = Grid::with_children(matched.into_iter().map(roll_tile))
        .fluid(THUMB_SIZE)
        .height(grid::Sizing::AspectRatio(TILE_ASPECT))
        .spacing(space_s);

    widget::scrollable(grid).height(Length::Fill).into()
}

/// Renders an open roll's frame grid (search-filtered), with the detail view
/// overlaid on an opaque surface when a frame is selected.
///
/// The grid stays mounted (scroll position persists) under the detail surface
/// that captures input, so the detail view cannot leak wheel/clicks to it.
fn frames_view(app: &AppModel) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let query = app.query.trim().to_lowercase();
    let matched: Vec<&Tile> = app
        .tiles
        .iter()
        .filter(|tile| query.is_empty() || tile.name.to_lowercase().contains(&query))
        .collect();

    let tiles: Element<'_, Message> = if matched.is_empty() {
        widget::container(widget::text(fl!("no-files")))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into()
    } else {
        let grid = Grid::with_children(matched.into_iter().map(tile_view))
            .fluid(THUMB_SIZE)
            .height(grid::Sizing::AspectRatio(TILE_ASPECT))
            .spacing(space_s);

        widget::scrollable(grid).height(Length::Fill).into()
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
    let value_text = widget::text(format!("{:+.2} EV", app.exposure_ev));
    let slider = widget::slider(-3.0..=3.0, app.exposure_ev, Message::ExposureChanged)
        .step(0.01_f32)
        // A finished drag is an edit flush point.
        .on_release(Message::EditSave);

    // Non-persisted tone-curve preview: two power sliders re-shape the GPU
    // texture via a uniform-only remap. Contrast pivots at the image's
    // measured mid-gray; highlight rolloff at the measured white point —
    // each visibly different from exposure's gain. Grid thumbnails are
    // unaffected; every detail open starts from the identity.
    let tone_label = widget::text(fl!("tone-label"));
    let contrast_label = widget::text(fl!("contrast-label"));
    let contrast_value = widget::text(format!("{:.2}", app.curve_contrast));
    let contrast_slider = widget::slider(
        0.5..=1.5,
        app.curve_contrast,
        // When either slider moves, the other value travels along so the
        // remap always composes the full curve, not a half-updated one.
        move |contrast| Message::CurveChanged(contrast, app.curve_rolloff),
    )
    .step(0.05_f32);
    let rolloff_label = widget::text(fl!("rolloff-label"));
    let rolloff_value = widget::text(format!("{:.2}", app.curve_rolloff));
    let rolloff_slider = widget::slider(
        0.5..=1.5,
        app.curve_rolloff,
        move |rolloff| Message::CurveChanged(app.curve_contrast, rolloff),
    )
    .step(0.05_f32);
    let reset = widget::button::standard(fl!("tone-reset")).on_press(Message::CurveReset);

    widget::column::with_capacity(12)
        .push(title)
        .push(label)
        .push(slider)
        .push(value_text)
        .push(tone_label)
        .push(contrast_label)
        .push(contrast_slider)
        .push(contrast_value)
        .push(rolloff_label)
        .push(rolloff_slider)
        .push(rolloff_value)
        .push(reset)
        .spacing(space_s)
        .width(Length::Fill)
        .into()
}

/// Renders a library page roll card, filling the square cell the grid assigns
/// it. Clicking the cover drills into the roll's frame grid.
fn roll_tile(roll: &Roll) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let preview: Element<'_, Message> = match &roll.thumb {
        Thumb::Ready(handle) => MouseArea::new(
            widget::image(handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Contain),
        )
        .on_press(Message::RollActivated(roll.dir.clone()))
        .into(),
        Thumb::Loading => icon::from_name("image-loading-symbolic").icon().into(),
        Thumb::Failed => icon::from_name("image-missing-symbolic").icon().into(),
    };

    widget::column::with_capacity(2)
        .push(
            widget::container(preview)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center),
        )
        .push(widget::text(&roll.name))
        .spacing(space_s)
        .align_x(Horizontal::Center)
        .into()
}

/// Renders a single frame tile of the open roll, filling the square cell the
/// grid assigns it.
fn tile_view(tile: &Tile) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let preview: Element<'_, Message> = match &tile.thumb {
        Thumb::Ready(handle) => MouseArea::new(
            widget::image(handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Contain),
        )
        .on_double_click(Message::ThumbnailActivated(tile.name.clone()))
        .into(),
        Thumb::Loading => icon::from_name("image-loading-symbolic").icon().into(),
        Thumb::Failed => icon::from_name("image-missing-symbolic").icon().into(),
    };

    widget::column::with_capacity(2)
        .push(
            widget::container(preview)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center),
        )
        .push(widget::text(&tile.name))
        .spacing(space_s)
        .align_x(Horizontal::Center)
        .into()
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
            .map(exposure_shader::ExposureProgram::view)
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
fn zoom_about_anchor(
    zoom_old: f32,
    zoom_new: f32,
    pan: (f32, f32),
    cursor: Point,
) -> (f32, f32) {
    let ratio = (zoom_new - zoom_old).exp2();
    let k = 1.0 - ratio;
    (
        pan.0 + k * (cursor.x - pan.0),
        pan.1 + k * (cursor.y - pan.1),
    )
}

/// Decodes a RAW frame from the open roll into a thumbnail message, baking in
/// the given exposure so the grid tile reflects the stored edit.
async fn decode_thumbnail(dir: PathBuf, name: String, exposure_ev: f32) -> Message {
    let result =
        decode_raw(dir, name.clone(), move |image| convert_thumbnail(image, THUMB_SIZE, exposure_ev))
            .await;

    Message::ThumbReady(name, result)
}

/// Decodes a roll's cover file into a thumbnail message (no stored exposure:
/// roll cards are not per-frame editable).
async fn decode_cover(dir: PathBuf, name: String) -> Message {
    let result =
        decode_raw(dir.clone(), name, |image| convert_thumbnail(image, THUMB_SIZE, 0.0)).await;

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

        let (mono, width, height) =
            resize_area(&mono, width as u32, height as u32, max_edge, 1);

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
fn downsample_rgb(
    image: &rawloader::RawImage,
    out_w: usize,
    out_h: usize,
) -> Option<Vec<f32>> {
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
fn downsample_bayer(
    image: &rawloader::RawImage,
    out_w: usize,
    out_h: usize,
) -> Option<Vec<f32>> {
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

/// Converts a decoded RAW image into a small oriented RGBA image, scaled so no
/// dimension exceeds `max_size`, baking `exposure_ev` into the pixels.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn convert_thumbnail(
    image: &rawloader::RawImage,
    max_size: f32,
    exposure_ev: f32,
) -> Result<Handle, ()> {
    // One fused pass: normalize, discard masked borders, and phase-preserve
    // downscale straight from the sensor samples into a small linear negative.
    let (mut mono, width, height) =
        downsample_thumbnail(image, max_size as u32).ok_or(())?;

    // Anchor the black point on the frame's clearest film, then invert the
    // negative in density space.
    let base = measure_base(&mono)
        .filter(|measured| *measured >= MIN_PLAUSIBLE_BASE)
        .unwrap_or(ACTIVE_STOCK.base);
    invert_gray(&mut mono, &ACTIVE_STOCK, base);

    // Restore edge punch lost to the heavy downscale, before tone encoding so
    // overshoot stays out of the perceptually amplified display range.
    unsharp_mask(&mut mono, width as usize, height as usize);

    // Bake the stored exposure in linear light, matching the detail shader's
    // `mono_linear * 2^EV`, so grid tile and detail view agree.
    apply_exposure(&mut mono, exposure_ev);

    for value in &mut mono {
        *value = srgb_encode(*value);
    }

    let mut rgba = Vec::with_capacity(mono.len() * 4);
    for &value in &mono {
        let level = (value * 255.0).round() as u8;
        rgba.extend_from_slice(&[level, level, level, 255]);
    }

    let (rgba, width, height) = orient(&rgba, width, height, image.orientation);

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    About,
}

impl menu::action::MenuAction for MenuAction {
    type Message = Message;

    fn message(&self) -> Self::Message {
        match self {
            MenuAction::About => Message::ToggleContextPage(ContextPage::About),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let image = raw_with_transmissions(
            2,
            2,
            "RGGB",
            vec![600, 500, 500, 400],
            [0, 0, 0, 0],
        );

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
}
