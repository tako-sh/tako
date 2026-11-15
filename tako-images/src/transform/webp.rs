//! Lossy WebP encoding through the statically linked, vendored libwebp.

use super::{Decoded, Layout, Raster};
use crate::ImageError;
use libwebp_sys as sys;
use std::os::raw::c_int;

/// libwebp effort, 0 (fast) to 6 (small). 4 is libwebp's and libvips' default.
const WEBP_METHOD: c_int = 4;
/// VP8X feature flag for "the canvas may contain transparency".
const VP8X_ALPHA_FLAG: u8 = 0x10;

pub(super) fn encode_still(raster: &Raster, quality: u8) -> Result<Vec<u8>, ImageError> {
    let config = lossy_config(quality)?;
    let mut picture = Picture::import(raster)?;
    let mut writer = MemoryWriter::new();
    // SAFETY: `picture` and `writer` are initialized and outlive the encode
    // call; the writer callback only appends to `writer.0`.
    unsafe {
        picture.0.writer = Some(sys::WebPMemoryWrite);
        picture.0.custom_ptr = (&mut writer.0 as *mut sys::WebPMemoryWriter).cast();
        if sys::WebPEncode(&config, &mut picture.0) == 0 {
            return Err(ImageError::TransformFailed);
        }
    }
    Ok(writer.to_vec())
}

pub(super) fn encode_animation(image: &Decoded, quality: u8) -> Result<Vec<u8>, ImageError> {
    let first = image.first()?;
    let (width, height) = (first.width, first.height);
    let config = lossy_config(quality)?;

    let mut options = std::mem::MaybeUninit::<sys::WebPAnimEncoderOptions>::uninit();
    // SAFETY: the init function fully initializes `options` when it succeeds.
    let mut options = unsafe {
        if sys::WebPAnimEncoderOptionsInitInternal(
            options.as_mut_ptr(),
            sys::WEBP_MUX_ABI_VERSION as c_int,
        ) == 0
        {
            return Err(ImageError::TransformFailed);
        }
        options.assume_init()
    };
    options.anim_params.loop_count = c_int::from(image.loop_count);
    // Fully transparent background, so uncovered canvas stays transparent.
    options.anim_params.bgcolor = 0;

    let encoder = AnimEncoder::new(width, height, &options)?;
    let mut timestamp_ms: c_int = 0;
    for (frame, delay) in image.frames.iter().zip(&image.delays_ms) {
        let mut picture = Picture::import(frame)?;
        // SAFETY: `encoder.0` is live and `picture` is a valid ARGB picture;
        // libwebp copies the pixels it needs before returning.
        if unsafe { sys::WebPAnimEncoderAdd(encoder.0, &mut picture.0, timestamp_ms, &config) } == 0
        {
            return Err(ImageError::TransformFailed);
        }
        let delay = c_int::try_from(*delay).map_err(|_| ImageError::TransformFailed)?;
        timestamp_ms = timestamp_ms
            .checked_add(delay)
            .ok_or(ImageError::TransformFailed)?;
    }
    // A final null frame records the duration of the last real frame.
    // SAFETY: a null frame is the documented end-of-stream marker.
    if unsafe {
        sys::WebPAnimEncoderAdd(
            encoder.0,
            std::ptr::null_mut(),
            timestamp_ms,
            std::ptr::null(),
        )
    } == 0
    {
        return Err(ImageError::TransformFailed);
    }

    let mut data = sys::WebPData::default();
    // SAFETY: on success libwebp hands ownership of `data.bytes` to us; it is
    // copied and then released with WebPDataClear.
    let mut bytes = unsafe {
        if sys::WebPAnimEncoderAssemble(encoder.0, &mut data) == 0 || data.bytes.is_null() {
            sys::WebPDataClear(&mut data);
            return Err(ImageError::TransformFailed);
        }
        let bytes = std::slice::from_raw_parts(data.bytes, data.size).to_vec();
        sys::WebPDataClear(&mut data);
        bytes
    };

    if image.frames.iter().any(Raster::has_transparency) {
        ensure_alpha_flag(&mut bytes)?;
    }
    Ok(bytes)
}

/// libwebp's animation encoder can trim fully transparent areas out of every
/// sub-frame and then emit a VP8X header without the alpha flag. Decoders
/// then paint the "uncovered" canvas opaque. Animations whose source frames
/// contain transparency must advertise it.
fn ensure_alpha_flag(bytes: &mut [u8]) -> Result<(), ImageError> {
    if bytes.len() < 21 || &bytes[12..16] != b"VP8X" {
        return Err(ImageError::TransformFailed);
    }
    bytes[20] |= VP8X_ALPHA_FLAG;
    Ok(())
}

