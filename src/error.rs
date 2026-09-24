// SPDX-License-Identifier: GPL-3.0-or-later

//! The crate's single error type. In practice every error here is terminal: it
//! is logged once at its boundary and the UI shows a fallback (a failed
//! thumbnail keeps its placeholder icon, an unparseable frame shows "No
//! metadata available"). `FrameError` therefore carries human-readable detail
//! rather than a deep `source` chain, and derives `Clone` so it can ride
//! through [`crate::app::Message`] payloads (the message enum is `Clone`).

use std::path::PathBuf;

/// A failure in the RAW-to-image pipeline, an export, or a frame-metadata read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FrameError {
    /// rawloader could not decode the file (the message is its own error text).
    #[error("failed to decode {path}: {message}")]
    Decode {
        /// The file that could not be decoded.
        path: PathBuf,
        /// rawloader's error text.
        message: String,
    },
    /// The decode produced too few (or degenerate) samples for its dimensions.
    #[error("decode produced too few samples for the pixel count")]
    ShortSamples,
    /// A background decode thread panicked.
    #[error("decode thread panicked")]
    ThreadPanic,
    /// An export encode or file-write failure.
    #[error("export failed: {message}")]
    Export {
        /// Human-readable reason (a `format!`-ed message or the underlying
        /// I/O error text).
        message: String,
    },
    /// The frame's EXIF/dimensions could not be parsed.
    #[error("failed to parse frame metadata")]
    Meta,
}