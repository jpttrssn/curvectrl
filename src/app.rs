// SPDX-License-Identifier: MPL-2.0

use crate::config::Config;
use crate::fl;
use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::widget::{Grid, grid};
use cosmic::iced::{Alignment, ContentFit, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, icon, image::Handle, menu, nav_bar};
use std::collections::HashMap;
use std::path::Path;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const APP_ICON: &[u8] = include_bytes!("../resources/icons/hicolor/scalable/apps/icon.svg");

/// Maximum dimension of decoded RAW thumbnails, also the maximum Page 1 tile width.
const THUMB_SIZE: f32 = 384.0;
/// Aspect ratio (width / height) of the image area of a Page 1 tile.
const TILE_ASPECT: f32 = 1.0;

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// Display a context drawer with the designated page if defined.
    context_page: ContextPage,
    /// The about page for this app.
    about: About,
    /// Contains items assigned to the nav bar panel.
    nav: nav_bar::Model,
    /// Key bindings for the application's menu bar.
    key_binds: HashMap<menu::KeyBind, MenuAction>,
    /// Configuration data that persists between application runs.
    config: Config,
    /// File entries from the pictures directory, displayed as tiles on Page 1.
    tiles: Vec<Tile>,
}

/// A file entry displayed as a tile on Page 1.
struct Tile {
    /// File name.
    name: String,
    /// Decoded thumbnail state.
    thumb: Thumb,
}

/// Thumbnail loading state of a [`Tile`].
enum Thumb {
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
    FilesLoaded(Vec<String>),
    LaunchUrl(String),
    ThumbReady(String, Result<Handle, ()>),
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
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        // Create a nav bar with three page items.
        let mut nav = nav_bar::Model::default();

        nav.insert()
            .text(fl!("page-id", num = 1))
            .data::<Page>(Page::Page1)
            .icon(icon::from_name("applications-science-symbolic"))
            .activate();

        nav.insert()
            .text(fl!("page-id", num = 2))
            .data::<Page>(Page::Page2)
            .icon(icon::from_name("applications-system-symbolic"));

        nav.insert()
            .text(fl!("page-id", num = 3))
            .data::<Page>(Page::Page3)
            .icon(icon::from_name("applications-games-symbolic"));

        // Create the about widget
        let about = About::default()
            .name(fl!("app-title"))
            .icon(widget::icon::from_svg_bytes(APP_ICON))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("repository"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        // Construct the app model with the runtime's core.
        let mut app = AppModel {
            core,
            context_page: ContextPage::default(),
            about,
            nav,
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
            tiles: Vec::new(),
        };

        // Set the window title and scan the pictures directory in parallel.
        let command = Task::batch([
            app.update_title(),
            cosmic::task::future(async { Message::FilesLoaded(load_files().await) }),
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

    /// Enables the COSMIC application to create a nav bar with this model.
    fn nav_model(&self) -> Option<&nav_bar::Model> {
        Some(&self.nav)
    }

    /// Display a context drawer if the context page is requested.
    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        Some(match self.context_page {
            ContextPage::About => context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_string()),
                Message::ToggleContextPage(ContextPage::About),
            ),
        })
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// Application events will be processed through the view. Any messages emitted by
    /// events received by widgets will be passed to the update method.
    fn view(&self) -> Element<'_, Self::Message> {
        let space_s = cosmic::theme::spacing().space_s;
        let content: Element<_> = match self.nav.active_data::<Page>().unwrap() {
            Page::Page1 => {
                let header = widget::row::with_capacity(2)
                    .push(widget::text::title1(fl!("welcome")))
                    .push(widget::text::title3(fl!("page-id", num = 1)))
                    .align_y(Alignment::End)
                    .spacing(space_s);

                let tiles: Element<'_, Message> = if self.tiles.is_empty() {
                    widget::container(widget::text(fl!("no-files")))
                        .width(Length::Fill)
                        .align_x(Horizontal::Center)
                        .into()
                } else {
                    let grid = Grid::with_children(self.tiles.iter().map(tile_view))
                        .fluid(THUMB_SIZE)
                        .height(grid::Sizing::AspectRatio(TILE_ASPECT))
                        .spacing(space_s);

                    widget::scrollable(grid).height(Length::Fill).into()
                };

                widget::column::with_capacity(2)
                    .push(header)
                    .push(tiles)
                    .spacing(space_s)
                    .height(Length::Fill)
                    .into()
            }

            Page::Page2 => {
                let header = widget::row::with_capacity(2)
                    .push(widget::text::title1(fl!("welcome")))
                    .push(widget::text::title3(fl!("page-id", num = 2)))
                    .align_y(Alignment::End)
                    .spacing(space_s);

                widget::column::with_capacity(1)
                    .push(header)
                    .spacing(space_s)
                    .height(Length::Fill)
                    .into()
            }

            Page::Page3 => {
                let header = widget::row::with_capacity(2)
                    .push(widget::text::title1(fl!("welcome")))
                    .push(widget::text::title3(fl!("page-id", num = 3)))
                    .align_y(Alignment::End)
                    .spacing(space_s);

                widget::column::with_capacity(1)
                    .push(header)
                    .spacing(space_s)
                    .height(Length::Fill)
                    .into()
            }
        };

        widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .apply(widget::container)
            .width(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
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
        let subscriptions = vec![
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

        Subscription::batch(subscriptions)
    }

    /// Handles messages emitted by the application and its widgets.
    ///
    /// Tasks may be returned for asynchronous execution of code in the background
    /// on the application's async runtime.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::FilesLoaded(files) => {
                self.tiles = files
                    .into_iter()
                    .map(|name| Tile {
                        name,
                        thumb: Thumb::Loading,
                    })
                    .collect();

                self.decode_next()
            }

            Message::ThumbReady(name, result) => {
                if let Some(tile) = self.tiles.iter_mut().find(|tile| tile.name == name) {
                    tile.thumb = match result {
                        Ok(handle) => Thumb::Ready(handle),
                        Err(()) => Thumb::Failed,
                    };
                }

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

            Message::LaunchUrl(url) => match open::that_detached(&url) {
                Ok(()) => Task::none(),
                Err(err) => {
                    eprintln!("failed to open {url:?}: {err}");
                    Task::none()
                }
            },
        }
    }

    /// Called when a nav item is selected.
    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        // Activate the page in the model.
        self.nav.activate(id);

        self.update_title()
    }
}

impl AppModel {
    /// Updates the header and window titles.
    pub fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut window_title = fl!("app-title");

        if let Some(page) = self.nav.text(self.nav.active()) {
            window_title.push_str(" — ");
            window_title.push_str(page);
        }

        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(window_title, id)
        } else {
            Task::none()
        }
    }

    /// Spawns decoding of the next pending thumbnail, if any.
    ///
    /// Decoding is chained sequentially so that only one full-resolution RAW
    /// buffer is held in memory at a time.
    fn decode_next(&mut self) -> Task<cosmic::Action<Message>> {
        let name = self
            .tiles
            .iter()
            .find(|tile| matches!(tile.thumb, Thumb::Loading))
            .map(|tile| tile.name.clone());

        match name {
            Some(name) => cosmic::task::future(decode_thumbnail(name)),
            None => Task::none(),
        }
    }
}

/// Scans `~/Pictures/exposure` for regular files and returns their sorted names.
async fn load_files() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };

    let Ok(mut entries) =
        tokio::fs::read_dir(Path::new(&home).join("Pictures").join("exposure")).await
    else {
        return Vec::new();
    };

    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|ty| ty.is_file())
            && let Some(name) = entry.file_name().into_string().ok()
        {
            files.push(name);
        }
    }

    files.sort();
    files
}

