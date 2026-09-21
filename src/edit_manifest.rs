// SPDX-License-Identifier: GPL-3.0-or-later

use crate::film::FilmPreset;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name of the per-roll edit manifest inside a roll directory.
///
/// Deliberately names the file after its function rather than the application,
/// so an application rename never orphans existing manifests.
pub const ROLL_MANIFEST_FILE: &str = ".film-roll.toml";

/// Exposure applied to an image until an edit records otherwise.
///
/// Matches Darktable's default +0.7 EV starting point: with the auto-expose
/// (`normalize_positive`) removed, EV 0 is true sensor exposure and untouched
/// frames open at this visible base rather than black. The slider spans
/// −3..+4 so the user can drag down to honest sensor exposure.
pub const DEFAULT_EXPOSURE_EV: f32 = 0.7;

/// Contrast power applied until an edit records otherwise (identity `1.0`).
pub const DEFAULT_CURVE_CONTRAST: f32 = 1.0;

/// Highlight-rolloff power applied until an edit records otherwise (identity
/// `1.0`).
pub const DEFAULT_CURVE_ROLLOFF: f32 = 1.0;

/// Shadows power (pivoted at the image's measured shadow anchor) applied
/// until an edit records otherwise (identity `1.0`).
pub const DEFAULT_CURVE_SHADOWS: f32 = 1.0;

/// Serializable per-file edits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EditData {
    /// Exposure compensation in EV (−3.00 to +4.00).
    #[serde(default = "default_exposure")]
    pub exposure_ev: f32,
    /// Tone-curve contrast power (pivoted at the image's measured mid-gray),
    /// `1.0` = identity. Missing in older manifests stays `1.0`.
    #[serde(default = "default_curve_identity")]
    pub curve_contrast: f32,
    /// Tone-curve highlight-rolloff power (pivoted at the image's measured
    /// white point), `1.0` = identity. Missing in older manifests stays `1.0`.
    #[serde(default = "default_curve_identity")]
    pub curve_rolloff: f32,
    /// Tone-curve shadows power (pivoted at the image's measured shadow
    /// anchor), `1.0` = identity. Missing in older manifests stays `1.0`.
    #[serde(default = "default_curve_identity")]
    pub curve_shadows: f32,
    /// Source-pixel crop margins removed from each edge. Missing in older
    /// manifests stays the all-zero (no-crop) [`CropMargins::default`].
    ///
    /// A crop is a first-class edit that persists and renders everywhere, but
    /// it is deliberately NOT part of [`ToneEdit`] so copy/paste never carries
    /// a crop onto another frame.
    #[serde(default)]
    pub crop: CropMargins,
    /// User-requested display rotation on TOP of the RAW's EXIF orientation:
    /// cumulative counter-clockwise 90° quarter-turns (`0`…`3`). Only the
    /// display (and export) of the frame, so the crop margins are still
    /// authored in the EXIF-upright source frame and are NOT re-interpreted
    /// here. Missing in older manifests stays `0` (no user rotation).
    #[serde(default)]
    pub rotation: u8,
}

impl Default for EditData {
    /// A fresh, un-edited entry: zero exposure, identity tone curve, no crop.
    fn default() -> Self {
        Self {
            exposure_ev: DEFAULT_EXPOSURE_EV,
            curve_contrast: DEFAULT_CURVE_CONTRAST,
            curve_rolloff: DEFAULT_CURVE_ROLLOFF,
            curve_shadows: DEFAULT_CURVE_SHADOWS,
            crop: CropMargins::default(),
            rotation: 0,
        }
    }
}

/// `#[serde(default)]` target so a legacy manifest entry without the curve
/// fields loads as the identity curve.
#[allow(clippy::unnecessary_wraps)]
fn default_curve_identity() -> f32 {
    1.0
}

/// `#[serde(default)]` target so a legacy manifest entry without an exposure
/// field loads the current base exposure rather than a raw `0.0`.
#[allow(clippy::unnecessary_wraps)]
fn default_exposure() -> f32 {
    DEFAULT_EXPOSURE_EV
}

/// A per-file edit aggregated into one value the decode/thumbnail pipelines
/// can thread through without a 5-tuple or per-field reads.
///
/// All fields are identities at `Default`: zero exposure, identity powers.
/// `Default` must match the un-edited rendering exactly so a manifest entry
/// without an edit paints identical to `EditData::default()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneEdit {
    pub exposure_ev: f32,
    pub curve_contrast: f32,
    pub curve_rolloff: f32,
    pub curve_shadows: f32,
}

impl Default for ToneEdit {
    fn default() -> Self {
        Self {
            exposure_ev: DEFAULT_EXPOSURE_EV,
            curve_contrast: DEFAULT_CURVE_CONTRAST,
            curve_rolloff: DEFAULT_CURVE_ROLLOFF,
            curve_shadows: DEFAULT_CURVE_SHADOWS,
        }
    }
}

impl ToneEdit {
    /// The value stored for a file carrying no edits.
    #[must_use]
    pub fn identity() -> Self {
        Self::default()
    }
}

