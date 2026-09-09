//! Test-only pixel normalization experiment; NOT production admission.
//! Decoder allocation limits are best-effort, not process RSS/time limits.
//! A supervised resource-limited subprocess is still required before enabling
//! this on untrusted production inputs. Never expose decoder error strings.
use anyhow::{Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::io::Cursor;

const MAX_INPUT: usize = 192 * 1024;
const MAX_SIDE: u32 = 2048;
const MAX_PIXELS: u64 = 1024 * 1024;
const DECODER_ALLOCATION: u64 = 32 * 1024 * 1024;

/// Decode under dimension/pixel checks, apply EXIF orientation, then discard
/// original metadata. Caller must re-encode these pixels, never forward input.
fn normalized_pixels(bytes: &[u8], mime: &str) -> Result<DynamicImage> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_INPUT,
        "image input exceeds limit"
    );
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => anyhow::bail!("unsupported image MIME type"),
    };
    ensure!(
        image::guess_format(bytes).ok() == Some(format),
        "image format mismatch"
    );
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(DECODER_ALLOCATION);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let private_error = |_| anyhow::anyhow!("image pixel validation failed");
    let mut decoder = reader.into_decoder().map_err(private_error)?;
    let (width, height) = decoder.dimensions();
    ensure!(
        width > 0
            && height > 0
            && width <= MAX_SIDE
            && height <= MAX_SIDE
            && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "image dimensions exceed limit"
    );
    let orientation = decoder.orientation().map_err(private_error)?;
    let mut pixels = DynamicImage::from_decoder(decoder).map_err(private_error)?;
    pixels.apply_orientation(orientation);
    Ok(pixels)
}

