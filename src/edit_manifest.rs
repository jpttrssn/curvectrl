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
}

impl Default for EditData {
    /// A fresh, un-edited entry: zero exposure, identity tone curve.
    fn default() -> Self {
        Self {
            exposure_ev: DEFAULT_EXPOSURE_EV,
            curve_contrast: DEFAULT_CURVE_CONTRAST,
            curve_rolloff: DEFAULT_CURVE_ROLLOFF,
        }
    }
}

/// `#[serde(default)]` target so a legacy manifest entry without the curve
/// fields loads as the identity curve.
#[allow(clippy::unnecessary_wraps)]
fn default_curve_identity() -> f32 {
    1.0
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
            version: 2,
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

    /// The stored exposure for `name`, or [`DEFAULT_EXPOSURE_EV`] when the
    /// file carries no edit.
    #[must_use]
    pub fn exposure_ev(&self, name: &str) -> f32 {
        self.edits
            .get(name)
            .map_or(DEFAULT_EXPOSURE_EV, |edit| edit.exposure_ev)
    }

    /// Records the tone curve for `name` (contrast + highlight rolloff),
    /// updating an existing entry in place. `(1.0, 1.0)` is the identity.
    ///
    /// RAM-only: the caller flushes to disk via [`save_roll_manifest`].
    pub fn set_curve(&mut self, name: &str, contrast: f32, rolloff: f32) {
        if let Some(edit) = self.edits.get_mut(name) {
            edit.curve_contrast = contrast;
            edit.curve_rolloff = rolloff;
        } else {
            self.edits.insert(
                name.to_owned(),
                EditData {
                    exposure_ev: DEFAULT_EXPOSURE_EV,
                    curve_contrast: contrast,
                    curve_rolloff: rolloff,
                },
            );
        }
    }

    /// The stored tone curve for `name`, or the identity `(1.0, 1.0)` when the
    /// file carries no edit (or a legacy manifest that predates the curve).
    #[must_use]
    pub fn curve(&self, name: &str) -> (f32, f32) {
        self.edits.get(name).map_or_else(
            || (DEFAULT_CURVE_CONTRAST, DEFAULT_CURVE_ROLLOFF),
            |edit| (edit.curve_contrast, edit.curve_rolloff),
        )
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
        manifest.name = Some("Berlin".to_owned());
        manifest.set_exposure("IMG_0001.DNG", 0.42);
        manifest.set_exposure("IMG_0002.RAW", -0.75);
        manifest.set_curve("IMG_0001.DNG", 0.85, 1.15);

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        assert_eq!(loaded.exposure_ev("IMG_0001.DNG"), 0.42);
        assert_eq!(loaded.exposure_ev("IMG_0002.RAW"), -0.75);
        assert_eq!(loaded.curve("IMG_0001.DNG"), (0.85, 1.15));
        // The second image carried no curve → identity.
        assert_eq!(loaded.curve("IMG_0002.RAW"), (1.0, 1.0));
    }

    #[test]
    fn missing_manifest_is_default() {
        let dir = temp_dir("missing");
        std::fs::create_dir_all(&dir).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, RollManifest::default());
        assert_eq!(loaded.version, 2);
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

        assert_eq!(loaded.exposure_ev("Rolle 1 IMG_10.DNG"), 0.3);
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

        assert_eq!(loaded.exposure_ev("a.DNG"), 1.5);
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

        assert_eq!(loaded.exposure_ev("a.DNG"), DEFAULT_EXPOSURE_EV);
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

        assert_eq!(loaded.exposure_ev("a.DNG"), 0.75);
        assert_eq!(loaded.curve("a.DNG"), (DEFAULT_CURVE_CONTRAST, DEFAULT_CURVE_ROLLOFF));
    }

    #[test]
    fn curve_fields_round_trip() {
        let dir = temp_dir("curve-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = RollManifest::default();
        manifest.set_exposure("IMG_0001.DNG", 0.42);
        manifest.set_curve("IMG_0001.DNG", 0.8, 1.2);

        save_roll_manifest(&dir, &manifest).unwrap();

        let loaded = load_roll_manifest(&dir);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded, manifest);
        assert_eq!(loaded.curve("IMG_0001.DNG"), (0.8, 1.2));
        // Exposure survives alongside the curve on the same edit.
        assert_eq!(loaded.exposure_ev("IMG_0001.DNG"), 0.42);
    }

    #[test]
    fn set_curve_updates_in_place() {
        let mut manifest = RollManifest::default();
        manifest.set_curve("a.DNG", 0.9, 1.1);
        manifest.set_curve("a.DNG", 1.0, 1.0);

        assert_eq!(manifest.edits.len(), 1);
        assert_eq!(manifest.curve("a.DNG"), (1.0, 1.0));
    }

    #[test]
    fn curve_of_unknown_file_is_identity() {
        let manifest = RollManifest::default();
        assert_eq!(manifest.curve("missing.DNG"), (1.0, 1.0));
    }

    #[test]
    fn reconcile_drops_stale_entries() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("gone.DNG", 0.3);
        manifest.set_exposure("kept.DNG", -0.4);

        reconcile(&mut manifest, &["kept.DNG".to_owned()]);

        assert_eq!(manifest.exposure_ev("gone.DNG"), DEFAULT_EXPOSURE_EV);
        assert_eq!(manifest.exposure_ev("kept.DNG"), -0.4);
        assert!(!manifest.edits.contains_key("gone.DNG"));
    }

    #[test]
    fn set_exposure_updates_in_place() {
        let mut manifest = RollManifest::default();
        manifest.set_exposure("a.DNG", 0.5);
        manifest.set_exposure("a.DNG", -1.25);

        assert_eq!(manifest.edits.len(), 1);
        assert_eq!(manifest.exposure_ev("a.DNG"), -1.25);
    }
}