/// Source-pixel margins removed from each edge of a frame by a keyboard-driven
/// crop, preserving the natural aspect ratio.
///
/// `top`/`right`/`bottom`/`left` are in source image pixels (raw photosites) and
/// are applied at decode time, so a 1px margin trims 1 real sensor pixel
/// regardless of display scale. The all-zero [`Default`] is identity (no crop).
///
/// Deliberately kept OUT of [`ToneEdit`]: copy/paste copies only tone, so a crop
/// can never be pasted onto another frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CropMargins {
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub left: u32,
}

impl CropMargins {
    /// The cropped width given the source width, as a non-negative [`u32`].
    #[must_use]
    pub fn cropped_width(self, source: u32) -> u32 {
        source.saturating_sub(self.left).saturating_sub(self.right)
    }

    /// The cropped height given the source height, as a non-negative [`u32`].
    #[must_use]
    pub fn cropped_height(self, source: u32) -> u32 {
        source.saturating_sub(self.top).saturating_sub(self.bottom)
    }
}

/// Which edge a keyboard crop press trims. Only the four edges are exposed; a
/// corner is built by trimming two adjoining edges sequentially. The anchor is
/// auto-selected as the opposite edge's midpoint, and the perpendicular margins
/// derive from the aspect ratio to keep the frame ratio-locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropDirection {
    Top,
    Bottom,
    Left,
    Right,
}

/// A film roll directory's app-owned manifest: roll metadata plus per-file
/// edits, keyed by file name within the roll. The directory itself is the
/// scope, so equal file names across two rolls never collide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollManifest {
    /// Manifest format revision; bumped when the on-disk schema changes.
    pub version: u32,
    /// Optional human-readable roll label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Edits keyed by file name within the roll.
    #[serde(default)]
    pub edits: HashMap<String, EditData>,
    /// The roll's film-inversion preset, stored as its choice key. Absent means
    /// the default [`FilmPreset::None`] (a non-negative/regular-RAW roll). The
    /// base-mode strategy (per-frame auto, a designated calibration frame, or
    /// the stock's preset base) lives in the preset choice itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The roll's resolved black point: the measured clear-film transmission of
    /// [`Self::calibration_frame`]. Meaningful only while the roll's preset is
    /// [`FilmPreset::AutoSelectedFrame`]; every frame inverts against this same
    /// base. Absent falls back to the stock's preset base (preset-first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<f32>,
    /// The frame name whose measured clear-film plateau is the roll's black
    /// point under [`FilmPreset::AutoSelectedFrame`]. Defaults to the roll's
    /// first sorted frame when that preset is chosen; `None` under any other
    /// preset (and never a decode input — only the dot indicator and the
    /// calibration flow read it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_frame: Option<String>,
    /// The roll's start date (the first shot), an ISO `YYYY-MM-DD` string set
    /// from the roll-info context drawer. Absent means undated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    /// The roll's optional end date (the last shot), ISO `YYYY-MM-DD`. Absent
    /// means the roll is undated or a single-day roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
}

impl Default for RollManifest {
    fn default() -> Self {
        Self {
            version: 8,
            name: None,
            edits: HashMap::new(),
            preset: None,
            base: None,
            calibration_frame: None,
            start_date: None,
            end_date: None,
        }
    }
}

impl RollManifest {
    /// Records the exposure for `name`, updating an existing entry in place.
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_exposure(&mut self, name: &str, exposure_ev: f32) {
        if let Some(edit) = self.edits.get_mut(name) {
            edit.exposure_ev = exposure_ev;
        } else {
            self.edits.insert(
                name.to_owned(),
                EditData {
                    exposure_ev,
                    ..Default::default()
                },
            );
        }
    }

    /// Records the tone curve for `name` (contrast + highlight rolloff +
    /// shadows), updating an existing entry in place. All identities are
    /// `1.0`.
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_curve(&mut self, name: &str, contrast: f32, rolloff: f32, shadows: f32) {
        if let Some(edit) = self.edits.get_mut(name) {
            edit.curve_contrast = contrast;
            edit.curve_rolloff = rolloff;
            edit.curve_shadows = shadows;
        } else {
            self.edits.insert(
                name.to_owned(),
                EditData {
                    exposure_ev: DEFAULT_EXPOSURE_EV,
                    curve_contrast: contrast,
                    curve_rolloff: rolloff,
                    curve_shadows: shadows,
                    ..EditData::default()
                },
            );
        }
    }

    /// The full edit for `name` as a single [`ToneEdit`], merging exposure
    /// and the curve powers. Files (or manifest fields) never touched fall
    /// back to their identities.
    ///
    /// The crop margins are intentionally NOT part of this aggregate: it is
    /// the copy/paste payload, and a crop must never be copied/pasted.
    #[must_use]
    pub fn tone(&self, name: &str) -> ToneEdit {
        self.edits
            .get(name)
            .map_or_else(ToneEdit::identity, |edit| ToneEdit {
                exposure_ev: edit.exposure_ev,
                curve_contrast: edit.curve_contrast,
                curve_rolloff: edit.curve_rolloff,
                curve_shadows: edit.curve_shadows,
            })
    }

