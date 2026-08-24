// SPDX-License-Identifier: MPL-2.0

//! Film stock profiles and monochrome negative inversion.

/// A developed monochrome film stock's scan-response profile.
pub struct MonoStock {
    /// Display name of the stock.
    // Unused until a stock picker exists.
    #[allow(dead_code)]
    pub name: &'static str,
    /// Scanner-linear transmission of unexposed film base + fog; defines the
    /// positive's black point (the clearest film areas).
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

/// Samples at or below this transmission encode maximum density.
const MIN_TRANSMISSION: f32 = 1e-6;

/// Measured channel bases below this are implausible — they indicate frames
/// without any measurable clear film — and fall back to [`MonoStock::base`].
pub const MIN_PLAUSIBLE_BASE: f32 = 0.1;

/// Maps one scanned transmission to its positive tone value, anchored on its
/// channel's clear-film transmission: optical density relative to that anchor,
/// positioned within the stock's usable density range, then run through the
/// contrast curve.
fn positive(transmission: f32, base: f32, stock: &MonoStock) -> f32 {
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
            *slot = positive(*slot, base, stock);
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
        *slot = positive(*slot, base, stock);
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
}
