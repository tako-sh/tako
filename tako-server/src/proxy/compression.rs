use bytes::Bytes;
use flate2::{Compression, write::GzEncoder};
use pingora_core::prelude::*;
use pingora_http::{RequestHeader, ResponseHeader};
use std::io::Write;
use std::time::Duration;

const MIN_COMPRESS_BODY_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompressionAlgorithm {
    Brotli,
    Gzip,
}

impl CompressionAlgorithm {
    fn as_header(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
        }
    }

    fn as_log_value(self) -> &'static str {
        self.as_header()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompressionSkipReason {
    AlreadyEncoded,
    HeadRequest,
    IneligibleStatus,
    MissingAcceptEncoding,
    NoTransform,
    ServerSentEvents,
    StreamingOrUnknownLength,
    TooSmall,
    UnsupportedEncoding,
    Upgrade,
    UnsupportedContentType,
}

impl CompressionSkipReason {
    fn as_log_value(self) -> &'static str {
        match self {
            Self::AlreadyEncoded => "already_encoded",
            Self::HeadRequest => "head_request",
            Self::IneligibleStatus => "ineligible_status",
            Self::MissingAcceptEncoding => "missing_accept_encoding",
            Self::NoTransform => "no_transform",
            Self::ServerSentEvents => "server_sent_events",
            Self::StreamingOrUnknownLength => "streaming_or_unknown_length",
            Self::TooSmall => "too_small",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::Upgrade => "upgrade",
            Self::UnsupportedContentType => "unsupported_content_type",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompressionDecision {
    Compress(CompressionAlgorithm),
    Skip {
        reason: CompressionSkipReason,
        vary_required: bool,
    },
}

#[derive(Debug)]
enum CompressionState {
    Undecided,
    Active {
        algorithm: CompressionAlgorithm,
        body: Vec<u8>,
        uncompressed_bytes: u64,
        compressed_bytes: Option<u64>,
    },
    Skipped {
        reason: CompressionSkipReason,
        vary_required: bool,
        bytes: u64,
    },
}

#[derive(Debug)]
pub(super) struct ResponseCompression {
    state: CompressionState,
}

impl ResponseCompression {
    pub(super) fn new() -> Self {
        Self {
            state: CompressionState::Undecided,
        }
    }

    pub(super) fn prepare(
        &mut self,
        request: &RequestHeader,
        response: &mut ResponseHeader,
    ) -> Result<()> {
        match compression_decision(request, response) {
            CompressionDecision::Compress(algorithm) => {
                ensure_vary_accept_encoding(response)?;
                response.insert_header("Content-Encoding", algorithm.as_header())?;
                let _ = response.remove_header("Content-Length");
                let _ = response.remove_header("ETag");
                let _ = response.remove_header("Content-MD5");
                self.state = CompressionState::Active {
                    algorithm,
                    body: Vec::new(),
                    uncompressed_bytes: 0,
                    compressed_bytes: None,
                };
            }
            CompressionDecision::Skip {
                reason,
                vary_required,
            } => {
                if vary_required {
                    ensure_vary_accept_encoding(response)?;
                }
                self.state = CompressionState::Skipped {
                    reason,
                    vary_required,
                    bytes: 0,
                };
            }
        }

        Ok(())
    }

    pub(super) fn filter_body(
        &mut self,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<Option<Duration>> {
        match &mut self.state {
            CompressionState::Undecided => Ok(None),
            CompressionState::Skipped { bytes, .. } => {
                if let Some(chunk) = body.as_ref() {
                    *bytes += chunk.len() as u64;
                }
                Ok(None)
            }
            CompressionState::Active {
                algorithm,
                body: buffered,
                uncompressed_bytes,
                compressed_bytes,
            } => {
                if let Some(chunk) = body.take() {
                    *uncompressed_bytes += chunk.len() as u64;
                    buffered.extend_from_slice(&chunk);
                }

                if !end_of_stream {
                    *body = None;
                    return Ok(None);
                }

                let compressed = compress_body(*algorithm, buffered)?;
                *compressed_bytes = Some(compressed.len() as u64);
                *body = Some(Bytes::from(compressed));
                Ok(None)
            }
        }
    }

    pub(super) fn algorithm_log_value(&self) -> &'static str {
        match self.state {
            CompressionState::Active { algorithm, .. } => algorithm.as_log_value(),
            _ => "none",
        }
    }

    pub(super) fn skip_reason_log_value(&self) -> &'static str {
        match self.state {
            CompressionState::Skipped { reason, .. } => reason.as_log_value(),
            CompressionState::Undecided => "not_proxied",
            CompressionState::Active { .. } => "none",
        }
    }

    pub(super) fn vary_required(&self) -> bool {
        match self.state {
            CompressionState::Active { .. } => true,
            CompressionState::Skipped { vary_required, .. } => vary_required,
            CompressionState::Undecided => false,
        }
    }

    pub(super) fn uncompressed_bytes(&self) -> u64 {
        match &self.state {
            CompressionState::Active {
                uncompressed_bytes, ..
            } => *uncompressed_bytes,
            CompressionState::Skipped { bytes, .. } => *bytes,
            CompressionState::Undecided => 0,
        }
    }

    pub(super) fn compressed_bytes(&self) -> Option<u64> {
        match &self.state {
            CompressionState::Active {
                compressed_bytes, ..
            } => *compressed_bytes,
            _ => None,
        }
    }
}

pub(super) fn compression_decision(
    request: &RequestHeader,
    response: &ResponseHeader,
) -> CompressionDecision {
    if request.method.as_str().eq_ignore_ascii_case("HEAD") {
        return skip(CompressionSkipReason::HeadRequest, false);
    }

    if request.headers.contains_key("upgrade") || response.status.as_u16() == 101 {
        return skip(CompressionSkipReason::Upgrade, false);
    }

    if !response.status.is_success() {
        return skip(CompressionSkipReason::IneligibleStatus, false);
    }

    if response.headers.contains_key("content-encoding") {
        return skip(CompressionSkipReason::AlreadyEncoded, false);
    }

    if cache_control_has_no_transform(response) {
        return skip(CompressionSkipReason::NoTransform, false);
    }

    let Some(content_type) = normalized_content_type(response) else {
        return skip(CompressionSkipReason::UnsupportedContentType, false);
    };

    if content_type == "text/event-stream" {
        return skip(CompressionSkipReason::ServerSentEvents, false);
    }

    if !is_compressible_content_type(&content_type) {
        return skip(CompressionSkipReason::UnsupportedContentType, false);
    }

    let Some(content_length) = response
        .headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return skip(CompressionSkipReason::StreamingOrUnknownLength, false);
    };