    /// Replaces the full edit for `name` with `tone` (exposure + curve powers
    /// in one step — copy/paste), updating an existing entry in place.
    ///
    /// A paste never touches the target's crop or rotation: the crop margins
    /// and the user rotation survive [`Self::set_tone`] unchanged (or stay
    /// default on a fresh file).
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_tone(&mut self, name: &str, tone: ToneEdit) {
        let existing = self
            .edits
            .get(name)
            .map_or(CropMargins::default(), |edit| edit.crop);
        let rotation = self.edits.get(name).map_or(0, |edit| edit.rotation);
        self.edits.insert(
            name.to_owned(),
            EditData {
                exposure_ev: tone.exposure_ev,
                curve_contrast: tone.curve_contrast,
                curve_rolloff: tone.curve_rolloff,
                curve_shadows: tone.curve_shadows,
                crop: existing,
                rotation,
            },
        );
    }

    /// The crop margins for `name`; files (or manifest fields) never touched
    /// fall back to the all-zero (no-crop) default.
    #[must_use]
    pub fn crop(&self, name: &str) -> CropMargins {
        self.edits
            .get(name)
            .map_or_else(CropMargins::default, |edit| edit.crop)
    }

    /// Records the crop margins for `name`, updating an existing entry in
    /// place. Replaces the whole margins set at once (the keyboard trims build
    /// it up via [`Self::set_crop_amount`]).
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_crop(&mut self, name: &str, crop: CropMargins) {
        if let Some(edit) = self.edits.get_mut(name) {
            edit.crop = crop;
        } else {
            self.edits.insert(
                name.to_owned(),
                EditData {
                    crop,
                    ..EditData::default()
                },
            );
        }
    }

    /// The user rotation for `name`; files (or manifest fields) never touched
    /// fall back to `0` (no rotation).
    #[must_use]
    pub fn rotation(&self, name: &str) -> u8 {
        self.edits.get(name).map_or(0, |edit| edit.rotation)
    }

    /// Records the cumulative counter-clockwise 90° rotation for `name`,
    /// updating an existing entry in place.
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_rotation(&mut self, name: &str, rotation: u8) {
        if let Some(edit) = self.edits.get_mut(name) {
            edit.rotation = rotation;
        } else {
            self.edits.insert(
                name.to_owned(),
                EditData {
                    rotation,
                    ..Default::default()
                },
            );
        }
    }

    /// The film-inversion preset recorded for this roll. A manifest with no
    /// preset key recorded (or an unknown key) falls back to the default
    /// [`FilmPreset::None`].
    #[must_use]
    pub fn preset(&self) -> FilmPreset {
        self.preset
            .as_deref()
            .map_or_else(FilmPreset::default, FilmPreset::from_key)
    }

    /// The roll's human-readable label, or `None` when it uses the directory
    /// leaf as its display name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Records the roll's display label. `Some` overrides the directory leaf;
    /// `None` clears it back to the leaf (the key disappears on the next save,
    /// keeping the default manifest clean for never-renamed rolls).
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    /// Records the film-inversion preset for the roll. Only a non-default
    /// preset is written: any stock preset (`Hp5Plus`, `TriX`, …) or auto
    /// base strategy (`AutoPerFrame`, `AutoSelectedFrame`) stores its choice
    /// key, [`FilmPreset::None`] clears it back to the implicit default (the
    /// key disappears on the next save, which keeps the default manifest clean
    /// for raw scans).
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_preset(&mut self, preset: FilmPreset) {
        match preset {
            FilmPreset::None => self.preset = None,
            _ => self.preset = Some(preset.choice_key().to_owned()),
        }
    }

    /// The roll's calibrated black point (the measured clear-film transmission
    /// of [`Self::calibration_frame`]), if the auto-selected calibration frame
    /// was measured and recorded.
    #[must_use]
    pub const fn calibrated_base(&self) -> Option<f32> {
        self.base
    }

    /// The roll's start date (ISO `YYYY-MM-DD`), or `None` if undated.
    #[must_use]
    pub fn start_date(&self) -> Option<&str> {
        self.start_date.as_deref()
    }

    /// The roll's optional end date (ISO `YYYY-MM-DD`), or `None` if unset.
    #[must_use]
    pub fn end_date(&self) -> Option<&str> {
        self.end_date.as_deref()
    }

    /// Records the roll's start and optional end dates as ISO `YYYY-MM-DD`
    /// strings (raw user input, validated by the UI layer). Passing `None`
    /// clears the corresponding field; an end date without a start date is
    /// still accepted but meaningless until a start date is recorded.
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_dates(&mut self, start: Option<String>, end: Option<String>) {
        self.start_date = start;
        self.end_date = end;
    }

    /// The frame name designated as the roll's auto-calibration reference
    /// (the black-point source under the [`FilmPreset::AutoSelectedFrame`]
    /// preset), if any. `None` under any other preset or before a frame is
    /// designated.
    #[must_use]
    pub fn calibration_frame(&self) -> Option<&str> {
        self.calibration_frame.as_deref()
    }

    /// Designates `name` as the roll's auto-calibration frame: under the
    /// [`FilmPreset::AutoSelectedFrame`] preset its measured clear-film
    /// plateau is the roll's black point. The numeric base itself is recorded
    /// separately via [`Self::set_calibrated_base`].
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_calibration_frame(&mut self, name: &str) {
        self.calibration_frame = Some(name.to_owned());
    }

    /// Records a calibrated black point measured from the roll's calibration
    /// frame. Every frame in the roll then inverts against this same base.
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_calibrated_base(&mut self, base: f32) {
        self.base = Some(base);
    }

