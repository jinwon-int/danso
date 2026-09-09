//! Test-only Responses user-image serialization; no file I/O or image decoder.
//! Ingress must decode/validate pixels before constructing these blocks. Journal
//! and CLI ingestion remain text-only until durable replay/compaction is ready.
use super::wire;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

// Leave room within the existing 512 KiB HTTP cap. This is NOT a guarantee that
// the entire history fits: request_bytes and compaction gates still apply.
const MAX_IMAGE_BYTES: usize = 192 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

/// Experimental capability input, not a model registry or proof of support.
/// A future caller must resolve this for the exact provider/endpoint/model tuple.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageCapability {
    Unknown,
    Unsupported,
    Supported,
}

pub(super) fn user_content_for_capability(
    content: &Value,
    capability: ImageCapability,
) -> Result<Value> {
    let has_image = content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "image"));
    ensure!(
        !has_image || capability == ImageCapability::Supported,
        "image capability is not explicitly supported"
    );
    user_content(content)
}

/// Size the FINAL synthetic request, including tools, history and adapter fields.
/// This is neither a compaction budget nor an ingress allocation limit. It must
/// be called again after any body mutation. Never return a partially written body.
pub(super) fn bounded_request_bytes(body: &Value, limit: usize) -> Result<Vec<u8>> {
    struct BoundedBytes {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for BoundedBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("image request exceeds byte budget"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = BoundedBytes {
        bytes: Vec::new(),
        // Match the HTTP ceiling; a caller cannot raise it through this helper.
        limit: limit.min(512 * 1024),
    };
    serde_json::to_writer(&mut output, body)
        .map_err(|_| anyhow::anyhow!("image request serialization exceeds bounded envelope"))?;
    Ok(output.bytes)
}

// Envelope validation only; never use this as pixel/admission validation.
fn image_mime(block: &Value) -> Result<&str> {
    let fields = block.as_object().context("invalid image block")?;
    ensure!(
        fields.len() == 3 && fields.contains_key("data") && fields.contains_key("mimeType"),
        "image block requires only type, data and mimeType"
    );
    let mime = wire::string(block, "mimeType")?;
    ensure!(
        matches!(mime, "image/png" | "image/jpeg"),
        "unsupported image MIME type"
    );
    let data = wire::string(block, "data")?;
    ensure!(
        !data.is_empty() && data.len() <= MAX_ENCODED_BYTES,
        "image data exceeds bounded envelope"
    );
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| anyhow::anyhow!("invalid image base64"))?;
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES && STANDARD.encode(&bytes) == data,
        "invalid image base64 envelope"
    );
    // A cheap mismatch check, NOT a substitute for bounded decoding.
    let signature_matches = match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        _ => false,
    };
    ensure!(
        signature_matches,
        "image signature does not match MIME type"
    );
    Ok(mime)
}

/// Textual projection of USER content only, not whole messages or tool results.
/// Emits a fixed omission marker, never image data or untrusted image metadata.
/// Caller must separately retain the latest structured image and budget it before
/// any summary request. This does not authorize admission or scrub arbitrary text.
pub(super) fn user_evidence_text(content: &Value, limit: usize) -> Result<String> {
    fn append(output: &mut String, part: &str, limit: usize) -> Result<()> {
        ensure!(
            part.len() <= limit.saturating_sub(output.len()),
            "image evidence exceeds byte budget"
        );
        output.push_str(part);
        Ok(())
    }
    let limit = limit.min(512 * 1024);
    let mut output = String::new();
    if let Some(text) = content.as_str() {
        append(&mut output, text, limit)?;
        return Ok(output);
    }
    let blocks = content
        .as_array()
        .context("invalid user evidence content")?;
    let mut images = 0;
    for (index, block) in blocks.iter().enumerate() {
        if index != 0 {
            append(&mut output, "\n", limit)?;
        }
        let part = match block["type"].as_str() {
            Some("text") => wire::string(block, "text")?,
            Some("image") => {
                images += 1;
                ensure!(images <= 1, "at most one image per user message");
                image_mime(block)?;
                "[image omitted from textual evidence]"
            }
            _ => bail!("unsupported user evidence block"),
        };
        append(&mut output, part, limit)?;
    }
    Ok(output)
}

