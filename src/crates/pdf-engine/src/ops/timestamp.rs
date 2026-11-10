//! RFC 3161 timestamp *request* plumbing for whole-document hashing.
//!
//! Honest scope — what this module does and does not do:
//!
//! - [`document_digest`] computes a cryptographic digest (SHA-256 or
//!   SHA-512) over the raw input bytes, enforcing `max_input_bytes`.
//! - [`build_timestamp_request`] DER-encodes a minimal, spec-shaped
//!   RFC 3161 `TimeStampReq` (version, `MessageImprint`, random nonce)
//!   for a caller-supplied digest. This is real DER — a TSA or
//!   `openssl ts` can parse it.
//! - [`request_external_timestamp`] POSTs those TSQ bytes to a
//!   caller-configured TSA URL over HTTPS (ureq, no redirects, bounded
//!   timeout, bounded response) and returns the raw `TimeStampResp`
//!   bytes the TSA sent back. [`timestamp_document`] chains the three.
//!
//! What is deliberately *not* here: issuing a timestamp token. A valid
//! RFC 3161 token is a CMS `SignedData` structure that only a Time
//! Stamping Authority holding a signing certificate can produce, and
//! generating one locally would be a forgery, not a timestamp. There is
//! no offline fallback: without a reachable TSA this module returns an
//! error rather than fabricating a token. Embedding a returned token
//! into a PDF signature dictionary (`/ByteRange` + `/Contents`) is the
//! signing domain — see `ops::sign` / `ops::verify_signature`.
//!
//! The TSQ/DER path needs no external tools, so unlike `raster` there
//! is no `find_tool` probe and no `MissingTool` failure mode; the only
//! external dependency is the network call to the TSA, which the
//! caller opts into by supplying a URL.

use std::io::Read;
use std::time::Duration;

use rand_core::{OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256, Sha512};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

/// Maximum size of a DER `TimeStampReq` we will build or send. A
/// well-formed TSQ is a few hundred bytes; anything larger means the
/// caller passed a non-digest payload.
const MAX_TSQ_BYTES: usize = 64 * 1024;

/// Maximum `TimeStampResp` body accepted from a TSA. Tokens are a few
/// KB; 4 MiB leaves room for certificate chains without letting a
/// hostile endpoint exhaust memory.
const MAX_TSA_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Whole-request timeout for the TSA HTTPS call.
const TSA_TIMEOUT: Duration = Duration::from_secs(30);

/// Digest algorithms this module can produce and encode into a
/// `MessageImprint`. Both are approved hash algorithms under RFC 3161 /
/// NIST SP 800-131A for timestamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampAlgorithm {
    /// SHA-256 — 32-byte digest, the common default for TSA requests.
    Sha256,
    /// SHA-512 — 64-byte digest, for stronger collision resistance.
    Sha512,
}

impl TimestampAlgorithm {
    /// Digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }

    /// Lowercase algorithm name as it appears in reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// DER encoding of the `algorithm` OID inside
    /// `AlgorithmIdentifier` (NIST CSOR OIDs for SHA-2).
    fn oid_der(self) -> &'static [u8] {
        match self {
            // 2.16.840.1.101.3.4.2.1  (id-sha256)
            Self::Sha256 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01],
            // 2.16.840.1.101.3.4.2.3  (id-sha512)
            Self::Sha512 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03],
        }
    }
}

/// Result of [`document_digest`]: the hex digest plus provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestReport {
    /// Algorithm used (`"sha256"` / `"sha512"`).
    pub algorithm: String,
    /// Lowercase hex digest.
    pub digest_hex: String,
    /// Digest length in bytes.
    pub digest_len: usize,
    /// Size of the digested input in bytes.
    pub input_size_bytes: u64,
}

/// A raw `TimeStampResp` returned by a TSA, plus where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimestampToken {
    /// The HTTPS URL the TSQ was POSTed to.
    pub tsa_url: String,
    /// Raw DER `TimeStampResp` bytes. Storing/embedding these into a
    /// PDF signature is a separate operation (`sign.rs` domain).
    pub der: Vec<u8>,
    /// `der.len()` for convenience.
    pub size_bytes: u64,
}