/// Re-encode only oriented pixels as PNG; never copy source metadata. The
/// writer caps logical output bytes, not encoder allocations or process RSS.
pub(crate) fn normalized_png(bytes: &[u8], mime: &str, limit: usize) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    use std::io::Write;

    struct BoundedOutput {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl Write for BoundedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("normalized image exceeds limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    ensure!(limit > 0, "normalized image exceeds limit");
    let pixels = normalized_pixels(bytes, mime)?.to_rgba8();
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        limit: limit.min(MAX_INPUT),
        exceeded: false,
    };
    image::codecs::png::PngEncoder::new(&mut output)
        .write_image(
            pixels.as_raw(),
            pixels.width(),
            pixels.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|_| anyhow::anyhow!("image normalization failed"))?;
    // Encoder finalization may discard a writer error; retain a sticky flag.
    ensure!(!output.exceeded, "normalized image exceeds limit");
    Ok(output.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageEncoder};

    fn fixture(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let pixels = DynamicImage::new_rgb8(width, height);
        let mut output = Cursor::new(Vec::new());
        pixels.write_to(&mut output, format).unwrap();
        output.into_inner()
    }

    #[test]
    fn decodes_real_png_and_jpeg_without_mutating_input() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
        ] {
            let bytes = fixture(format, 3, 2);
            let original = bytes.clone();
            let pixels = normalized_pixels(&bytes, mime).unwrap();
            assert_eq!(pixels.dimensions(), (3, 2));
            assert_eq!(bytes, original);
            // Pixels, rather than the original metadata-bearing bytes, are the
            // sole output. A future bounded encoder owns wire MIME and size.
            assert_eq!(pixels.to_rgb8().as_raw(), &[0; 18]);
        }
    }

    #[test]
    fn rejects_signature_only_truncated_and_mismatched_inputs_privately() {
        let png = fixture(ImageFormat::Png, 3, 2);
        let jpeg = fixture(ImageFormat::Jpeg, 3, 2);
        for (bytes, mime) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png"),
            (b"\xff\xd8\xff".as_slice(), "image/jpeg"),
            (&png[..png.len() / 2], "image/png"),
            (&jpeg[..jpeg.len() / 2], "image/jpeg"),
            (&png, "image/jpeg"),
            (&jpeg, "image/png"),
            (b"PRIVATE_PAYLOAD_DO_NOT_ECHO", "image/png"),
            (&png, "image/gif"),
        ] {
            let error = normalized_pixels(bytes, mime).unwrap_err();
            assert!(!format!("{error:#}").contains("PRIVATE_PAYLOAD"));
        }
    }

    #[test]
    fn rejects_compressed_dimension_and_pixel_bombs() {
        for (width, height) in [(MAX_SIDE + 1, 1), (1, MAX_SIDE + 1), (1025, 1024)] {
            let bytes = fixture(ImageFormat::Png, width, height);
            assert!(bytes.len() < MAX_INPUT);
            assert!(normalized_pixels(&bytes, "image/png").is_err());
        }
        let bytes = fixture(ImageFormat::Png, 1024, 1024);
        assert!(normalized_pixels(&bytes, "image/png").is_ok());
    }

    #[test]
    fn rejects_input_over_budget_before_decode() {
        for bytes in [vec![], vec![0; MAX_INPUT + 1]] {
            assert_eq!(
                normalized_pixels(&bytes, "image/png")
                    .unwrap_err()
                    .to_string(),
                "image input exceeds limit"
            );
        }
    }

    #[test]
    fn rejects_corrupt_png_pixel_stream() {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[12, 34, 56], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let start = bytes.windows(4).position(|w| w == b"IDAT").unwrap() + 4;
        bytes[start] ^= 0xff;
        assert!(normalized_pixels(&bytes, "image/png").is_err());
    }

    #[test]
    fn applies_jpeg_exif_orientation_before_discarding_metadata() {
        let jpeg = fixture(ImageFormat::Jpeg, 3, 2);
        // Little-endian TIFF: one SHORT orientation tag with value 6 (90 CW).
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend_from_slice(b"\xff\xe1");
        bytes.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(exif);
        bytes.extend_from_slice(&jpeg[2..]);
        assert_eq!(
            normalized_pixels(&bytes, "image/jpeg")
                .unwrap()
                .dimensions(),
            (2, 3)
        );
    }

    #[test]
    fn normalized_output_exact_budget_and_round_trip() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
        ] {
            let source = fixture(format, 3, 2);
            let original = source.clone();
            let output = normalized_png(&source, mime, usize::MAX).unwrap();
            assert_eq!(image::guess_format(&output).unwrap(), ImageFormat::Png);
            assert_eq!(
                normalized_pixels(&output, "image/png")
                    .unwrap()
                    .dimensions(),
                (3, 2)
            );
            assert_eq!(normalized_png(&source, mime, output.len()).unwrap(), output);
            assert!(normalized_png(&source, mime, output.len() - 1).is_err());
            assert!(normalized_png(&source, mime, 0).is_err());
            assert_eq!(source, original);
        }
    }

    #[test]
    fn drops_jpeg_comment_metadata_in_normalized_output() {
        let source = fixture(ImageFormat::Jpeg, 3, 2);
        let secret = b"PRIVATE_CAMERA_LOCATION";
        let mut annotated = source[..2].to_vec();
        annotated.extend_from_slice(b"\xff\xfe");
        annotated.extend_from_slice(&((secret.len() + 2) as u16).to_be_bytes());
        annotated.extend_from_slice(secret);
        annotated.extend_from_slice(&source[2..]);
        let output = normalized_png(&annotated, "image/jpeg", MAX_INPUT).unwrap();
        assert!(!output.windows(secret.len()).any(|w| w == secret));
        assert_eq!(
            output,
            normalized_png(&source, "image/jpeg", MAX_INPUT).unwrap()
        );
        // Fresh encoding emits only pixel-bearing PNG chunks, no text/EXIF.
        let mut offset = 8;
        while offset < output.len() {
            let len = u32::from_be_bytes(output[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &output[offset + 4..offset + 8];
            assert!([b"IHDR", b"IDAT", b"IEND"].contains(&kind.try_into().unwrap()));
            offset += 12 + len;
        }
        assert_eq!(offset, output.len());
    }
}
