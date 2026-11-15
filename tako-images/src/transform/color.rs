//! Converts sources tagged with a non-sRGB color space to sRGB.
//!
//! Optimized output strips color metadata, so its pixels must already be
//! sRGB. Without conversion, wide-gamut sources such as Display P3 photos
//! from phones or Adobe RGB exports render washed out.

use super::{Layout, Raster};
use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, DataColorSpace, Layout as CmsLayout,
    MatrixCoefficients, RenderingIntent, TransferCharacteristics, TransformOptions,
};

/// Larger embedded profiles are ignored. Display profiles are a few
/// kilobytes; megabyte profiles are print LUTs that don't describe web images.
const MAX_ICC_BYTES: usize = 1024 * 1024;
/// Per-channel rounding a profile may introduce and still count as sRGB.
const IDENTITY_TOLERANCE: u8 = 1;

/// Parses an embedded ICC profile. Returns `None` for malformed profiles and
/// for non-RGB ones (CMYK and grayscale sources are already converted to RGB
/// by their decoders), so a bad profile never fails a transform.
pub(super) fn profile_from_icc(icc: &[u8]) -> Option<ColorProfile> {
    if icc.is_empty() || icc.len() > MAX_ICC_BYTES {
        return None;
    }
    ColorProfile::new_from_slice(icc)
        .ok()
        .filter(|profile| profile.color_space == DataColorSpace::Rgb)
}

/// Builds a profile from AVIF/HEIF CICP color tags when they describe a
/// non-sRGB gamut with an SDR transfer curve. sRGB-equivalent and HDR (PQ,
/// HLG) tags return `None`: the former need nothing, the latter would need
/// tone mapping.
pub(super) fn profile_from_cicp(primaries: u16, transfer: u16) -> Option<ColorProfile> {
    let primaries = CicpColorPrimaries::try_from(u8::try_from(primaries).ok()?).ok()?;
    if matches!(
        primaries,
        CicpColorPrimaries::Bt709 | CicpColorPrimaries::Unspecified | CicpColorPrimaries::Reserved
    ) {
        return None;
    }
    let transfer = match TransferCharacteristics::try_from(u8::try_from(transfer).ok()?).ok()? {
        // Unspecified SDR content is treated as sRGB-encoded, as browsers do.
        TransferCharacteristics::Unspecified => TransferCharacteristics::Srgb,
        sdr @ (TransferCharacteristics::Srgb
        | TransferCharacteristics::Bt709
        | TransferCharacteristics::Bt601
        | TransferCharacteristics::Bt202010bit
        | TransferCharacteristics::Bt202012bit) => sdr,
        _ => return None,
    };
    Some(ColorProfile::new_from_cicp(CicpProfile {
        color_primaries: primaries,
        transfer_characteristics: transfer,
        matrix_coefficients: MatrixCoefficients::Identity,
        full_range: true,
    }))
}

/// Converts every frame from `profile` to sRGB in place. Profiles that are
/// already sRGB, and profiles moxcms cannot build a transform for, leave the
/// pixels untouched.
pub(super) fn convert_to_srgb(frames: &mut [Raster], profile: &ColorProfile) {
    let Some(first) = frames.first() else {
        return;
    };
    let layout = first.layout;
    let cms_layout = match layout {
        Layout::Rgb => CmsLayout::Rgb,
        Layout::Rgba => CmsLayout::Rgba,
    };
    let options = TransformOptions {
        // Matches how browsers map tagged images onto sRGB displays.
        rendering_intent: RenderingIntent::RelativeColorimetric,
        ..TransformOptions::default()
    };
    let Ok(transform) =
        profile.create_transform_8bit(cms_layout, &ColorProfile::new_srgb(), cms_layout, options)
    else {
        return;
    };
    if is_identity(&*transform, layout) {
        return;
    }

    // Convert every frame before replacing any, so a failure can't leave an
    // animation half converted.
    let mut converted = Vec::with_capacity(frames.len());
    for frame in frames.iter() {
        if frame.layout != layout {
            return;
        }
        let mut data = vec![0; frame.data.len()];
        if transform.transform(&frame.data, &mut data).is_err() {
            return;
        }
        converted.push(data);
    }
    for (frame, data) in frames.iter_mut().zip(converted) {
        frame.data = data;
    }
}