    /// Clears the auto-selected calibration reference: both the designated
    /// frame and its measured base. Used when the roll leaves the
    /// [`FilmPreset::AutoSelectedFrame`] preset (the strategy is part of the
    /// preset choice, so a stale reference must not linger for other modes).
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn clear_calibration(&mut self) {
        self.base = None;
        self.calibration_frame = None;
    }

    /// The roll's base-resolution state as a single [`film::BaseConfig`], the
    /// unit threaded into every decode and bake so a rendering reflects the
    /// roll's calibration mode. The strategy is part of the film preset: the
    /// per-frame auto preset opts into per-frame measurement, the
    /// auto-selected-frame preset pins the whole roll to its measured
    /// calibration, and every stock preset (and `None`) resolves preset-first.
    #[must_use]
    pub fn base_config(&self) -> crate::film::BaseConfig {
        match self.preset() {
            FilmPreset::AutoPerFrame => crate::film::BaseConfig {
                calibrated: None,
                auto: true,
            },
            FilmPreset::AutoSelectedFrame => crate::film::BaseConfig {
                calibrated: self.calibrated_base(),
                auto: false,
            },
            FilmPreset::None | FilmPreset::Hp5Plus | FilmPreset::TriX | FilmPreset::Fp4Plus
            | FilmPreset::TMax400 | FilmPreset::Fomapan100 | FilmPreset::Delta400
            | FilmPreset::Fomapan400 | FilmPreset::Kentmere400 => crate::film::BaseConfig {
                calibrated: None,
                auto: false,
            },
        }
    }
}

/// Full path to the roll manifest inside `dir`.
#[must_use]
pub fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(ROLL_MANIFEST_FILE)
}