/// Renders a single Page 1 tile, filling the square cell the grid assigns it.
fn tile_view(tile: &Tile) -> Element<'_, Message> {
    let space_s = cosmic::theme::spacing().space_s;

    let preview: Element<'_, Message> = match &tile.thumb {
        Thumb::Ready(handle) => widget::image(handle.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(ContentFit::Contain)
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

/// Decodes a RAW file from the pictures directory into a thumbnail message.
async fn decode_thumbnail(name: String) -> Message {
    let Ok(home) = std::env::var("HOME") else {
        return Message::ThumbReady(name, Err(()));
    };

    let path = Path::new(&home)
        .join("Pictures")
        .join("exposure")
        .join(&name);

    // RAW decoding is CPU-heavy, so run it on a blocking worker thread.
    let result = tokio::task::spawn_blocking(move || {
        rawloader::decode_file(&path)
            .map_err(|_| ())
            .and_then(|image| convert_thumbnail(&image))
    })
    .await
    .unwrap_or(Err(()));

    Message::ThumbReady(name, result)
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

/// Reduces a mosaic sample buffer to half-resolution RGB pixels by averaging each
/// 2x2 sensor block per CFA channel.
///
/// Returns `None` when the dimensions degenerate or any block lacks an entire R/G/B
/// channel, signaling callers to fall back to a simpler conversion.
#[allow(clippy::cast_precision_loss)]
fn demosaic_half(
    samples: &[f32],
    width: usize,
    height: usize,
    cfa: &rawloader::CFA,
) -> Option<(Vec<f32>, usize, usize)> {
    let (out_width, out_height) = (width / 2, height / 2);
    if out_width == 0 || out_height == 0 || samples.len() < width * height {
        return None;
    }

    let mut rgb = vec![0.0_f32; out_width * out_height * 3];
    for by in 0..out_height {
        for bx in 0..out_width {
            let mut sums = [0.0_f32; 3];
            let mut counts = [0_usize; 3];

            for dy in 0..2 {
                for dx in 0..2 {
                    let y = by * 2 + dy;
                    let x = bx * 2 + dx;
                    let color = cfa.color_at(y, x);
                    if color < 3 {
                        sums[color] += samples[y * width + x];
                        counts[color] += 1;
                    }
                }
            }

            let base = (by * out_width + bx) * 3;
            for (channel, slot) in rgb[base..base + 3].iter_mut().enumerate() {
                *slot = if counts[channel] > 0 {
                    sums[channel] / counts[channel] as f32
                } else {
                    return None;
                };
            }
        }
    }

    Some((rgb, out_width, out_height))
}

/// Applies in-file white balance gains to interleaved RGB, normalized against green.
fn apply_white_balance(rgb: &mut [f32], coeffs: [f32; 4]) {
    let green = coeffs[1];
    let gains = if green > f32::EPSILON {
        [coeffs[0] / green, 1.0, coeffs[2] / green]
    } else {
        [1.0; 3]
    };

    for [r, g, b] in rgb.as_chunks_mut::<3>().0 {
        *r *= gains[0];
        *g *= gains[1];
        *b *= gains[2];
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

/// Converts a decoded RAW image into a small oriented RGBA thumbnail.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn convert_thumbnail(image: &rawloader::RawImage) -> Result<Handle, ()> {
    let width = usize::max(image.width, 1);
    let height = usize::max(image.height, 1);

    let samples = normalize_samples(image);

    // RGB sources pass through; bayer mosaics are demosaiced into half-resolution
    // true-color pixels, falling back to a gray 2x2 box average for degenerate or
    // unsupported CFA patterns.
    let (mut rgb, width, height) = if image.cpp >= 3 {
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

        (rgb, width, height)
    } else if let Some((rgb, half_width, half_height)) =
        demosaic_half(&samples, width, height, &image.cfa)
    {
        let mut rgb = rgb;
        apply_white_balance(&mut rgb, image.wb_coeffs);
        (rgb, half_width, half_height)
    } else {
        let (half_width, half_height) = (width / 2, height / 2);
        if half_width == 0 || half_height == 0 || samples.len() < width * height {
            return Err(());
        }

        let mut rgb = Vec::with_capacity(half_width * half_height * 3);
        for y in 0..half_height {
            for x in 0..half_width {
                let sum = samples[y * 2 * width + x * 2]
                    + samples[y * 2 * width + x * 2 + 1]
                    + samples[(y * 2 + 1) * width + x * 2]
                    + samples[(y * 2 + 1) * width + x * 2 + 1];

                rgb.extend_from_slice(&[sum / 4.0, sum / 4.0, sum / 4.0]);
            }
        }

        (rgb, half_width, half_height)
    };

    for value in &mut rgb {
        *value = srgb_encode(*value);
    }

    let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
    for [r, g, b] in rgb.as_chunks::<3>().0 {
        rgba.extend_from_slice(&[
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
            255,
        ]);
    }

    let (rgba, width, height) =
        resize_nearest(&rgba, width as u32, height as u32, THUMB_SIZE as u32);
    let (rgba, width, height) = orient(&rgba, width, height, image.orientation);

    Ok(Handle::from_rgba(width, height, rgba))
}

/// Resizes an RGBA buffer with nearest-neighbor sampling, never upscaling.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn resize_nearest(rgba: &[u8], width: u32, height: u32, max: u32) -> (Vec<u8>, u32, u32) {
    let scale = f32::min(
        1.0,
        f32::min(max as f32 / width as f32, max as f32 / height as f32),
    );
    let out_width = u32::max((width as f32 * scale) as u32, 1);
    let out_height = u32::max((height as f32 * scale) as u32, 1);

    let mut resized = Vec::with_capacity((out_width * out_height * 4) as usize);
    for y in 0..out_height {
        let source_y = (u64::from(y) * u64::from(height) / u64::from(out_height)) as usize;
        for x in 0..out_width {
            let source_x = (u64::from(x) * u64::from(width) / u64::from(out_width)) as usize;
            let offset = (source_y * width as usize + source_x) * 4;
            resized.extend_from_slice(&rgba[offset..offset + 4]);
        }
    }

    (resized, out_width, out_height)
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

/// The page to display in the application.
pub enum Page {
    Page1,
    Page2,
    Page3,
}

/// The context page to display in the context drawer.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ContextPage {
    #[default]
    About,
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
    fn demosaic_reduces_rggb_block_to_rgb() {
        let cfa = rawloader::CFA::new("RGGB");
        let samples = [1.0, 0.5, 0.25, 0.75];

        let (rgb, width, height) = demosaic_half(&samples, 2, 2, &cfa).unwrap();

        assert_eq!((width, height), (1, 1));
        assert_eq!(rgb, vec![1.0, 0.375, 0.75]);
    }

    #[test]
    fn demosaic_rejects_blocks_missing_a_channel() {
        let cfa = rawloader::CFA::new("GGGG");

        assert!(demosaic_half(&[0.5; 4], 2, 2, &cfa).is_none());
    }

    #[test]
    fn white_balance_normalizes_against_green() {
        let mut rgb = vec![1.0, 1.0, 1.0];

        apply_white_balance(&mut rgb, [2.0, 2.0, 1.0, 1.0]);

        assert_eq!(rgb, vec![1.0, 1.0, 0.5]);
    }
}