fn user_content(content: &Value) -> Result<Value> {
    // Keep the existing wire representation (including newline joining) for
    // every previously supported text-only user message.
    if content.is_string() {
        return Ok(json!(wire::text(content)?));
    }
    let blocks = content.as_array().context("invalid user content")?;
    if blocks.iter().all(|b| b["type"] == "text") {
        return Ok(json!(wire::text(content)?));
    }
    let mut output = Vec::new();
    let mut images = 0;
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => output.push(json!({
                "type": "input_text", "text": wire::string(block, "text")?
            })),
            Some("image") => {
                images += 1;
                ensure!(images <= 1, "at most one image per user message");
                let mime = image_mime(block)?;
                let data = wire::string(block, "data")?;
                output.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime};base64,{data}"),
                    "detail": "auto"
                }));
            }
            _ => bail!("unsupported user content block"),
        }
    }
    Ok(Value::Array(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(bytes: &[u8], mime: &str) -> Value {
        json!({"type":"image", "mimeType":mime, "data":STANDARD.encode(bytes)})
    }

    #[test]
    fn preserves_text_wire_representation() {
        for (input, expected) in [
            (json!("plain"), json!("plain")),
            (json!([]), json!("")),
            (
                json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]),
                json!("a\nb"),
            ),
        ] {
            assert_eq!(user_content(&input).unwrap(), expected);
        }
    }

    #[test]
    fn maps_image_envelopes_without_textualizing_them() {
        // Signature-only fixtures test the wire envelope, not image decoding.
        for (bytes, mime) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png"),
            (b"\xff\xd8\xff".as_slice(), "image/jpeg"),
        ] {
            let block = image(bytes, mime);
            let result = user_content(&json!([
                {"type":"text","text":"describe"}, block,
                {"type":"text","text":"caption"}
            ]))
            .unwrap();
            assert_eq!(result[0], json!({"type":"input_text","text":"describe"}));
            assert_eq!(result[1]["type"], "input_image");
            assert_eq!(
                result[1]["image_url"],
                format!("data:{mime};base64,{}", STANDARD.encode(bytes))
            );
            assert_eq!(result[2]["text"], "caption");
        }
    }

    #[test]
    fn rejects_urls_paths_unknown_fields_and_invalid_encodings() {
        let valid = image(b"\xff\xd8\xff", "image/jpeg");
        for data in [
            "",
            "%%%",
            "/tmp/private.jpg",
            "https://example.invalid/a",
            "data:image/jpeg;base64,/9j/",
            "/9j/\n",
            // Noncanonical trailing bits (canonical spelling is /9j/AA==).
            "/9j/AB==",
        ] {
            let mut block = valid.clone();
            block["data"] = json!(data);
            assert!(user_content(&json!([block])).is_err());
        }
        let mut extra = valid.clone();
        extra["url"] = json!("https://example.invalid/a");
        assert!(user_content(&json!([extra])).is_err());
        let mut missing = valid;
        missing.as_object_mut().unwrap().remove("data");
        assert!(user_content(&json!([missing])).is_err());
    }

    #[test]
    fn rejects_wrong_mime_signatures_and_multiple_images() {
        for block in [
            image(b"\xff\xd8\xff", "image/png"),
            image(b"not an image", "image/jpeg"),
            image(b"GIF89a", "image/gif"),
            json!({"type":"input_image","image_url":"https://example.invalid"}),
            json!({"type":"text","text":7}),
        ] {
            assert!(user_content(&json!([block])).is_err());
        }
        let block = image(b"\xff\xd8\xff", "image/jpeg");
        assert!(user_content(&json!([block, block])).is_err());
    }

    #[test]
    fn rejects_malformed_shapes_without_echoing_payloads() {
        for input in [
            Value::Null,
            json!(17),
            json!({"type":"text","text":"not an array"}),
            json!([null]),
            json!([{"type":"image","mimeType":7,"data":"/9j/"}]),
            json!([{"type":"image","mimeType":"image/jpeg","data":7}]),
        ] {
            assert!(user_content(&input).is_err());
        }
        let secret = "PRIVATE_SYNTHETIC_PAYLOAD_DO_NOT_ECHO";
        let error = user_content(&json!([{
            "type":"image","mimeType":"image/jpeg","data":secret
        }]))
        .unwrap_err();
        assert!(!format!("{error:#}").contains(secret));
    }

    #[test]
    fn failed_conversion_preserves_input_and_hides_all_untrusted_fields() {
        let secret = "PRIVATE_SYNTHETIC_FIELD_DO_NOT_ECHO";
        let valid = image(b"\xff\xd8\xff", "image/jpeg");
        for field in ["type", "mimeType", "data", "unexpected"] {
            let mut block = valid.clone();
            block[field] = json!(secret);
            // An error after valid leading content must not mutate the message
            // or return partially converted content for a caller to dispatch.
            let input = json!([{"type":"text","text":"caption"}, block]);
            let original = input.clone();
            let error = user_content(&input).unwrap_err();
            assert_eq!(input, original);
            assert!(!format!("{error:#}").contains(secret));
        }
    }

    #[test]
    fn capability_is_explicit_and_rechecked_for_each_conversion() {
        let input = json!([image(b"\xff\xd8\xff", "image/jpeg")]);
        let original = input.clone();
        for capability in [ImageCapability::Unknown, ImageCapability::Unsupported] {
            assert!(user_content_for_capability(&input, capability).is_err());
            assert_eq!(
                user_content_for_capability(&json!("text"), capability).unwrap(),
                json!("text")
            );
            assert!(user_content_for_capability(&json!([null]), capability).is_err());
        }
        assert!(user_content_for_capability(&input, ImageCapability::Supported).is_ok());
        // A previous supported conversion must not authorize a provider switch.
        assert!(user_content_for_capability(&input, ImageCapability::Unknown).is_err());
        assert_eq!(input, original);
    }

    #[test]
    fn request_budget_counts_utf8_escaping_and_adapter_mutations() {
        let content = user_content_for_capability(
            &json!([
                {"type":"text","text":"한글\n\"caption\""},
                image(b"\xff\xd8\xff", "image/jpeg")
            ]),
            ImageCapability::Supported,
        )
        .unwrap();
        let mut body = json!({
            "model":"synthetic-model", "instructions":"synthetic instructions",
            "tools":[{"type":"function","name":"synthetic_tool"}],
            "input":[{"role":"user","content":content}], "store":false
        });
        let expected = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            bounded_request_bytes(&body, expected.len()).unwrap(),
            expected
        );
        assert!(bounded_request_bytes(&body, expected.len() - 1).is_err());
        assert!(bounded_request_bytes(&body, 0).is_err());
        body["stream"] = json!(true);
        assert!(bounded_request_bytes(&body, expected.len()).is_err());
        let streamed = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            bounded_request_bytes(&body, streamed.len()).unwrap(),
            streamed
        );
    }

    #[test]
    fn historical_images_share_http_ceiling_without_payload_echo() {
        let mut bytes = vec![0; MAX_IMAGE_BYTES];
        bytes[..3].copy_from_slice(b"\xff\xd8\xff");
        let content = user_content(&json!([image(&bytes, "image/jpeg")])).unwrap();
        let message = json!({"role":"user","content":content});
        let one = json!({"input":[message.clone()]});
        assert!(bounded_request_bytes(&one, usize::MAX).is_ok());
        let secret = "PRIVATE_SYNTHETIC_REQUEST_DO_NOT_ECHO";
        let two = json!({"instructions":secret,"input":[message.clone(),message]});
        let original = two.clone();
        let error = bounded_request_bytes(&two, usize::MAX).unwrap_err();
        assert!(!format!("{error:#}").contains(secret));
        assert_eq!(two, original);
        // The complete serialized envelope, not just image data, sets the boundary.
        let exact = json!("x".repeat(512 * 1024 - 2));
        assert_eq!(
            bounded_request_bytes(&exact, usize::MAX).unwrap().len(),
            512 * 1024
        );
        let over = json!("x".repeat(512 * 1024 - 1));
        assert!(bounded_request_bytes(&over, usize::MAX).is_err());
    }

    #[test]
    fn evidence_omits_images_preserving_text_order_and_structured_source() {
        for (bytes, mime) in [
            (
                b"\x89PNG\r\n\x1a\nPRIVATE_SYNTHETIC_IMAGE".as_slice(),
                "image/png",
            ),
            (
                b"\xff\xd8\xffPRIVATE_SYNTHETIC_IMAGE".as_slice(),
                "image/jpeg",
            ),
        ] {
            let input = json!([
                {"type":"text","text":"before"}, image(bytes, mime),
                {"type":"text","text":"after"}
            ]);
            let original = input.clone();
            let evidence = user_evidence_text(&input, 1024).unwrap();
            assert_eq!(
                evidence,
                "before\n[image omitted from textual evidence]\nafter"
            );
            assert!(!evidence.contains(&STANDARD.encode(bytes)));
            assert!(!evidence.contains("PRIVATE_SYNTHETIC_IMAGE"));
            assert!(!evidence.contains("data:"));
            assert_eq!(input, original);
            // Projection must not remove the source needed for latest-user retention.
            assert!(user_content_for_capability(&input, ImageCapability::Supported).is_ok());
        }
    }

    #[test]
    fn evidence_preserves_text_and_bounds_utf8_output_without_partial_results() {
        for input in [
            json!(""),
            json!([]),
            json!("한글\n\"caption\""),
            json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]),
        ] {
            let expected = wire::text(&input).unwrap();
            assert_eq!(
                user_evidence_text(&input, expected.len()).unwrap(),
                expected
            );
            if !expected.is_empty() {
                assert!(user_evidence_text(&input, expected.len() - 1).is_err());
            }
        }
        let input = json!([image(b"\xff\xd8\xff", "image/jpeg")]);
        let expected = "[image omitted from textual evidence]";
        assert_eq!(
            user_evidence_text(&input, expected.len()).unwrap(),
            expected
        );
        assert!(user_evidence_text(&input, expected.len() - 1).is_err());
        assert!(user_evidence_text(&json!("x".repeat(512 * 1024)), usize::MAX).is_ok());
        assert!(user_evidence_text(&json!("x".repeat(512 * 1024 + 1)), usize::MAX).is_err());
    }

    #[test]
    fn evidence_rejects_malformed_blocks_without_echo_or_mutation() {
        let secret = "/private/SYNTHETIC_DO_NOT_ECHO";
        let valid = image(b"\xff\xd8\xff", "image/jpeg");
        let mut extra = valid.clone();
        extra["path"] = json!(secret);
        for input in [
            Value::Null,
            json!([null]),
            json!([{"type":"text","text":7}]),
            json!([{"type":"image","mimeType":"image/jpeg","data":secret}]),
            json!([{"type":"input_image","image_url":secret}]),
            json!([{"type":"toolCall","arguments":secret}]),
            json!([extra]),
            json!([valid.clone(), valid]),
        ] {
            let original = input.clone();
            let error = user_evidence_text(&input, 1024).unwrap_err();
            assert!(!format!("{error:#}").contains(secret));
            assert_eq!(input, original);
        }
        let input = json!([{"type":"text","text":secret}, {"type":"unknown"}]);
        let error = user_evidence_text(&input, 1024).unwrap_err();
        assert!(!format!("{error:#}").contains(secret));
    }

    #[test]
    fn checkpoint_retains_latest_structured_image_without_changing_source() {
        // Exercise the existing checkpoint retention boundary, not summarize:
        // production summarize still copies raw content and is NOT image-safe.
        let old = image(b"\xff\xd8\xffOLD_SYNTHETIC_IMAGE", "image/jpeg");
        let current = image(b"\x89PNG\r\n\x1a\nCURRENT_SYNTHETIC_IMAGE", "image/png");
        let latest = json!({"role":"user","content":[
            {"type":"text","text":"current caption"}, current
        ]});
        let summary = crate::compaction::empty_summary();
        let messages = vec![
            json!({"role":"user","content":[old]}),
            latest.clone(),
            // A later checkpoint is a user-shaped message, not the current user.
            crate::compaction::context_message(&summary),
        ];
        let original = messages.clone();
        let checkpoint = crate::compaction::checkpoint_messages(&summary, &messages).unwrap();
        assert_eq!(checkpoint.len(), 2);
        assert_eq!(checkpoint[0], latest);
        assert_eq!(messages, original);
        let evidence = user_evidence_text(&checkpoint[0]["content"], 1024).unwrap();
        assert_eq!(
            evidence,
            "current caption\n[image omitted from textual evidence]"
        );
        assert!(
            user_content_for_capability(&checkpoint[0]["content"], ImageCapability::Supported)
                .is_ok()
        );
        let checkpoint_text = serde_json::to_string(&checkpoint[1]).unwrap();
        for bytes in [
            b"\xff\xd8\xffOLD_SYNTHETIC_IMAGE".as_slice(),
            b"\x89PNG\r\n\x1a\nCURRENT_SYNTHETIC_IMAGE".as_slice(),
        ] {
            assert!(!checkpoint_text.contains(&STANDARD.encode(bytes)));
        }
        // Repeated checkpoints must preserve the original structured user too.
        let repeated = crate::compaction::checkpoint_messages(&summary, &checkpoint).unwrap();
        assert_eq!(repeated[0], latest);
    }

    #[test]
    fn projected_evidence_requires_separate_serialized_request_budget() {
        // A successful text projection is not permission to dispatch a summary.
        // JSON escaping and the surrounding ledger/request consume more bytes.
        let input = json!([
            {"type":"text","text":"한글\n\"caption\"\\"},
            image(b"\xff\xd8\xffPRIVATE_SYNTHETIC_IMAGE", "image/jpeg")
        ]);
        let projection = user_evidence_text(&input, 1024).unwrap();
        assert_eq!(
            user_evidence_text(&input, projection.len()).unwrap(),
            projection
        );
        let ledger = serde_json::to_string(&json!([
            {"role":"user","content":projection}
        ]))
        .unwrap();
        let body = json!({
            "model":"synthetic-model", "instructions":"synthetic summary",
            "input":[{"role":"user","content":ledger}], "tools":[], "store":false
        });
        let expected = serde_json::to_vec(&body).unwrap();
        assert!(expected.len() > projection.len());
        assert!(bounded_request_bytes(&body, projection.len()).is_err());
        assert!(bounded_request_bytes(&body, expected.len() - 1).is_err());
        assert_eq!(
            bounded_request_bytes(&body, expected.len()).unwrap(),
            expected
        );
        let serialized = String::from_utf8(expected).unwrap();
        assert!(!serialized.contains("PRIVATE_SYNTHETIC_IMAGE"));
        assert!(!serialized.contains(input[1]["data"].as_str().unwrap()));
        assert!(!serialized.contains("image/jpeg"));
        assert!(!serialized.contains("data:"));
    }

    #[test]
    fn enforces_decoded_and_encoded_limits() {
        let mut bytes = vec![0; MAX_IMAGE_BYTES];
        bytes[..3].copy_from_slice(b"\xff\xd8\xff");
        assert!(user_content(&json!([image(&bytes, "image/jpeg")])).is_ok());
        bytes.push(0);
        assert!(user_content(&json!([image(&bytes, "image/jpeg")])).is_err());
    }
}