/// Loads the roll manifest for `dir`.
///
/// A missing manifest maps to a default roll. An unreadable or malformed
/// manifest likewise falls back to a default roll and is reported to stderr,
/// so a broken file never blocks scanning the library.
///
/// Legacy manifests are migrated on load: a v7 (or older) roll that explicitly
/// opted into per-frame automatic base measurement (`base_auto = true`) has
/// that toggle folded into the film-preset choice as
/// [`FilmPreset::AutoPerFrame`], the strategy now living in the preset. The
/// older numeric `base` calibration is preserved verbatim (it becomes the
/// `AutoSelectedFrame` black point if that preset is ever chosen).
#[must_use]
pub fn load_roll_manifest(dir: &Path) -> RollManifest {
    let path = manifest_path(dir);
    match std::fs::read(&path) {
        Ok(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => match toml::from_str(text) {
                Ok(mut manifest) => {
                    migrate_legacy_base_auto(&mut manifest, text);
                    manifest
                }
                Err(err) => {
                    eprintln!("malformed edit manifest {}: {err}", path.display());
                    RollManifest::default()
                }
            },
            Err(err) => {
                eprintln!("non-UTF-8 edit manifest {}: {err}", path.display());
                RollManifest::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => RollManifest::default(),
        Err(err) => {
            eprintln!("failed to read edit manifest {}: {err}", path.display());
            RollManifest::default()
        }
    }
}

/// Folds a pre-v8 per-frame auto opt-in into the film-preset choice.
///
/// Before v8 the `base_auto` toggle lived beside the preset; the v8 schema
/// removed it, making the base strategy part of the preset itself. A manifest
/// carrying `base_auto = true` therefore loads as the `AutoPerFrame` preset so
/// the existing behavior survives a version bump.
fn migrate_legacy_base_auto(manifest: &mut RollManifest, text: &str) {
    if manifest.version >= 8 {
        return;
    }
    let legacy_auto = match toml::from_str::<toml::Value>(text) {
        Ok(value) => value
            .get("base_auto")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        Err(_) => false,
    };
    if legacy_auto {
        manifest.preset = Some(FilmPreset::AutoPerFrame.choice_key().to_owned());
    }
}

/// Writes the roll manifest to `dir` atomically via a temp file and rename.
///
/// Unknown fields in an existing manifest are tolerated when loading (future
/// versions never break a scan) but are not preserved on save.
pub fn save_roll_manifest(dir: &Path, manifest: &RollManifest) -> std::io::Result<()> {
    let text = toml::to_string_pretty(manifest).map_err(std::io::Error::other)?;
    let path = manifest_path(dir);
    // The temp name starts with a dot like the manifest, so a scan that skips
    // dotfiles can never pick it up mid-write.
    let tmp = dir.join(format!("{ROLL_MANIFEST_FILE}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Drops edit entries whose file name no longer exists in the roll's scan, so
/// a recycled name can never reattach stale edits.
pub fn reconcile(manifest: &mut RollManifest, files: &[String]) {
    manifest.edits.retain(|name, _| files.contains(name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique scratch directory under the OS temp dir, caller-created and
    /// caller-cleaned.
    fn temp_dir(label: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "exposure-edit-manifest-{}-{label}-{seq}",
            std::process::id()
        ))
    }

    #[test]
    fn round_trip_preserves_edits_and_name() {
        let dir = temp_dir("roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_exposure("IMG_0001.DNG", 0.42);
        manifest.set_exposure("IMG_0002.RAW", -0.75);
        manifest.set_curve("IMG_0001.DNG", 0.85, 1.15, 1.1);
        manifest.set_rotation("IMG_0001.DNG", 1);

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        assert_eq!(loaded.tone("IMG_0001.DNG").exposure_ev, 0.42);
        assert_eq!(loaded.tone("IMG_0002.RAW").exposure_ev, -0.75);
        let tone_one = loaded.tone("IMG_0001.DNG");
        assert_eq!(tone_one.curve_contrast, 0.85);
        assert_eq!(tone_one.curve_rolloff, 1.15);
        assert_eq!(tone_one.curve_shadows, 1.1);
        // The second image carried only an exposure edit → curve identity.
        assert_eq!(
            loaded.tone("IMG_0002.RAW"),
            ToneEdit {
                exposure_ev: -0.75,
                ..ToneEdit::identity()
            }
        );
    }

    #[test]
    fn missing_manifest_is_default() {
        let dir = temp_dir("missing");
        std::fs::create_dir_all(&dir).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, RollManifest::default());
        assert_eq!(loaded.version, 8);
    }

    #[test]
    fn dates_round_trip() {
        let dir = temp_dir("dates");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_dates(Some("2024-05-09".to_owned()), Some("2024-05-12".to_owned()));

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.start_date(), Some("2024-05-09"));
        assert_eq!(loaded.end_date(), Some("2024-05-12"));
    }

    #[test]
    fn name_round_trips_and_clears() {
        let dir = temp_dir("name");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        assert_eq!(manifest.name(), None);

        manifest.set_name(Some("Rollerskates".to_owned()));
        save_roll_manifest(&dir, &manifest).unwrap();
        let mut loaded = load_roll_manifest(&dir);
        assert_eq!(loaded.name(), Some("Rollerskates"));

        // Clearing the label writes it back to None (defaulted on load), so a
        // renamed roll reverts to its directory leaf.
        loaded.set_name(None);
        save_roll_manifest(&dir, &loaded).unwrap();
        let cleared = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(cleared.name(), None);
    }

    #[test]
    fn undated_default_round_trip_stays_clean() {
        let dir = temp_dir("undated");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = RollManifest::default();

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.start_date(), None);
        assert_eq!(loaded.end_date(), None);
    }

    #[test]
    fn malformed_manifest_falls_back_to_default() {
        let dir = temp_dir("malformed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(manifest_path(&dir), "[edits".as_bytes()).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, RollManifest::default());
    }

    #[test]
    fn unicode_file_names_round_trip() {
        let dir = temp_dir("unicode");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_exposure("Rolle 1 IMG_10.DNG", 0.3);

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.tone("Rolle 1 IMG_10.DNG").exposure_ev, 0.3);
    }

    #[test]
    fn unknown_fields_and_tables_are_tolerated() {
        let dir = temp_dir("unknown");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            manifest_path(&dir),
            "version = 1\nfortune = 42\n\n[edits.\"a.DNG\"]\nexposure_ev = 1.5\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.tone("a.DNG").exposure_ev, 1.5);
        // The on-disk version (`version = 1`) is preserved: loading tolerates
        // unknown fields AND older schema versions, so a v1 manifest is read
        // as a v1 manifest.
        assert_eq!(loaded.version, 1);
    }

    #[test]
    fn edit_without_exposure_field_is_identity() {
        let dir = temp_dir("bare-edit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(manifest_path(&dir), "version = 1\n\n[edits.\"a.DNG\"]\n").unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.tone("a.DNG").exposure_ev, DEFAULT_EXPOSURE_EV);
    }

    #[test]
    fn edit_without_curve_fields_is_identity_curve() {
        // A manifest that predates the tone curve (or one written by this same
        // schema without the curve on a bare exposure edit) must load the curve
        // as identity, so exposure-only edits never pick up a phantom curve.
        let dir = temp_dir("bare-curve");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            manifest_path(&dir),
            "version = 1\n\n[edits.\"a.DNG\"]\nexposure_ev = 0.75\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        let tone = loaded.tone("a.DNG");
        assert_eq!(tone.exposure_ev, 0.75);
        assert_eq!(tone.curve_contrast, DEFAULT_CURVE_CONTRAST);
        assert_eq!(tone.curve_rolloff, DEFAULT_CURVE_ROLLOFF);
        assert_eq!(tone.curve_shadows, DEFAULT_CURVE_SHADOWS);
    }

    #[test]
    fn curve_fields_round_trip() {
        let dir = temp_dir("curve-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_exposure("IMG_0001.DNG", 0.42);
        manifest.set_curve("IMG_0001.DNG", 0.8, 1.2, 1.05);

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        let tone = loaded.tone("IMG_0001.DNG");
        assert_eq!(tone.curve_contrast, 0.8);
        assert_eq!(tone.curve_rolloff, 1.2);
        assert_eq!(tone.curve_shadows, 1.05);
        // Exposure survives alongside the curve on the same edit.
        assert_eq!(tone.exposure_ev, 0.42);
    }

    #[test]
    fn set_curve_updates_in_place() {
        let mut manifest = RollManifest::default();
        manifest.set_curve("a.DNG", 0.9, 1.1, 1.0);
        manifest.set_curve("a.DNG", 1.0, 1.0, 1.0);

        assert_eq!(manifest.edits.len(), 1);
        let tone = manifest.tone("a.DNG");
        assert_eq!(tone.curve_contrast, 1.0);
        assert_eq!(tone.curve_rolloff, 1.0);
        assert_eq!(tone.curve_shadows, 1.0);
    }

    #[test]
    fn curve_and_exposure_coexist_on_one_edit() {
        // A file edited through both APIs keeps all values on one entry, and
        // a bare exposure edit never disturbs the curve defaults (or vice
        // versa).
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", 0.3);
        assert_eq!(manifest.tone("a.DNG").curve_contrast, 1.0);

        manifest.set_curve("a.DNG", 1.2, 0.9, 1.1);
        let tone = manifest.tone("a.DNG");
        assert_eq!(tone.exposure_ev, 0.3);
        assert_eq!(manifest.edits.len(), 1);
    }

    #[test]
    fn tone_merges_exposure_and_curve() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", -0.5);
        manifest.set_curve("a.DNG", 1.2, 0.8, 0.9);

        let tone = manifest.tone("a.DNG");
        assert_eq!(tone.exposure_ev, -0.5);
        assert_eq!(tone.curve_contrast, 1.2);
        assert_eq!(tone.curve_rolloff, 0.8);
        assert_eq!(tone.curve_shadows, 0.9);
    }

    #[test]
    fn set_tone_replaces_the_full_edit_in_one_step() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", 0.3);
        manifest.set_curve("a.DNG", 1.2, 0.9, 1.1);

        // A copy/paste replaces every field at once.
        manifest.set_tone(
            "a.DNG",
            ToneEdit {
                exposure_ev: -1.2,
                curve_contrast: 0.7,
                curve_rolloff: 1.4,
                curve_shadows: 1.3,
            },
        );

        let tone = manifest.tone("a.DNG");
        assert_eq!(tone.exposure_ev, -1.2);
        assert_eq!(tone.curve_contrast, 0.7);
        assert_eq!(tone.curve_rolloff, 1.4);
        assert_eq!(tone.curve_shadows, 1.3);
        assert_eq!(manifest.edits.len(), 1);
    }

    #[test]
    fn set_tone_creates_an_entry_on_a_fresh_file() {
        let mut manifest = RollManifest::default();
        manifest.set_tone(
            "b.DNG",
            ToneEdit {
                exposure_ev: 0.4,
                curve_contrast: 1.1,
                curve_rolloff: 0.9,
                curve_shadows: 1.0,
            },
        );
        assert_eq!(manifest.tone("b.DNG").exposure_ev, 0.4);
    }

    #[test]
    fn curve_of_unknown_file_is_identity() {
        let manifest = RollManifest::default();
        assert_eq!(manifest.tone("missing.DNG"), ToneEdit::identity());
    }

    #[test]
    fn reconcile_drops_stale_entries() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("gone.DNG", 0.3);
        manifest.set_exposure("kept.DNG", -0.4);

        reconcile(&mut manifest, &["kept.DNG".to_owned()]);

        assert_eq!(manifest.tone("gone.DNG").exposure_ev, DEFAULT_EXPOSURE_EV);
        assert_eq!(manifest.tone("kept.DNG").exposure_ev, -0.4);
        assert!(!manifest.edits.contains_key("gone.DNG"));
    }

    #[test]
    fn set_exposure_updates_in_place() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", 0.5);
        manifest.set_exposure("a.DNG", -1.25);

        assert_eq!(manifest.edits.len(), 1);
        assert_eq!(manifest.tone("a.DNG").exposure_ev, -1.25);
    }

    #[test]
    fn crop_round_trips_and_defaults_to_zero() {
        let dir = temp_dir("crop-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        let crop = CropMargins {
            top: 10,
            right: 20,
            bottom: 30,
            left: 40,
        };
        manifest.set_crop("IMG_0001.DNG", crop);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        assert_eq!(loaded.crop("IMG_0001.DNG"), crop);
        // An untouched file has no crop.
        assert_eq!(loaded.crop("IMG_0002.DNG"), CropMargins::default());
    }

    #[test]
    fn legacy_manifest_without_crop_loads_zero_margins() {
        let dir = temp_dir("legacy-crop");
        std::fs::create_dir_all(&dir).unwrap();
        // A v1 manifest scripted before the crop field ever existed.
        std::fs::write(
            manifest_path(&dir),
            "version = 1\n\n[edits.\"a.DNG\"]\nexposure_ev = 0.75\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.crop("a.DNG"), CropMargins::default());
        // And the surviving tone fields still load.
        assert_eq!(loaded.tone("a.DNG").exposure_ev, 0.75);
    }

    #[test]
    fn crop_coexists_with_tone_edits_on_one_entry() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", 0.3);
        manifest.set_curve("a.DNG", 1.2, 0.9, 1.1);
        let crop = CropMargins {
            top: 5,
            right: 6,
            bottom: 7,
            left: 8,
        };
        manifest.set_crop("a.DNG", crop);

        assert_eq!(manifest.edits.len(), 1, "all edits on one entry");
        assert_eq!(manifest.crop("a.DNG"), crop);
        let tone = manifest.tone("a.DNG");
        assert_eq!(tone.exposure_ev, 0.3);
        assert_eq!(tone.curve_contrast, 1.2);
    }

    #[test]
    fn rotation_round_trips_and_defaults_to_zero() {
        let dir = temp_dir("rotation-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_rotation("IMG_0001.DNG", 2);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        assert_eq!(loaded.rotation("IMG_0001.DNG"), 2);
        // An untouched file has no user rotation.
        assert_eq!(loaded.rotation("IMG_0002.DNG"), 0);
    }

    #[test]
    fn legacy_manifest_without_rotation_loads_zero() {
        let dir = temp_dir("legacy-rotation");
        std::fs::create_dir_all(&dir).unwrap();
        // A v1 manifest scripted before the rotation field ever existed.
        std::fs::write(
            manifest_path(&dir),
            "version = 4\n\n[edits.\"a.DNG\"]\nexposure_ev = 0.75\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.rotation("a.DNG"), 0);
        // And the surviving tone fields still load.
        assert_eq!(loaded.tone("a.DNG").exposure_ev, 0.75);
    }

    #[test]
    fn rotation_coexists_with_tone_and_crop_on_one_entry() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", 0.3);
        manifest.set_curve("a.DNG", 1.2, 0.9, 1.1);
        let crop = CropMargins {
            top: 5,
            right: 6,
            bottom: 7,
            left: 8,
        };
        manifest.set_crop("a.DNG", crop);
        manifest.set_rotation("a.DNG", 3);

        assert_eq!(manifest.edits.len(), 1, "all edits on one entry");
        assert_eq!(manifest.rotation("a.DNG"), 3);
        assert_eq!(manifest.crop("a.DNG"), crop);
        let tone = manifest.tone("a.DNG");
        assert_eq!(tone.exposure_ev, 0.3);
        assert_eq!(tone.curve_contrast, 1.2);
    }

    #[test]
    fn set_rotation_updates_in_place_and_creates_on_fresh_file() {
        let mut manifest = RollManifest::default();
        manifest.set_rotation("a.DNG", 0);
        assert_eq!(manifest.rotation("a.DNG"), 0);

        manifest.set_rotation("a.DNG", 1);
        manifest.set_rotation("a.DNG", 2);
        assert_eq!(manifest.edits.len(), 1);
        assert_eq!(manifest.rotation("a.DNG"), 2);
        // The fresh-file branch keeps the tone identities.
        assert_eq!(manifest.tone("a.DNG"), ToneEdit::identity());
    }

    #[test]
    fn paste_does_not_clobber_an_existing_crop_or_rotation() {
        let mut manifest = RollManifest::default();
        let crop = CropMargins {
            top: 5,
            right: 6,
            bottom: 7,
            left: 8,
        };
        manifest.set_crop("a.DNG", crop);
        manifest.set_rotation("a.DNG", 3);
        manifest.set_exposure("a.DNG", 0.3);

        // Copy/paste writes a fresh ToneEdit onto the same file; the crop and
        // the rotation must survive untouched (neither is part of the
        // copy/paste payload).
        manifest.set_tone(
            "a.DNG",
            ToneEdit {
                exposure_ev: -1.2,
                curve_contrast: 0.7,
                curve_rolloff: 1.4,
                curve_shadows: 1.3,
            },
        );

        assert_eq!(manifest.crop("a.DNG"), crop, "paste keeps the target's crop");
        assert_eq!(manifest.rotation("a.DNG"), 3, "paste keeps the target's rotation");
        assert_eq!(manifest.tone("a.DNG").exposure_ev, -1.2);
    }

    #[test]
    fn set_crop_updates_in_place_and_creates_on_fresh_file() {
        let mut manifest = RollManifest::default();
        manifest.set_crop("a.DNG", CropMargins::default());
        assert_eq!(manifest.crop("a.DNG"), CropMargins::default());

        let crop = CropMargins {
            top: 2,
            right: 2,
            bottom: 2,
            left: 2,
        };
        manifest.set_crop("a.DNG", crop);
        assert_eq!(manifest.edits.len(), 1);
        assert_eq!(manifest.crop("a.DNG"), crop);
    }

    #[test]
    fn preset_absent_loads_default_none() {
        // A manifest predating the preset field (or a default roll) renders as
        // a non-inverted scan: nothing recorded → FilmPreset::None.
        let mut manifest = RollManifest::default();
        assert_eq!(manifest.preset(), FilmPreset::None);

        manifest.set_exposure("a.DNG", 0.5);
        assert_eq!(manifest.preset(), FilmPreset::None);

        let dir = temp_dir("preset-absent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(manifest_path(&dir), "version = 5\n").unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::None);
    }

    #[test]
    fn preset_round_trips_non_default_and_coexists_with_edits() {
        let dir = temp_dir("preset-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_preset(FilmPreset::Hp5Plus);
        manifest.set_exposure("IMG_0001.DNG", 0.42);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::Hp5Plus);
        assert_eq!(loaded.tone("IMG_0001.DNG").exposure_ev, 0.42);
    }

    #[test]
    fn set_preset_default_clears_the_stored_key() {
        let mut manifest = RollManifest::default();
        manifest.set_preset(FilmPreset::Hp5Plus);
        assert_eq!(manifest.preset(), FilmPreset::Hp5Plus);
        assert_eq!(manifest.preset.as_deref(), Some("hp5"));

        // Switching back to the default clears the key so a save omits it.
        manifest.set_preset(FilmPreset::None);
        assert_eq!(manifest.preset(), FilmPreset::None);
        assert_eq!(manifest.preset, None);
    }

    #[test]
    fn unknown_preset_key_resolves_to_default() {
        let dir = temp_dir("preset-unknown");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(manifest_path(&dir), "version = 5\npreset = \"delta\"\n").unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::None);
    }

    #[test]
    fn preset_key_string_loads_verbatim() {
        let dir = temp_dir("preset-key");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(manifest_path(&dir), "version = 5\npreset = \"hp5\"\n").unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::Hp5Plus);
    }

    #[test]
    fn calibrated_base_round_trips() {
        let dir = temp_dir("base-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_calibrated_base(0.71);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        assert_eq!(loaded.calibrated_base(), Some(0.71));
    }

    #[test]
    fn preset_first_default_has_no_base_recording() {
        // Preset-first: a fresh roll records neither a calibration frame nor a
        // base.
        let mut manifest = RollManifest::default();
        assert_eq!(manifest.calibrated_base(), None);
        assert_eq!(manifest.calibration_frame(), None);
    }

    #[test]
    fn calibration_frame_round_trips_with_its_base() {
        let dir = temp_dir("calib-frame");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_preset(FilmPreset::AutoSelectedFrame);
        manifest.set_calibration_frame("IMG_0007.DNG");
        manifest.set_calibrated_base(0.63);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::AutoSelectedFrame);
        assert_eq!(loaded.calibration_frame(), Some("IMG_0007.DNG"));
        assert_eq!(loaded.calibrated_base(), Some(0.63));
    }

    #[test]
    fn clear_calibration_drops_frame_and_base() {
        let mut manifest = RollManifest::default();
        manifest.set_calibration_frame("IMG_0007.DNG");
        manifest.set_calibrated_base(0.63);
        manifest.clear_calibration();
        assert_eq!(manifest.calibration_frame(), None);
        assert_eq!(manifest.calibrated_base(), None);
    }

    #[test]
    fn legacy_manifest_without_base_loads_preset_default() {
        let dir = temp_dir("legacy-base");
        std::fs::create_dir_all(&dir).unwrap();
        // A v5 manifest predating the base fields.
        std::fs::write(
            manifest_path(&dir),
            "version = 5\n\n[edits.\"a.DNG\"]\nexposure_ev = 0.75\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.version, 5, "older schema versions load verbatim");
        assert_eq!(loaded.calibrated_base(), None);
        assert_eq!(loaded.calibration_frame(), None);
        assert_eq!(loaded.tone("a.DNG").exposure_ev, 0.75);
    }

    #[test]
    fn legacy_base_auto_migrates_to_the_auto_per_frame_preset() {
        let dir = temp_dir("legacy-auto");
        std::fs::create_dir_all(&dir).unwrap();
        // A v7 manifest with the pre-v8 per-frame auto opt-in.
        std::fs::write(
            manifest_path(&dir),
            "version = 7\npreset = \"hp5\"\nbase_auto = true\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::AutoPerFrame);
        assert_eq!(loaded.preset.as_deref(), Some("auto-per-frame"));
    }

    #[test]
    fn legacy_manifest_without_auto_keeps_its_preset() {
        let dir = temp_dir("legacy-no-auto");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            manifest_path(&dir),
            "version = 7\npreset = \"hp5\"\nbase = 0.71\n",
        )
        .unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::Hp5Plus);
        assert_eq!(loaded.calibrated_base(), Some(0.71));
    }

    #[test]
    fn auto_preset_keys_round_trip() {
        let dir = temp_dir("auto-keys");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_preset(FilmPreset::AutoPerFrame);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::AutoPerFrame);
        assert_eq!(loaded.preset.as_deref(), Some("auto-per-frame"));
    }

    #[test]
    fn base_config_maps_the_preset_strategy() {
        // The base strategy is part of the preset choice: stock presets resolve
        // preset-first, per-frame auto measures every frame, and the
        // auto-selected-frame preset pins the roll to its measured base.
        let mut manifest = RollManifest::default();
        assert_eq!(
            manifest.base_config(),
            crate::film::BaseConfig {
                calibrated: None,
                auto: false
            }
        );

        manifest.set_preset(FilmPreset::AutoPerFrame);
        assert_eq!(
            manifest.base_config(),
            crate::film::BaseConfig {
                calibrated: None,
                auto: true
            }
        );

        manifest.set_preset(FilmPreset::AutoSelectedFrame);
        manifest.set_calibrated_base(0.63);
        assert_eq!(
            manifest.base_config(),
            crate::film::BaseConfig {
                calibrated: Some(0.63),
                auto: false
            }
        );

        manifest.set_preset(FilmPreset::Hp5Plus);
        assert_eq!(
            manifest.base_config(),
            crate::film::BaseConfig {
                calibrated: None,
                auto: false
            }
        );
    }

    #[test]
    fn base_calibration_coexists_with_preset_and_edits() {
        let dir = temp_dir("base-coexist");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_preset(FilmPreset::AutoSelectedFrame);
        manifest.set_calibrated_base(0.7);
        manifest.set_exposure("IMG_0001.DNG", 0.42);

        save_roll_manifest(&dir, &manifest).unwrap();
        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.preset(), FilmPreset::AutoSelectedFrame);
        assert_eq!(loaded.calibrated_base(), Some(0.7));
        assert_eq!(loaded.tone("IMG_0001.DNG").exposure_ev, 0.42);
    }
}