    if content_length < MIN_COMPRESS_BODY_BYTES {
        return skip(CompressionSkipReason::TooSmall, false);
    }

    let Some(accept_encoding) = request
        .headers
        .get("accept-encoding")
        .and_then(|value| value.to_str().ok())
    else {
        return skip(CompressionSkipReason::MissingAcceptEncoding, true);
    };

    match negotiate_algorithm(accept_encoding) {
        Some(algorithm) => CompressionDecision::Compress(algorithm),
        None => skip(CompressionSkipReason::UnsupportedEncoding, true),
    }
}

fn skip(reason: CompressionSkipReason, vary_required: bool) -> CompressionDecision {
    CompressionDecision::Skip {
        reason,
        vary_required,
    }
}

fn compress_body(algorithm: CompressionAlgorithm, body: &[u8]) -> Result<Vec<u8>> {
    match algorithm {
        CompressionAlgorithm::Brotli => {
            let mut output = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
                writer.write_all(body).map_err(compression_error)?;
            }
            Ok(output)
        }
        CompressionAlgorithm::Gzip => {
            let mut writer = GzEncoder::new(Vec::new(), Compression::default());
            writer.write_all(body).map_err(compression_error)?;
            writer.finish().map_err(compression_error)
        }
    }
}

fn compression_error(error: std::io::Error) -> Box<Error> {
    Error::explain(
        ErrorType::InternalError,
        format!("failed to compress response body: {error}"),
    )
}

fn negotiate_algorithm(accept_encoding: &str) -> Option<CompressionAlgorithm> {
    let mut explicit_brotli = None;
    let mut explicit_gzip = None;
    let mut wildcard = None;

    for raw in accept_encoding.split(',') {
        let mut parts = raw.split(';');
        let encoding = parts
            .next()
            .map(str::trim)
            .unwrap_or("")
            .to_ascii_lowercase();
        if encoding.is_empty() {
            continue;
        }

        let mut quality = 1.0_f32;
        for param in parts {
            let mut kv = param.splitn(2, '=');
            let key = kv.next().map(str::trim).unwrap_or("");
            let value = kv.next().map(str::trim).unwrap_or("");
            if key.eq_ignore_ascii_case("q") {
                quality = value
                    .parse::<f32>()
                    .ok()
                    .filter(|q| (0.0..=1.0).contains(q))
                    .unwrap_or(0.0);
            }
        }

        match encoding.as_str() {
            "br" => explicit_brotli = Some(quality),
            "gzip" => explicit_gzip = Some(quality),
            "*" => wildcard = Some(quality),
            _ => {}
        }
    }

    let brotli_quality = explicit_brotli.or(wildcard).unwrap_or(0.0);
    let gzip_quality = explicit_gzip.or(wildcard).unwrap_or(0.0);

    match (brotli_quality > 0.0, gzip_quality > 0.0) {
        (true, true) if brotli_quality >= gzip_quality => Some(CompressionAlgorithm::Brotli),
        (true, false) => Some(CompressionAlgorithm::Brotli),
        (true, true) | (false, true) => Some(CompressionAlgorithm::Gzip),
        (false, false) => None,
    }
}

