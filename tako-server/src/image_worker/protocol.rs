use super::errors::{image_error_code, image_error_from_code};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use tako_images::{
    ImageCrop, ImageError, ImageFit, OutputFormat, TransformLimits, TransformOptions,
    TransformedImage, transform_image,
};

#[derive(Debug, Serialize, Deserialize)]
struct WorkerLimits {
    max_source_bytes: usize,
    max_image_width: u32,
    max_image_height: u32,
    max_decoded_pixels: u64,
}

impl From<&TransformLimits> for WorkerLimits {
    fn from(limits: &TransformLimits) -> Self {
        Self {
            max_source_bytes: limits.max_source_bytes,
            max_image_width: limits.max_image_width,
            max_image_height: limits.max_image_height,
            max_decoded_pixels: limits.max_decoded_pixels,
        }
    }
}

impl From<WorkerLimits> for TransformLimits {
    fn from(limits: WorkerLimits) -> Self {
        Self {
            max_source_bytes: limits.max_source_bytes,
            max_image_width: limits.max_image_width,
            max_image_height: limits.max_image_height,
            max_decoded_pixels: limits.max_decoded_pixels,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct WorkerRequest {
    source_base64: String,
    source_content_type: Option<String>,
    format: OutputFormat,
    width: u32,
    height: Option<u32>,
    fit: Option<String>,
    crop: Option<String>,
    quality: u8,
    limits: WorkerLimits,
}

impl WorkerRequest {
    pub(super) fn new(
        source: &[u8],
        source_content_type: Option<&str>,
        options: TransformOptions,
        limits: &TransformLimits,
    ) -> Self {
        Self {
            source_base64: BASE64_STANDARD.encode(source),
            source_content_type: source_content_type.map(str::to_string),
            format: options.format,
            width: options.width,
            height: options.height,
            fit: options.fit.map(fit_code).map(str::to_string),
            crop: options.crop.map(crop_code).map(str::to_string),
            quality: options.quality,
            limits: WorkerLimits::from(limits),
        }
    }

    fn transform_options(&self) -> Result<TransformOptions, ImageError> {
        Ok(TransformOptions {
            format: self.format,
            width: self.width,
            height: self.height,
            fit: self.fit.as_deref().map(parse_fit).transpose()?,
            crop: self.crop.as_deref().map(parse_crop).transpose()?,
            quality: self.quality,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum WorkerResponse {
    Ok {
        bytes_base64: String,
        format: OutputFormat,
        width: u32,
        height: u32,
    },
    Error {
        error: String,
    },
}

pub(super) fn handle_request_bytes(input: &[u8]) -> WorkerResponse {
    let request = match serde_json::from_slice::<WorkerRequest>(input) {
        Ok(request) => request,
        Err(_) => {
            return WorkerResponse::Error {
                error: image_error_code(&ImageError::InvalidUrl).to_string(),
            };
        }
    };
    let source = match BASE64_STANDARD.decode(&request.source_base64) {
        Ok(source) => source,
        Err(_) => {
            return WorkerResponse::Error {
                error: image_error_code(&ImageError::InvalidSource).to_string(),
            };
        }
    };
    let options = match request.transform_options() {
        Ok(options) => options,
        Err(error) => {
            return WorkerResponse::Error {
                error: image_error_code(&error).to_string(),
            };
        }
    };
    let limits = TransformLimits::from(request.limits);

    match transform_image(
        &source,
        request.source_content_type.as_deref(),
        options,
        &limits,
    ) {
        Ok(transformed) => WorkerResponse::Ok {
            bytes_base64: BASE64_STANDARD.encode(transformed.bytes),
            format: transformed.format,
            width: transformed.width,
            height: transformed.height,
        },
        Err(error) => WorkerResponse::Error {
            error: image_error_code(&error).to_string(),
        },
    }
}

pub(super) fn decode_worker_response(
    output: &[u8],
    expected_format: OutputFormat,
) -> Result<TransformedImage, ImageError> {
    match serde_json::from_slice(output).map_err(|_| ImageError::TransformFailed)? {
        WorkerResponse::Ok {
            bytes_base64,
            format,
            width,
            height,
        } => {
            if !worker_output_format_matches_request(format, expected_format) {
                return Err(ImageError::TransformFailed);
            }
            Ok(TransformedImage {
                bytes: BASE64_STANDARD
                    .decode(bytes_base64)
                    .map_err(|_| ImageError::TransformFailed)?,
                content_type: content_type_for_format(format),
                format,
                width,
                height,
            })
        }
        WorkerResponse::Error { error } => Err(image_error_from_code(&error)),
    }
}

fn worker_output_format_matches_request(format: OutputFormat, requested: OutputFormat) -> bool {
    format == requested || (requested == OutputFormat::Avif && format == OutputFormat::Webp)
}

fn content_type_for_format(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Avif => "image/avif",
        OutputFormat::Webp => "image/webp",
    }
}

fn fit_code(fit: ImageFit) -> &'static str {
    match fit {
        ImageFit::Cover => "cover",
        ImageFit::Contain => "contain",
    }
}

fn parse_fit(value: &str) -> Result<ImageFit, ImageError> {
    match value {
        "cover" => Ok(ImageFit::Cover),
        "contain" => Ok(ImageFit::Contain),
        _ => Err(ImageError::InvalidResize),
    }
}

fn crop_code(crop: ImageCrop) -> &'static str {
    match crop {
        ImageCrop::Center => "center",
        ImageCrop::Smart => "smart",
    }
}

fn parse_crop(value: &str) -> Result<ImageCrop, ImageError> {
    match value {
        "center" => Ok(ImageCrop::Center),
        "smart" => Ok(ImageCrop::Smart),
        _ => Err(ImageError::InvalidResize),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_worker_response_rejects_wrong_format() {
        let output = serde_json::to_vec(&WorkerResponse::Ok {
            bytes_base64: BASE64_STANDARD.encode([1, 2, 3]),
            format: OutputFormat::Avif,
            width: 16,
            height: 8,
        })
        .expect("encode worker response");

        let err = decode_worker_response(&output, OutputFormat::Webp).unwrap_err();

        assert_eq!(err, ImageError::TransformFailed);
    }

    #[test]
    fn decode_worker_response_accepts_webp_fallback_for_avif_request() {
        let output = serde_json::to_vec(&WorkerResponse::Ok {
            bytes_base64: BASE64_STANDARD.encode([1, 2, 3]),
            format: OutputFormat::Webp,
            width: 16,
            height: 8,
        })
        .expect("encode worker response");

        let transformed =
            decode_worker_response(&output, OutputFormat::Avif).expect("decode fallback");

        assert_eq!(transformed.format, OutputFormat::Webp);
        assert_eq!(transformed.content_type, "image/webp");
    }
}
