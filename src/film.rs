// SPDX-License-Identifier: GPL-3.0-or-later

//! Film stock profiles and monochrome negative inversion.

/// A developed monochrome film stock's scan-response profile.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MonoStock {
    /// Display name of the stock.
    // Unused until a stock picker exists.
    #[allow(dead_code)]
    pub name: &'static str,
    /// Scanner-linear transmission of unexposed film base + fog; the positive's
    /// black point (the clearest film areas). A preset fallback: a roll may
    /// override it with a calibrated reference or per-frame measurement (see
    /// [`resolve_base`]).
    pub base: f32,
    /// Usable density range of the film above the base; the densest useful
    /// negative area maps to the positive's white point.
    pub d_max: f32,
    /// Tone-curve exponent applied to normalized density; below 1 lifts
    /// shadows.
    pub gamma: f32,
}

/// The stock assumed for all thumbnails during the inversion proof of concept.
///
/// `base` was calibrated against real HP5+ scans (brightest plateau across
/// horizontal bands of `_MG_0828.CR2`, see NOTES.md); `d_max`/`gamma` start
/// from published HP5+ curve values pending visual tuning.
pub const ACTIVE_STOCK: MonoStock = MonoStock {
    name: "Ilford HP5+",
    base: 0.82,
    d_max: 2.4,
    gamma: 0.7,
};

/// The film-inversion preset a roll's frames are rendered with: which
/// [`MonoStock`] profile inverts the negatives, or `None` for already-positive
/// scans (regular RAWs) that must NOT be inverted.
///
/// The default is [`FilmPreset::None`], so a roll with no recorded preset
/// renders as a regular (non-inverted) scan.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum FilmPreset {
    /// No inversion: the scan is treated as an already-positive image (a
    /// regular RAW). The pipeline normalizes it in place of inverting a
    /// negative.
    #[default]
    None,
    /// Invert with the active stock profile.
    Hp5Plus,
}

impl FilmPreset {
    /// The inversion profile for a chosen preset: [`None`] (no inversion)
    /// carries no profile; the negative preset carries [`ACTIVE_STOCK`].
    #[must_use]
    pub const fn stock(self) -> Option<MonoStock> {
        match self {
            Self::None => None,
            Self::Hp5Plus => Some(ACTIVE_STOCK),
        }
    }

    /// Whether this preset marks the scan as a negative to be density-inverted
    /// (`None` = already-positive scan, no inversion).
    ///
    /// The pipeline consumes this: an inverted preset applies the exposure gain
    /// to the true sensor-linear data BEFORE the inversion and flips the EV
    /// sign at that point, so the user-facing controls (+EV = brighter) stay
    /// identical across presets.
    #[must_use]
    pub const fn is_inverted(self) -> bool {
        self.stock().is_some()
    }

    /// The stable dialog and manifest storage key for this preset. `None` is
    /// the zero-value default, represented by the key's absence; only a
    /// non-default preset is ever written.
    #[must_use]
    pub const fn choice_key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hp5Plus => "hp5",
        }
    }

    /// Resolves a stored choice key into a preset; the missing/absent key and
    /// unknown values fall back to the default [`FilmPreset::None`].
    #[must_use]
    pub fn from_key(key: &str) -> Self {
        match key {
            "hp5" => Self::Hp5Plus,
            _ => Self::None,
        }
    }

    /// The dropdown index ordering (None first, matching the default).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::None => 0,
            Self::Hp5Plus => 1,
        }
    }

    /// Resolves a dropdown index (see [`Self::index`]) back into a preset;
    /// any out-of-range index falls back to the default [`FilmPreset::None`].
    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Hp5Plus,
            _ => Self::None,
        }
    }
}

/// Samples at or below this transmission encode maximum density.
const MIN_TRANSMISSION: f32 = 1e-6;

/// Measured channel bases below this are implausible — they indicate frames
/// without any measurable clear film — and fall back to [`MonoStock::base`].
pub const MIN_PLAUSIBLE_BASE: f32 = 0.1;

/// Maps one scanned transmission to its positive tone value, anchored on its
/// channel's clear-film transmission: optical density relative to that anchor,
/// positioned within the stock's usable density range, then run through the
/// contrast curve.
///
/// `base` is the clear-film transmission ([`MonoStock::base`] or a per-frame
/// measurement); `value == base` prints black, the densest useful area prints
/// white.
pub fn invert_value(transmission: f32, base: f32, stock: &MonoStock) -> f32 {
    let value = transmission.clamp(MIN_TRANSMISSION, base);
    // Density relative to the clear-film anchor.
    let density = f32::log10(base / value);
    // Position within the film's usable density range, mapped through a
    // contrast curve onto the full positive range: the clearest film
    // areas print black, the densest useful areas print white.
    let position = (density / stock.d_max).clamp(0.0, 1.0);

    position.powf(stock.gamma)
}

