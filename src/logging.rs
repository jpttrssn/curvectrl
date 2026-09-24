// SPDX-License-Identifier: GPL-3.0-or-later

//! The app's logging sink: the `log` facade with a minimal stderr writer, so
//! diagnostics go through the standard macros and can be filtered with
//! `RUST_LOG` (e.g. `curvectrl=debug`) without pulling in a full subscriber
//! tree. The `EXPOSURE_TRACE_DETAIL`/`EXPOSURE_TRACE_REBAKE` env vars force the
//! trace level, preserving the codebase's existing detail/rebake tracing knobs.
//!
//! Output keeps the established "prefix-less" style: `failed to decode
//! /path: reason` — the message is written exactly as passed to the macro.

use log::{LevelFilter, Log, Metadata, Record};

/// A `log` sink that writes every enabled record to stderr.
struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        eprintln!("{}", record.args());
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

/// Installs the stderr logger once. The max level is derived from `RUST_LOG`
/// (best-effort: a bare level or `curvectrl=<level>`), with the two
/// `EXPOSURE_TRACE_*` env vars forcing `trace` so the detail/rebake tracing
/// gates behave exactly as before. Idempotent; safe to call from `main`.
pub(crate) fn init() {
    log::set_logger(&LOGGER).ok();
    log::set_max_level(max_level());
}

/// The max level to emit, from `RUST_LOG` (default `info`) or the trace knobs.
fn max_level() -> LevelFilter {
    if std::env::var("EXPOSURE_TRACE_DETAIL").is_ok()
        || std::env::var("EXPOSURE_TRACE_REBAKE").is_ok()
    {
        return LevelFilter::Trace;
    }
    let Some(spec) = std::env::var("RUST_LOG").ok() else {
        return LevelFilter::Info;
    };
    // `curvectrl=debug` or a bare `debug`; other targets are ignored.
    let (target, level) = spec.split_once('=').unwrap_or(("", &spec));
    if !target.is_empty() && target != "curvectrl" {
        return LevelFilter::Info;
    }
    match level.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        // `info` (or an unknown level) is the default.
        _ => LevelFilter::Info,
    }
}