/// Full [`timestamp_document`] result: the digest that was requested
/// and the TSA's response token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentTimestamp {
    /// Digest over the input that the TSA attested to.
    pub digest: DigestReport,
    /// The TSA's `TimeStampResp`.
    pub token: TimestampToken,
}

/// Compute `algorithm` over `input` and return the report.
///
/// The input is treated as opaque bytes — it need not be a parseable
/// PDF (callers timestamping a `/ByteRange` slice pass the slice).
/// `max_input_bytes` from `limits` is still enforced.
pub fn document_digest(
    input: &[u8],
    algorithm: TimestampAlgorithm,
    limits: &EngineLimits,
) -> PdfResult<DigestReport> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(PdfError::LimitExceeded(format!(
            "input size {} bytes exceeds max_input_bytes {max}",
            input.len()
        )));
    }
    let digest = match algorithm {
        TimestampAlgorithm::Sha256 => Sha256::digest(input).to_vec(),
        TimestampAlgorithm::Sha512 => Sha512::digest(input).to_vec(),
    };
    Ok(DigestReport {
        algorithm: algorithm.name().to_string(),
        digest_hex: hex::encode(&digest),
        digest_len: digest.len(),
        input_size_bytes: input.len() as u64,
    })
}

/// Convenience: [`document_digest`] + [`build_timestamp_request`]
/// over the whole `input`. Returns the DER `TimeStampReq` bytes.
pub fn timestamp_query(
    input: &[u8],
    algorithm: TimestampAlgorithm,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let report = document_digest(input, algorithm, limits)?;
    // `digest_hex` was produced by `hex::encode` above, so decode is
    // infallible in practice; keep the error path honest anyway.
    let digest_bytes = hex::decode(&report.digest_hex)
        .map_err(|e| PdfError::InvalidInput(format!("internal digest re-encode failed: {e}")))?;
    build_timestamp_request(&digest_bytes, algorithm)
}

/// DER-encode an RFC 3161 `TimeStampReq` for `digest`.
///
/// ```text
/// TimeStampReq ::= SEQUENCE {
///   version        INTEGER { v1(1) },
///   messageImprint MessageImprint,
///   nonce          INTEGER OPTIONAL
/// }
/// MessageImprint ::= SEQUENCE {
///   hashAlgorithm  AlgorithmIdentifier,
///   hashedMessage  OCTET STRING
/// }
/// ```
///
/// A fresh 8-byte random nonce is included (replay protection — TSAs
/// echo it in the token). `certReq` is omitted (DEFAULT FALSE).
/// `digest` must be exactly the algorithm's digest length.
pub fn build_timestamp_request(digest: &[u8], algorithm: TimestampAlgorithm) -> PdfResult<Vec<u8>> {
    let expected = algorithm.digest_len();
    if digest.len() != expected {
        return Err(PdfError::InvalidInput(format!(
            "{} digest must be {expected} bytes, got {}",
            algorithm.name(),
            digest.len()
        )));
    }

    // AlgorithmIdentifier { algorithm OID, parameters NULL }
    let alg_id = der_sequence(&[
        &der_tlv(TAG_OID, algorithm.oid_der()),
        &der_tlv(TAG_NULL, &[]),
    ]);
    let message_imprint = der_sequence(&[&alg_id, &der_tlv(TAG_OCTET_STRING, digest)]);
    // Random positive nonce: 8 bytes with the top bit cleared so the
    // INTEGER stays positive without a sign pad in the common case.
    let mut nonce_bytes = [0u8; 8];
    OsRng.fill_bytes(&mut nonce_bytes);
    nonce_bytes[0] &= 0x7f;
    let nonce = der_integer(&nonce_bytes);

    let version = der_integer(&[1]);
    Ok(der_sequence(&[&version, &message_imprint, &nonce]))
}

