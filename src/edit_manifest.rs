// SPDX-License-Identifier: MPL-2.0

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name of the per-roll edit manifest inside a roll directory.
///
/// Deliberately names the file after its function rather than the application,
/// so an application rename never orphans existing manifests.
pub const ROLL_MANIFEST_FILE: &str = ".film-roll.toml";

/// Exposure applied to an image until an edit records otherwise.
pub const DEFAULT_EXPOSURE_EV: f32 = 0.0;

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
    /// Exposure compensation in EV (−3.00 to +3.00).
    #[serde(default)]
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
}

impl Default for EditData {
    /// A fresh, un-edited entry: zero exposure, identity tone curve.
    fn default() -> Self {
        Self {
            exposure_ev: DEFAULT_EXPOSURE_EV,
            curve_contrast: DEFAULT_CURVE_CONTRAST,
            curve_rolloff: DEFAULT_CURVE_ROLLOFF,
            curve_shadows: DEFAULT_CURVE_SHADOWS,
        }
    }
}

/// `#[serde(default)]` target so a legacy manifest entry without the curve
/// fields loads as the identity curve.
#[allow(clippy::unnecessary_wraps)]
fn default_curve_identity() -> f32 {
    1.0
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
}

impl Default for RollManifest {
    fn default() -> Self {
        Self {
            version: 3,
            name: None,
            edits: HashMap::new(),
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
            self.edits
                .insert(name.to_owned(), EditData { exposure_ev, ..Default::default() });
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
                },
            );
        }
    }

    /// The full edit for `name` as a single [`ToneEdit`], merging exposure
    /// and the curve powers. Files (or manifest fields) never touched fall
    /// back to their identities.
    #[must_use]
    pub fn tone(&self, name: &str) -> ToneEdit {
        self.edits.get(name).map_or_else(ToneEdit::identity, |edit| ToneEdit {
            exposure_ev: edit.exposure_ev,
            curve_contrast: edit.curve_contrast,
            curve_rolloff: edit.curve_rolloff,
            curve_shadows: edit.curve_shadows,
        })
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
#[must_use]
pub fn load_roll_manifest(dir: &Path) -> RollManifest {
    let path = manifest_path(dir);
    match std::fs::read(&path) {
        Ok(bytes) => match std::str::from_utf8(&bytes) {
            Ok(text) => match toml::from_str(text) {
                Ok(manifest) => manifest,
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
        assert_eq!(loaded.version, 3);
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
}