/// Inverts interleaved linear RGB scanned from a monochrome negative into a
/// positive, working in optical-density space.
///
/// `bases` holds each channel's clear-film transmission, anchoring the black
/// point per channel so capture casts (light table, sensor response) are
/// neutralized.
///
/// Unused while scans collapse to luminance before inversion; kept with its
/// tests as the per-channel path for future color stocks.
#[allow(dead_code)]
pub fn invert_mono(rgb: &mut [f32], stock: &MonoStock, bases: [f32; 3]) {
    for pixel in rgb.as_chunks_mut::<3>().0 {
        for (slot, base) in pixel.iter_mut().zip(bases) {
            *slot = invert_value(*slot, base, stock);
        }
    }
}

/// Inverts linear samples scanned from a monochrome negative into a positive,
/// working in optical-density space.
///
/// `base` is the scan's clear-film transmission and anchors the black point;
/// collapsing the capture to luminance beforehand removes any cast between
/// channels outright.
pub fn invert_gray(samples: &mut [f32], stock: &MonoStock, base: f32) {
    for slot in samples {
        *slot = invert_value(*slot, base, stock);
    }
}

/// Estimates the base+fog transmission from normalized linear samples of an
/// exposure showing only clear film, e.g. a photographed blank leader. Takes a
/// high percentile so dust and sensor outliers are rejected.
///
/// Returns `None` for inputs without finite samples.
pub fn measure_base(samples: &[f32]) -> Option<f32> {
    let mut sorted: Vec<f32> = samples
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();

    if sorted.is_empty() {
        return None;
    }

    sorted.sort_by(f32::total_cmp);
    Some(sorted[sorted.len() * 95 / 100])
}

/// The roll-level base-resolution state threaded from the manifest into every
/// decode and bake, so the detail shader, grid thumbnails, and exports cannot
/// drift the film's black point between renderings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BaseConfig {
    /// An explicit per-roll calibration measured once from a blank/leader
    /// frame held in the roll manifest. Wins over everything else.
    pub calibrated: Option<f32>,
    /// Opt-in per-frame auto measurement of the clear-film anchor. When off,
    /// the frame's own `measure_base` result is never computed (sorting a full
    /// buffer is wasted work).
    pub auto: bool,
}

impl BaseConfig {
    /// Resolves the tonally-effective black point for one frame of `stock`.
    ///
    /// `measured` is the frame's own `measure_base` result — pass it only when
    /// [`BaseConfig::auto`] is on; the caller must not compute it otherwise.
    #[must_use]
    pub fn resolve(self, measured: Option<f32>, stock: &MonoStock) -> f32 {
        resolve_base(self.calibrated, self.auto, measured, stock)
    }
}

/// Resolves the tonally-effective black point for one frame from the roll's
/// calibration state, preset-first:
///
/// 1. An explicit per-roll calibration wins outright (a blank/leader frame
///    photographed once, stored in the roll manifest).
/// 2. Otherwise, the per-frame auto measurement — an explicit opt-in — refines
///    the stock's preset base, skipping it when it is implausible or absent.
/// 3. Otherwise the stock's preset base is the truth.
///
/// `measured` is the frame's own `measure_base` result when `auto` is on;
/// callers must not compute it otherwise (sorting a full buffer is wasted).
///
/// Kept pure + unit-tested so the detail decode, the thumbnail bake, and the
/// export path cannot drift the black point between renderings.
#[must_use]
pub fn resolve_base(
    calibrated: Option<f32>,
    auto: bool,
    measured: Option<f32>,
    stock: &MonoStock,
) -> f32 {
    if let Some(base) = calibrated {
        base
    } else if auto {
        measured
            .filter(|value| *value >= MIN_PLAUSIBLE_BASE)
            .unwrap_or(stock.base)
    } else {
        stock.base
    }
}

