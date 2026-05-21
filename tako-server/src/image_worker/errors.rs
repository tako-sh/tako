use tako_images::ImageError;

pub(super) fn image_error_code(error: &ImageError) -> &'static str {
    match error {
        ImageError::InvalidUrl => "invalid_url",
        ImageError::InvalidSource => "invalid_source",
        ImageError::InvalidWidth => "invalid_width",
        ImageError::InvalidHeight => "invalid_height",
        ImageError::InvalidResize => "invalid_resize",
        ImageError::InvalidQuality => "invalid_quality",
        ImageError::InvalidBrowserCacheMaxAge => "invalid_browser_cache_max_age",
        ImageError::InvalidSignature => "invalid_signature",
        ImageError::Expired => "expired",
        ImageError::SourceTooLarge => "source_too_large",
        ImageError::UnsupportedFormat => "unsupported_format",
        ImageError::ImageTooLarge => "image_too_large",
        ImageError::TransformFailed => "transform_failed",
        ImageError::TransformQueueFull => "transform_queue_full",
    }
}

pub(super) fn image_error_from_code(code: &str) -> ImageError {
    match code {
        "invalid_url" => ImageError::InvalidUrl,
        "invalid_source" => ImageError::InvalidSource,
        "invalid_width" => ImageError::InvalidWidth,
        "invalid_height" => ImageError::InvalidHeight,
        "invalid_resize" => ImageError::InvalidResize,
        "invalid_quality" => ImageError::InvalidQuality,
        "invalid_browser_cache_max_age" => ImageError::InvalidBrowserCacheMaxAge,
        "invalid_signature" => ImageError::InvalidSignature,
        "expired" => ImageError::Expired,
        "source_too_large" => ImageError::SourceTooLarge,
        "unsupported_format" => ImageError::UnsupportedFormat,
        "image_too_large" => ImageError::ImageTooLarge,
        "transform_queue_full" => ImageError::TransformQueueFull,
        _ => ImageError::TransformFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_error_codes_round_trip() {
        for error in [
            ImageError::InvalidUrl,
            ImageError::InvalidSource,
            ImageError::InvalidWidth,
            ImageError::InvalidHeight,
            ImageError::InvalidResize,
            ImageError::InvalidQuality,
            ImageError::InvalidBrowserCacheMaxAge,
            ImageError::InvalidSignature,
            ImageError::Expired,
            ImageError::SourceTooLarge,
            ImageError::UnsupportedFormat,
            ImageError::ImageTooLarge,
            ImageError::TransformFailed,
            ImageError::TransformQueueFull,
        ] {
            assert_eq!(image_error_from_code(image_error_code(&error)), error);
        }
    }
}