fn lossy_config(quality: u8) -> Result<sys::WebPConfig, ImageError> {
    let mut config = std::mem::MaybeUninit::<sys::WebPConfig>::uninit();
    // SAFETY: the init function fully initializes `config` when it succeeds.
    let mut config = unsafe {
        if sys::WebPConfigInitInternal(
            config.as_mut_ptr(),
            sys::WebPPreset::WEBP_PRESET_PHOTO,
            f32::from(quality),
            sys::WEBP_ENCODER_ABI_VERSION as c_int,
        ) == 0
        {
            return Err(ImageError::TransformFailed);
        }
        config.assume_init()
    };
    config.alpha_quality = c_int::from(quality);
    config.method = WEBP_METHOD;
    // Sharp RGB->YUV conversion, libvips' `smart-subsample`.
    config.use_sharp_yuv = 1;
    config.thread_level = 0;
    // SAFETY: `config` is initialized.
    if unsafe { sys::WebPValidateConfig(&config) } == 0 {
        return Err(ImageError::TransformFailed);
    }
    Ok(config)
}

struct Picture(sys::WebPPicture);

impl Picture {
    fn import(raster: &Raster) -> Result<Self, ImageError> {
        let mut picture = Self(sys::WebPPicture::new().map_err(|_| ImageError::TransformFailed)?);
        picture.0.width = c_int::try_from(raster.width).map_err(|_| ImageError::TransformFailed)?;
        picture.0.height =
            c_int::try_from(raster.height).map_err(|_| ImageError::TransformFailed)?;
        // ARGB input lets WebPEncode apply sharp YUV conversion.
        picture.0.use_argb = 1;
        let stride =
            c_int::try_from(raster.row_bytes()).map_err(|_| ImageError::TransformFailed)?;
        // SAFETY: `raster.data` holds `height * stride` bytes in the declared
        // layout; libwebp copies them into memory owned by the picture.
        let imported = unsafe {
            match raster.layout {
                Layout::Rgb => {
                    sys::WebPPictureImportRGB(&mut picture.0, raster.data.as_ptr(), stride)
                }
                Layout::Rgba => {
                    sys::WebPPictureImportRGBA(&mut picture.0, raster.data.as_ptr(), stride)
                }
            }
        };
        if imported == 0 {
            return Err(ImageError::TransformFailed);
        }
        Ok(picture)
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: frees memory allocated by the import; safe on empty pictures.
        unsafe { sys::WebPPictureFree(&mut self.0) }
    }
}

struct MemoryWriter(sys::WebPMemoryWriter);

impl MemoryWriter {
    fn new() -> Self {
        let mut writer = std::mem::MaybeUninit::<sys::WebPMemoryWriter>::uninit();
        // SAFETY: WebPMemoryWriterInit fully initializes the writer.
        unsafe {
            sys::WebPMemoryWriterInit(writer.as_mut_ptr());
            Self(writer.assume_init())
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        if self.0.mem.is_null() {
            return Vec::new();
        }
        // SAFETY: libwebp keeps `size` initialized bytes at `mem`.
        unsafe { std::slice::from_raw_parts(self.0.mem, self.0.size).to_vec() }
    }
}

impl Drop for MemoryWriter {
    fn drop(&mut self) {
        // SAFETY: releases the buffer allocated by WebPMemoryWrite.
        unsafe { sys::WebPMemoryWriterClear(&mut self.0) }
    }
}

struct AnimEncoder(*mut sys::WebPAnimEncoder);

impl AnimEncoder {
    fn new(
        width: u32,
        height: u32,
        options: &sys::WebPAnimEncoderOptions,
    ) -> Result<Self, ImageError> {
        let width = c_int::try_from(width).map_err(|_| ImageError::TransformFailed)?;
        let height = c_int::try_from(height).map_err(|_| ImageError::TransformFailed)?;
        // SAFETY: `options` is initialized; null means invalid input or OOM.
        let raw = unsafe {
            sys::WebPAnimEncoderNewInternal(
                width,
                height,
                options,
                sys::WEBP_MUX_ABI_VERSION as c_int,
            )
        };
        if raw.is_null() {
            Err(ImageError::TransformFailed)
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for AnimEncoder {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from WebPAnimEncoderNewInternal.
        unsafe { sys::WebPAnimEncoderDelete(self.0) }
    }
}