/// Probes a 6x6x6 color grid: a transform that leaves every probe within
/// rounding is an sRGB profile (such as "sRGB IEC61966-2.1"), so skipping it
/// keeps those images byte-for-byte unchanged.
fn is_identity(transform: &moxcms::Transform8BitExecutor, layout: Layout) -> bool {
    const LEVELS: [u8; 6] = [0, 51, 102, 153, 204, 255];
    let channels = layout.channels();
    let mut probe = Vec::with_capacity(LEVELS.len().pow(3) * channels);
    for r in LEVELS {
        for g in LEVELS {
            for b in LEVELS {
                probe.extend_from_slice(&[r, g, b]);
                if layout == Layout::Rgba {
                    probe.push(255);
                }
            }
        }
    }
    let mut output = vec![0; probe.len()];
    if transform.transform(&probe, &mut output).is_err() {
        return false;
    }
    probe
        .iter()
        .zip(&output)
        .all(|(before, after)| before.abs_diff(*after) <= IDENTITY_TOLERANCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform(layout: Layout, pixel: &[u8]) -> Raster {
        let data = pixel.repeat(16);
        Raster::new(4, 4, layout, data).expect("raster")
    }

    #[test]
    fn leaves_srgb_profiles_untouched() {
        let srgb = ColorProfile::new_srgb().encode().expect("encode sRGB");
        let profile = profile_from_icc(&srgb).expect("parse sRGB");
        let mut frames = vec![uniform(Layout::Rgb, &[200, 100, 50])];
        let before = frames.clone();
        convert_to_srgb(&mut frames, &profile);
        assert_eq!(frames, before);
    }

    #[test]
    fn converts_display_p3_and_preserves_alpha() {
        let p3 = ColorProfile::new_display_p3().encode().expect("encode P3");
        let profile = profile_from_icc(&p3).expect("parse P3");
        let mut frames = vec![
            uniform(Layout::Rgba, &[200, 100, 50, 128]),
            uniform(Layout::Rgba, &[0, 255, 0, 255]),
        ];
        convert_to_srgb(&mut frames, &profile);
        // A saturated P3 orange is redder in sRGB: red rises, green and blue fall.
        let orange = &frames[0].data[..4];
        assert!(
            orange[0] > 200 && orange[1] < 100 && orange[2] < 50,
            "{orange:?}"
        );
        assert_eq!(orange[3], 128, "alpha must be preserved");
        // P3 green is outside sRGB and clips to full green.
        let green = &frames[1].data[..4];
        assert!(
            green[0] < 10 && green[1] > 245 && green[2] < 70,
            "{green:?}"
        );
        assert_eq!(green[3], 255);
    }

    #[test]
    fn ignores_malformed_cmyk_and_oversized_profiles() {
        assert!(profile_from_icc(b"not an icc profile").is_none());
        assert!(profile_from_icc(&[]).is_none());
        assert!(profile_from_icc(&vec![0; MAX_ICC_BYTES + 1]).is_none());
    }

    #[test]
    fn builds_profiles_only_for_wide_gamut_sdr_cicp() {
        // BT.709 / sRGB and unspecified primaries need no conversion.
        assert!(profile_from_cicp(1, 13).is_none());
        assert!(profile_from_cicp(2, 13).is_none());
        // Display P3 (SMPTE EG 432-1) with the sRGB transfer converts.
        assert!(profile_from_cicp(12, 13).is_some());
        // PQ and HLG would need tone mapping.
        assert!(profile_from_cicp(12, 16).is_none());
        assert!(profile_from_cicp(9, 18).is_none());
        assert!(profile_from_cicp(300, 13).is_none());
    }
}