/// POST `tsq` (DER `TimeStampReq` bytes, e.g. from
/// [`build_timestamp_request`]) to `tsa_url` and return the TSA's raw
/// `TimeStampResp`.
///
/// Requirements and guarantees:
///
/// - `tsa_url` must be `https://` — timestamp requests carry the
///   document digest and responses are trust anchors; plain HTTP is
///   rejected with [`PdfError::InvalidInput`].
/// - The request times out after [`TSA_TIMEOUT`], follows no redirects
///   (a redirect could leak the query to a different host), and caps
///   the response body at [`MAX_TSA_RESPONSE_BYTES`].
/// - Non-2xx responses surface as [`PdfError::ExternalTool`]; transport
///   failures as [`PdfError::Io`]/`ExternalTool`. Nothing is faked —
///   if the TSA cannot be reached, this errors.
///
/// `limits` is consulted for `max_input_bytes` against the TSQ size
/// only; the TSA response cap is fixed above.
pub fn request_external_timestamp(
    tsq: &[u8],
    tsa_url: &str,
    limits: &EngineLimits,
) -> PdfResult<TimestampToken> {
    if tsq.is_empty() || tsq.len() > MAX_TSQ_BYTES {
        return Err(PdfError::InvalidInput(format!(
            "timestamp query must be 1..={MAX_TSQ_BYTES} bytes, got {}",
            tsq.len()
        )));
    }
    if let Some(max) = limits.max_input_bytes
        && tsq.len() as u64 > max
    {
        return Err(PdfError::LimitExceeded(format!(
            "timestamp query size {} exceeds max_input_bytes {max}",
            tsq.len()
        )));
    }
    let tsa_url = tsa_url.trim();
    if !tsa_url.starts_with("https://") {
        return Err(PdfError::InvalidInput(
            "tsa_url must be an https:// URL — timestamp requests require TLS".to_string(),
        ));
    }
    if tsa_url.len() > 2048 {
        return Err(PdfError::InvalidInput(
            "tsa_url exceeds 2048 bytes".to_string(),
        ));
    }

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TSA_TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .build();
    let agent: ureq::Agent = config.into();

    let request = ureq::http::Request::builder()
        .method("POST")
        .uri(tsa_url)
        .header("Content-Type", "application/timestamp-query")
        .body(tsq.to_vec())
        .map_err(|e| PdfError::InvalidInput(format!("invalid TSA request: {e}")))?;

    let mut response = agent.run(request).map_err(|e| match e {
        ureq::Error::Io(io) => PdfError::Io(io),
        other => PdfError::ExternalTool {
            tool: "tsa".to_string(),
            detail: format!("request failed: {other}"),
        },
    })?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(PdfError::ExternalTool {
            tool: "tsa".to_string(),
            detail: format!("TSA returned HTTP status {status}"),
        });
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_TSA_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(PdfError::Io)?;
    if body.len() as u64 > MAX_TSA_RESPONSE_BYTES {
        return Err(PdfError::LimitExceeded(format!(
            "TSA response exceeds {MAX_TSA_RESPONSE_BYTES} bytes"
        )));
    }
    if body.is_empty() {
        return Err(PdfError::ExternalTool {
            tool: "tsa".to_string(),
            detail: "TSA returned an empty response".to_string(),
        });
    }

    Ok(TimestampToken {
        tsa_url: tsa_url.to_string(),
        size_bytes: body.len() as u64,
        der: body,
    })
}

/// End-to-end: digest `input`, build the TSQ, POST it to `tsa_url`.
/// Returns both the digest report and the raw token bytes.
///
/// This is the honest RFC 3161 flow — the token is whatever the TSA
/// signed; nothing about the response is synthesized locally.
pub fn timestamp_document(
    input: &[u8],
    algorithm: TimestampAlgorithm,
    tsa_url: &str,
    limits: &EngineLimits,
) -> PdfResult<DocumentTimestamp> {
    let digest = document_digest(input, algorithm, limits)?;
    let digest_bytes = hex::decode(&digest.digest_hex)
        .map_err(|e| PdfError::InvalidInput(format!("internal digest re-encode failed: {e}")))?;
    let tsq = build_timestamp_request(&digest_bytes, algorithm)?;
    let token = request_external_timestamp(&tsq, tsa_url, limits)?;
    Ok(DocumentTimestamp { digest, token })
}

// ---------------------------------------------------------------------------
// Minimal DER encoder (just enough for TimeStampReq)
// ---------------------------------------------------------------------------

const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_NULL: u8 = 0x05;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;

/// Encode one TLV: tag byte + DER length + content.
fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + content.len());
    out.push(tag);
    encode_der_length(content.len(), &mut out);
    out.extend_from_slice(content);
    out
}