/// Measures each RGB channel's clear-film transmission from interleaved linear
/// pixels, neutralizing capture casts by anchoring every channel on its own
/// base plateau (typically found in frame gaps and margins).
///
/// Channels measuring below [`MIN_PLAUSIBLE_BASE`] fall back to the active
/// stock's constant; returns `None` only when no channel has finite samples.
///
/// Unused alongside [`invert_mono`]; kept for future color stocks.
#[allow(dead_code)]
pub fn measure_base_channels(rgb: &[f32]) -> Option<[f32; 3]> {
    let mut channels: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for [r, g, b] in rgb.as_chunks::<3>().0 {
        channels[0].push(*r);
        channels[1].push(*g);
        channels[2].push(*b);
    }

    let mut bases = [0.0_f32; 3];
    for (channel, slot) in channels.iter().zip(bases.iter_mut()) {
        *slot = measure_base(channel)?;
    }

    for slot in &mut bases {
        if *slot < MIN_PLAUSIBLE_BASE {
            *slot = ACTIVE_STOCK.base;
        }
    }

    Some(bases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inversion_maps_base_and_dmax_to_endpoints() {
        // White point sits at the film's usable density limit above base.
        let white_point_input = ACTIVE_STOCK.base * 10.0_f32.powf(-ACTIVE_STOCK.d_max);

        // One gray pixel at each endpoint.
        let mut rgb = vec![ACTIVE_STOCK.base; 3];
        for _ in 0..3 {
            rgb.push(white_point_input);
        }

        invert_mono(&mut rgb, &ACTIVE_STOCK, [ACTIVE_STOCK.base; 3]);

        assert!(rgb[..3].iter().all(|value| value.abs() < 1e-5));
        assert!(rgb[3..].iter().all(|value| (*value - 1.0).abs() < 1e-5));
    }

    #[test]
    fn inversion_is_monotonic_between_endpoints() {
        let inputs = [0.05, 0.2, 0.5, ACTIVE_STOCK.base];
        let mut rgb = Vec::new();
        for value in inputs {
            for _ in 0..3 {
                rgb.push(value);
            }
        }

        invert_mono(&mut rgb, &ACTIVE_STOCK, [ACTIVE_STOCK.base; 3]);

        for pixel in 0..inputs.len() - 1 {
            // Brighter scans (closer to clear base) must print darker.
            assert!(rgb[pixel * 3] > rgb[(pixel + 1) * 3]);
        }
    }

    #[test]
    fn inversion_clamps_out_of_range_input() {
        let mut rgb = vec![1.5, 1.5, 1.5];
        for _ in 0..3 {
            rgb.push(MIN_TRANSMISSION / 2.0);
        }

        invert_mono(&mut rgb, &ACTIVE_STOCK, [ACTIVE_STOCK.base; 3]);

        assert!(rgb[..3].iter().all(|value| value.abs() < 1e-5));
        assert!(rgb[3..].iter().all(|value| (*value - 1.0).abs() < 1e-5));
    }

    #[test]
    fn measure_base_takes_a_high_percentile() {
        let mut samples = vec![0.9_f32; 100];
        samples[0] = 0.3;

        let base = measure_base(&samples).unwrap();

        assert!((base - 0.9).abs() < 1e-6);
    }

    #[test]
    fn measure_base_needs_finite_samples() {
        assert_eq!(measure_base(&[]), None);
        assert_eq!(measure_base(&[f32::NAN]), None);
    }

    #[test]
    fn resolve_base_prefers_calibration_over_auto_and_preset() {
        // A roll calibration is the truth regardless of auto or measured.
        assert_eq!(resolve_base(Some(0.71), false, Some(0.9), &ACTIVE_STOCK), 0.71);
        assert_eq!(resolve_base(Some(0.71), true, Some(0.9), &ACTIVE_STOCK), 0.71);
    }

    #[test]
    fn resolve_base_auto_uses_a_plausible_measurement() {
        assert_eq!(resolve_base(None, true, Some(0.88), &ACTIVE_STOCK), 0.88);
    }

    #[test]
    fn resolve_base_auto_falls_back_on_implausible_or_missing_measurement() {
        // A frame without measurable clear film (or an absent measurement)
        // falls back to the preset base in auto mode too.
        assert_eq!(resolve_base(None, true, Some(0.02), &ACTIVE_STOCK), ACTIVE_STOCK.base);
        assert_eq!(resolve_base(None, true, None, &ACTIVE_STOCK), ACTIVE_STOCK.base);
    }

    #[test]
    fn resolve_base_defaults_to_the_preset() {
        // Preset-first: without calibration or auto, even a frame measurement
        // is ignored — the stock's preset base is the truth.
        assert_eq!(resolve_base(None, false, Some(0.88), &ACTIVE_STOCK), ACTIVE_STOCK.base);
        assert_eq!(resolve_base(None, false, None, &ACTIVE_STOCK), ACTIVE_STOCK.base);
    }

    #[test]
    fn measure_base_channels_recovers_per_channel_bases() {
        // Clear film with uniform green excess, plus a dust outlier.
        let mut rgb = Vec::with_capacity(300);
        for _ in 0..100 {
            rgb.extend_from_slice(&[0.60, 0.66, 0.54]);
        }
        rgb[..3].copy_from_slice(&[0.2, 0.2, 0.2]);

        let bases = measure_base_channels(&rgb).unwrap();

        assert!((bases[0] - 0.60).abs() < 1e-6);
        assert!((bases[1] - 0.66).abs() < 1e-6);
        assert!((bases[2] - 0.54).abs() < 1e-6);
    }

    #[test]
    fn inversion_neutralizes_cast_against_measured_bases() {
        // Pixels sitting at their channel's own base must all land on neutral
        // black, regardless of the cast between channels.
        let mut rgb = vec![0.60, 0.66, 0.54];
        let bases = measure_base_channels(&rgb).unwrap();

        invert_mono(&mut rgb, &ACTIVE_STOCK, bases);

        assert!(rgb.iter().all(|value| value.abs() < 1e-5));
    }

    #[test]
    fn measure_base_channels_falls_back_on_implausible_channels() {
        // Blue has no measurable clear film in this frame.
        let mut rgb = vec![0.6_f32; 90];
        for [_, _, b] in rgb.as_chunks_mut::<3>().0 {
            *b = 0.001;
        }

        let bases = measure_base_channels(&rgb).unwrap();

        assert!((bases[0] - 0.6).abs() < 1e-6);
        assert!((bases[1] - 0.6).abs() < 1e-6);
        assert_eq!(bases[2], ACTIVE_STOCK.base);
    }

    #[test]
    fn inversion_gamma_below_one_lifts_midtones() {
        let mut lifted = vec![0.5_f32; 3];
        invert_mono(&mut lifted, &ACTIVE_STOCK, [ACTIVE_STOCK.base; 3]);

        let linear = MonoStock {
            name: "linear",
            base: ACTIVE_STOCK.base,
            d_max: ACTIVE_STOCK.d_max,
            gamma: 1.0,
        };
        let mut neutral = vec![0.5_f32; 3];
        invert_mono(&mut neutral, &linear, [ACTIVE_STOCK.base; 3]);

        assert!(lifted[0] > neutral[0]);
    }

    #[test]
    fn inversion_gray_maps_base_and_dmax_to_endpoints() {
        // White point sits at the film's usable density limit above base.
        let white_point_input = ACTIVE_STOCK.base * 10.0_f32.powf(-ACTIVE_STOCK.d_max);
        let mut gray = vec![ACTIVE_STOCK.base, white_point_input];

        invert_gray(&mut gray, &ACTIVE_STOCK, ACTIVE_STOCK.base);

        assert!(gray[0].abs() < 1e-5);
        assert!((gray[1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn inversion_gray_is_monotonic_between_endpoints() {
        let inputs = [0.05, 0.2, 0.5, ACTIVE_STOCK.base];
        let mut gray = inputs.to_vec();

        invert_gray(&mut gray, &ACTIVE_STOCK, ACTIVE_STOCK.base);

        // Brighter scans (closer to clear base) must print darker.
        for window in gray.windows(2) {
            assert!(window[0] > window[1]);
        }
    }

    #[test]
    fn inversion_gray_matches_rgb_inversion_on_neutral_pixels() {
        let inputs = [0.05, 0.2, 0.5, ACTIVE_STOCK.base];
        let mut rgb = Vec::new();
        for value in inputs {
            rgb.extend_from_slice(&[value; 3]);
        }
        let mut gray = inputs.to_vec();

        invert_mono(&mut rgb, &ACTIVE_STOCK, [ACTIVE_STOCK.base; 3]);
        invert_gray(&mut gray, &ACTIVE_STOCK, ACTIVE_STOCK.base);

        for (pixel, expected) in rgb.as_chunks::<3>().0.iter().zip(&gray) {
            for value in pixel {
                assert!((value - expected).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn film_preset_default_is_none() {
        assert_eq!(FilmPreset::default(), FilmPreset::None);
    }

    #[test]
    fn film_preset_stock_mapping() {
        assert_eq!(FilmPreset::None.stock(), None);
        assert_eq!(FilmPreset::Hp5Plus.stock(), Some(ACTIVE_STOCK));
    }

    #[test]
    fn film_preset_inverted_marking() {
        assert!(!FilmPreset::None.is_inverted());
        assert!(FilmPreset::Hp5Plus.is_inverted());
    }

    #[test]
    fn film_preset_choice_key_round_trip() {
        for preset in [FilmPreset::None, FilmPreset::Hp5Plus] {
            assert_eq!(FilmPreset::from_key(preset.choice_key()), preset);
        }
        // Unknown keys resolve to the default (None), like a missing entry.
        assert_eq!(FilmPreset::from_key(""), FilmPreset::None);
        assert_eq!(FilmPreset::from_key("delta"), FilmPreset::None);
    }

    #[test]
    fn film_preset_index_round_trip() {
        for preset in [FilmPreset::None, FilmPreset::Hp5Plus] {
            assert_eq!(FilmPreset::from_index(preset.index()), preset);
        }
        assert_eq!(FilmPreset::from_index(99), FilmPreset::None);
    }
}
