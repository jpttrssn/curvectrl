// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod config;
mod detail_area;
mod edit_manifest;
mod error;
mod exif_writer;
mod export;
mod film;
mod i18n;
mod library;
mod logging;
mod pipeline;
mod shader;
mod ui;

fn main() -> cosmic::iced::Result {
    // Route diagnostics through the `log` facade (stderr, `RUST_LOG`-filtered).
    logging::init();

    // Get the system's preferred languages.
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    // Enable localizations to be applied.
    i18n::init(&requested_languages);

    // Settings for configuring the application window and iced runtime.
    let settings = cosmic::app::Settings::default().size_limits(
        cosmic::iced::Limits::NONE
            .min_width(360.0)
            .min_height(180.0),
    );

    // Starts the application's event loop with `()` as the application's flags.
    cosmic::app::run::<app::AppModel>(settings, ())
}