/// DER length octets: short form below 128, otherwise `0x80|n` followed
/// by `n` big-endian length bytes (DER forbids indefinite/longest-form
/// padding, so minimal-length encoding is required).
fn encode_der_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    let first = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len() - 1);
    let significant = &bytes[first..];
    out.push(0x80 | significant.len() as u8);
    out.extend_from_slice(significant);
}

/// `SEQUENCE` wrapping already-encoded children.
fn der_sequence(children: &[&[u8]]) -> Vec<u8> {
    let content_len: usize = children.iter().map(|c| c.len()).sum();
    let mut out = Vec::with_capacity(1 + 4 + content_len);
    out.push(TAG_SEQUENCE);
    encode_der_length(content_len, &mut out);
    for child in children {
        out.extend_from_slice(child);
    }
    out
}

/// INTEGER from magnitude bytes. A leading `0x00` is added when the
/// top bit is set so the value stays positive; redundant leading zeros
/// are stripped first.
fn der_integer(magnitude: &[u8]) -> Vec<u8> {
    let first_nonzero = magnitude.iter().position(|&b| b != 0);
    let mut bytes = match first_nonzero {
        Some(i) => &magnitude[i..],
        None => &magnitude[magnitude.len().saturating_sub(1)..],
    };
    // `&[0]` case: empty slice can't happen because magnitude is never
    // empty for our callers; guard anyway.
    if bytes.is_empty() {
        bytes = &[0];
    }
    let mut content = Vec::with_capacity(bytes.len() + 1);
    if bytes[0] & 0x80 != 0 {
        content.push(0);
    }
    content.extend_from_slice(bytes);
    der_tlv(TAG_INTEGER, &content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Minimal DER reader for test assertions: returns (tag, content,
    /// rest). Not a validator — tests use it to check our own encoding.
    fn read_tlv(buf: &[u8]) -> (u8, &[u8], &[u8]) {
        assert!(buf.len() >= 2, "truncated TLV");
        let tag = buf[0];
        let first_len = buf[1];
        let (len, len_bytes) = if first_len & 0x80 == 0 {
            (first_len as usize, 1)
        } else {
            let n = (first_len & 0x7f) as usize;
            assert!(
                n >= 1 && n <= 4 && buf.len() >= 2 + n,
                "bad long-form length"
            );
            let mut len = 0usize;
            for &b in &buf[2..2 + n] {
                len = (len << 8) | b as usize;
            }
            (len, 1 + n)
        };
        let start = 1 + len_bytes;
        assert!(buf.len() >= start + len, "content overruns buffer");
        (tag, &buf[start..start + len], &buf[start + len..])
    }

    /// Split a SEQUENCE's content into its child TLVs. Returns
    /// `(tag, content, raw_tlv)` per child — `raw_tlv` is the complete
    /// encoded TLV so callers can re-parse a child with [`read_tlv`].
    fn children(seq: &[u8]) -> Vec<(u8, &[u8], &[u8])> {
        let mut out = Vec::new();
        let mut rest = seq;
        while !rest.is_empty() {
            let (tag, content, tail) = read_tlv(rest);
            let tlv_len = rest.len() - tail.len();
            out.push((tag, content, &rest[..tlv_len]));
            rest = tail;
        }
        out
    }

    #[test]
    fn tsq_has_expected_der_structure() {
        let digest = [0xabu8; 32];
        let tsq = build_timestamp_request(&digest, TimestampAlgorithm::Sha256).unwrap();

        // Outer SEQUENCE consumes the whole buffer.
        let (tag, content, rest) = read_tlv(&tsq);
        assert_eq!(tag, TAG_SEQUENCE);
        assert!(rest.is_empty());

        let kids = children(content);
        assert_eq!(kids.len(), 3, "version + imprint + nonce");

        // version INTEGER == 1
        let (tag, v, _) = read_tlv(kids[0].2);
        assert_eq!(tag, TAG_INTEGER);
        assert_eq!(v, &[1]);

        // messageImprint SEQUENCE { algId SEQUENCE, OCTET STRING }
        let (tag, imprint, _) = read_tlv(kids[1].2);
        assert_eq!(tag, TAG_SEQUENCE);
        let imp = children(imprint);
        assert_eq!(imp.len(), 2);
        let (tag, alg, _) = read_tlv(imp[0].2);
        assert_eq!(tag, TAG_SEQUENCE);
        let alg_kids = children(alg);
        assert_eq!(alg_kids.len(), 2);
        assert_eq!(alg_kids[0].0, TAG_OID);
        assert_eq!(alg_kids[0].1, TimestampAlgorithm::Sha256.oid_der());
        assert_eq!(alg_kids[1].0, TAG_NULL);
        assert_eq!(imp[1].0, TAG_OCTET_STRING);
        assert_eq!(imp[1].1, &digest[..]);

        // nonce INTEGER present; DER-minimal so 1..=9 content bytes
        // (leading zeros stripped, optional sign pad).
        let (tag, nonce, _) = read_tlv(kids[2].2);
        assert_eq!(tag, TAG_INTEGER);
        assert!((1..=9).contains(&nonce.len()));
    }

    #[test]
    fn tsq_sha512_uses_sha512_oid() {
        let tsq = build_timestamp_request(&[0x55; 64], TimestampAlgorithm::Sha512).unwrap();
        let (_, content, _) = read_tlv(&tsq);
        let kids = children(content);
        let (_, imprint, _) = read_tlv(kids[1].2);
        let imp = children(imprint);
        let (_, alg, _) = read_tlv(imp[0].2);
        let alg_kids = children(alg);
        assert_eq!(alg_kids[0].1, TimestampAlgorithm::Sha512.oid_der());
    }

    #[test]
    fn tsq_rejects_wrong_digest_length() {
        let err = build_timestamp_request(&[0u8; 16], TimestampAlgorithm::Sha256).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
    }

    #[test]
    fn document_digest_matches_sha2() {
        let input = b"the quick brown fox";
        let report = document_digest(input, TimestampAlgorithm::Sha256, &limits()).unwrap();
        let expected = hex::encode(Sha256::digest(input));
        assert_eq!(report.digest_hex, expected);
        assert_eq!(report.digest_len, 32);
        assert_eq!(report.algorithm, "sha256");
        assert_eq!(report.input_size_bytes, input.len() as u64);

        let report512 = document_digest(input, TimestampAlgorithm::Sha512, &limits()).unwrap();
        assert_eq!(report512.digest_hex, hex::encode(Sha512::digest(input)));
    }

    #[test]
    fn document_digest_enforces_input_limit() {
        let mut lim = EngineLimits::default();
        lim.max_input_bytes = Some(4);
        let err = document_digest(b"12345", TimestampAlgorithm::Sha256, &lim).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)));
    }

    #[test]
    fn timestamp_query_roundtrip_is_valid_tsq() {
        let tsq = timestamp_query(b"payload", TimestampAlgorithm::Sha256, &limits()).unwrap();
        let (tag, _, rest) = read_tlv(&tsq);
        assert_eq!(tag, TAG_SEQUENCE);
        assert!(rest.is_empty());
    }

    #[test]
    fn request_rejects_non_https_tsa_url() {
        let tsq = vec![0x30, 0x03, 0x02, 0x01, 0x01];
        for url in ["http://tsa.example.com", "ftp://x", "notaurl"] {
            let err = request_external_timestamp(&tsq, url, &limits()).unwrap_err();
            assert!(matches!(err, PdfError::InvalidInput(_)), "{url}");
        }
    }

    #[test]
    fn request_rejects_oversized_or_empty_tsq() {
        assert!(matches!(
            request_external_timestamp(&[], "https://tsa.example.com", &limits()).unwrap_err(),
            PdfError::InvalidInput(_)
        ));
        let big = vec![0u8; MAX_TSQ_BYTES + 1];
        assert!(matches!(
            request_external_timestamp(&big, "https://tsa.example.com", &limits()).unwrap_err(),
            PdfError::InvalidInput(_)
        ));
    }

    #[test]
    fn der_integer_strips_leading_zeros_and_pads_sign() {
        // 0x00 0x80 → stays positive via pad: content 00 00 80.
        let enc = der_integer(&[0x00, 0x80]);
        let (tag, content, _) = read_tlv(&enc);
        assert_eq!(tag, TAG_INTEGER);
        assert_eq!(content, &[0x00, 0x80]);
        // Zero → single 0x00.
        let enc = der_integer(&[0x00, 0x00]);
        let (_, content, _) = read_tlv(&enc);
        assert_eq!(content, &[0x00]);
    }
}