fn normalized_content_type(response: &ResponseHeader) -> Option<String> {
    response
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn is_compressible_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/ecmascript"
                | "application/javascript"
                | "application/json"
                | "application/wasm"
                | "application/x-javascript"
                | "application/xml"
                | "image/svg+xml"
        )
        || content_type
            .strip_prefix("application/")
            .is_some_and(|subtype| subtype.ends_with("+json") || subtype.ends_with("+xml"))
}

fn cache_control_has_no_transform(response: &ResponseHeader) -> bool {
    response
        .headers
        .get_all("cache-control")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|directive| directive.trim().eq_ignore_ascii_case("no-transform"))
}

fn ensure_vary_accept_encoding(response: &mut ResponseHeader) -> Result<()> {
    let values = response
        .headers
        .get_all("vary")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();

    if values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|value| value == "*" || value.eq_ignore_ascii_case("accept-encoding"))
    {
        return Ok(());
    }

    if values.is_empty() {
        response.insert_header("Vary", "Accept-Encoding")?;
        return Ok(());
    }

    let mut merged = values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    merged.push("Accept-Encoding".to_string());

    let _ = response.remove_header("Vary");
    response.insert_header("Vary", merged.join(", "))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brotli::Decompressor;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn request(accept_encoding: Option<&str>) -> RequestHeader {
        let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
        if let Some(value) = accept_encoding {
            request
                .insert_header("Accept-Encoding", value)
                .expect("insert accept encoding");
        }
        request
    }

    fn response(content_type: &str, content_length: usize) -> ResponseHeader {
        let mut response =
            ResponseHeader::build(200, Some(content_length)).expect("build response");
        response
            .insert_header("Content-Type", content_type)
            .expect("insert content type");
        response
            .insert_header("Content-Length", content_length.to_string())
            .expect("insert content length");
        response
    }

    fn repeated_body() -> Vec<u8> {
        "body { color: #234; padding: 12px; }\n"
            .repeat(80)
            .into_bytes()
    }

    #[test]
    fn negotiates_brotli_over_gzip_when_quality_is_tied() {
        let request = request(Some("gzip, br"));
        let response = response("application/json", MIN_COMPRESS_BODY_BYTES);

        assert_eq!(
            compression_decision(&request, &response),
            CompressionDecision::Compress(CompressionAlgorithm::Brotli)
        );
    }

    #[test]
    fn negotiates_gzip_when_it_has_higher_quality() {
        let request = request(Some("br;q=0.2, gzip;q=1"));
        let response = response("text/css", MIN_COMPRESS_BODY_BYTES);

        assert_eq!(
            compression_decision(&request, &response),
            CompressionDecision::Compress(CompressionAlgorithm::Gzip)
        );
    }

    #[test]
    fn skips_eligible_response_without_supported_encoding_but_requires_vary() {
        let request = request(Some("identity"));
        let response = response("image/svg+xml", MIN_COMPRESS_BODY_BYTES);

        assert_eq!(
            compression_decision(&request, &response),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::UnsupportedEncoding,
                vary_required: true
            }
        );
    }

    #[test]
    fn skips_when_accept_encoding_is_missing_but_requires_vary() {
        let request = request(None);
        let response = response("text/html", MIN_COMPRESS_BODY_BYTES);

        assert_eq!(
            compression_decision(&request, &response),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::MissingAcceptEncoding,
                vary_required: true
            }
        );
    }

    #[test]
    fn skips_already_encoded_responses() {
        let request = request(Some("br, gzip"));
        let mut response = response("text/plain", MIN_COMPRESS_BODY_BYTES);
        response
            .insert_header("Content-Encoding", "gzip")
            .expect("insert content encoding");

        assert_eq!(
            compression_decision(&request, &response),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::AlreadyEncoded,
                vary_required: false
            }
        );
    }

    #[test]
    fn skips_no_transform_responses() {
        let request = request(Some("br, gzip"));
        let mut response = response("text/plain", MIN_COMPRESS_BODY_BYTES);
        response
            .insert_header("Cache-Control", "public, no-transform")
            .expect("insert cache control");

        assert_eq!(
            compression_decision(&request, &response),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::NoTransform,
                vary_required: false
            }
        );
    }

    #[test]
    fn skips_streaming_and_sse_responses() {
        let request = request(Some("br, gzip"));
        let mut stream = ResponseHeader::build(200, None).expect("build response");
        stream
            .insert_header("Content-Type", "text/plain")
            .expect("insert content type");
        let sse = response("text/event-stream", MIN_COMPRESS_BODY_BYTES);

        assert_eq!(
            compression_decision(&request, &stream),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::StreamingOrUnknownLength,
                vary_required: false
            }
        );
        assert_eq!(
            compression_decision(&request, &sse),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::ServerSentEvents,
                vary_required: false
            }
        );
    }

    #[test]
    fn skips_small_and_binary_responses() {
        let request = request(Some("br, gzip"));
        let small = response("text/plain", MIN_COMPRESS_BODY_BYTES - 1);
        let binary = response("image/png", MIN_COMPRESS_BODY_BYTES);

        assert_eq!(
            compression_decision(&request, &small),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::TooSmall,
                vary_required: false
            }
        );
        assert_eq!(
            compression_decision(&request, &binary),
            CompressionDecision::Skip {
                reason: CompressionSkipReason::UnsupportedContentType,
                vary_required: false
            }
        );
    }

    #[test]
    fn prepare_sets_compression_headers_and_merges_vary() {
        let request = request(Some("gzip"));
        let body = repeated_body();
        let mut response = response("text/css", body.len());
        response
            .insert_header("Vary", "Accept")
            .expect("insert vary");
        response
            .insert_header("ETag", "\"abc123\"")
            .expect("insert etag");

        let mut compression = ResponseCompression::new();
        compression
            .prepare(&request, &mut response)
            .expect("prepare compression");

        assert_eq!(
            response
                .headers
                .get("Content-Encoding")
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
        assert_eq!(
            response
                .headers
                .get("Vary")
                .and_then(|value| value.to_str().ok()),
            Some("Accept, Accept-Encoding")
        );
        assert!(!response.headers.contains_key("Content-Length"));
        assert!(!response.headers.contains_key("ETag"));
    }

    #[test]
    fn body_filter_buffers_chunks_and_emits_gzip_at_end() {
        let body = repeated_body();
        let request = request(Some("gzip"));
        let mut response = response("text/css", body.len());
        let mut compression = ResponseCompression::new();
        compression
            .prepare(&request, &mut response)
            .expect("prepare compression");

        let mut first = Some(Bytes::copy_from_slice(&body[..900]));
        compression
            .filter_body(&mut first, false)
            .expect("filter first body chunk");
        assert!(first.is_none());

        let mut second = Some(Bytes::copy_from_slice(&body[900..]));
        compression
            .filter_body(&mut second, true)
            .expect("filter final body chunk");
        let compressed = second.expect("compressed final body");
        let mut decoder = GzDecoder::new(compressed.as_ref());
        let mut decoded = Vec::new();
        decoder
            .read_to_end(&mut decoded)
            .expect("decode gzip response");

        assert_eq!(decoded, body);
        assert_eq!(compression.algorithm_log_value(), "gzip");
        assert_eq!(compression.uncompressed_bytes(), body.len() as u64);
        assert!(compression.compressed_bytes().is_some());
    }

    #[test]
    fn body_filter_emits_brotli_at_empty_end_chunk() {
        let body = repeated_body();
        let request = request(Some("br"));
        let mut response = response("application/activity+json", body.len());
        let mut compression = ResponseCompression::new();
        compression
            .prepare(&request, &mut response)
            .expect("prepare compression");

        let mut chunk = Some(Bytes::copy_from_slice(&body));
        compression
            .filter_body(&mut chunk, false)
            .expect("filter body chunk");
        let mut end = None;
        compression
            .filter_body(&mut end, true)
            .expect("filter end chunk");

        let compressed = end.expect("compressed body at end of stream");
        let mut decoder = Decompressor::new(compressed.as_ref(), 4096);
        let mut decoded = Vec::new();
        decoder
            .read_to_end(&mut decoded)
            .expect("decode brotli response");

        assert_eq!(decoded, body);
    }
}
