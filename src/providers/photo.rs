//! Inbound photos/documents (JPEG, PNG, HEIC/HEIF) → JPEG for the vision model. The EXIF capture
//! time for weigh-in backfill is `domain::body::photo_taken_at`.
//!
//! HEIC needs the `heic` feature (default; system libheif). Without it HEIC is PHOTO_UNSUPPORTED.
use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

use crate::error::{Error, Result};

/// Longest edge sent to the vision model.
pub const MAX_EDGE: u32 = 1600;
/// Upper bound on decoded pixel count (50 MP covers 48 MP phone photos while rejecting bombs).
const MAX_DECODE_PIXELS: u64 = 50_000_000;
const JPEG_QUALITY: u8 = 85;

/// A JPEG ready for the vision model, upright and at most `max_edge` on its longest side.
#[derive(Clone, Debug)]
pub struct Photo {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn unsupported() -> Error {
    Error::domain("PHOTO_UNSUPPORTED", "Photo format not supported; send a JPEG, PNG or HEIC")
}

fn invalid() -> Error {
    Error::domain("PHOTO_INVALID", "Photo could not be decoded")
}

/// ISO-BMFF `ftyp` with an HEIF brand (iPhone HEIC, HEIF sequences).
pub fn is_heif(bytes: &[u8]) -> bool {
    const BRANDS: [&[u8; 4]; 8] = [b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1"];
    bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && BRANDS.iter().any(|b| &bytes[8..12] == *b)
}

/// Decode, apply EXIF orientation, downscale to `max_edge` and re-encode as JPEG.
pub fn to_jpeg(bytes: &[u8], max_edge: u32) -> Result<Photo> {
    let image = if is_heif(bytes) { decode_heif(bytes)? } else { decode_raster(bytes)? };
    let image = if image.width().max(image.height()) > max_edge {
        image.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };
    let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|_| invalid())?;
    Ok(Photo { jpeg, width: rgb.width(), height: rgb.height() })
}

fn check_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(invalid());
    }
    if u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        crate::bail!("PHOTO_TOO_LARGE", "Photo exceeds 50 MP size limit");
    }
    Ok(())
}

fn decode_raster(bytes: &[u8]) -> Result<DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(unsupported());
    }
    let mut decoder = reader.into_decoder().map_err(|_| invalid())?;
    let (w, h) = decoder.dimensions();
    check_dimensions(w, h)?;
    let orientation = decoder.orientation().map_err(|_| invalid())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| invalid())?;
    image.apply_orientation(orientation);
    Ok(image)
}

#[cfg(feature = "heic")]
fn decode_heif(bytes: &[u8]) -> Result<DynamicImage> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let context = HeifContext::read_from_bytes(bytes).map_err(|_| invalid())?;
    let handle = context.primary_image_handle().map_err(|_| invalid())?;
    check_dimensions(handle.width(), handle.height())?;
    // libheif applies the container's rotation/mirroring while decoding.
    let decoded = LibHeif::new().decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None).map_err(|_| invalid())?;
    let plane = decoded.planes().interleaved.ok_or_else(invalid)?;
    let (width, height) = (plane.width, plane.height);
    let row = width as usize * 3;
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        pixels.extend_from_slice(plane.data.get(y * plane.stride..y * plane.stride + row).ok_or_else(invalid)?);
    }
    Ok(DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, pixels).ok_or_else(invalid)?))
}

#[cfg(not(feature = "heic"))]
fn decode_heif(_: &[u8]) -> Result<DynamicImage> {
    Err(unsupported())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFSET_JPG: &[u8] = include_bytes!("testdata/photo/offset.jpg");
    const PLAIN_PNG: &[u8] = include_bytes!("testdata/photo/plain.png");

    #[test]
    fn decodes_upright_jpeg() {
        // EXIF orientation 6: the 64x48 pixels display as 48x64.
        let photo = to_jpeg(OFFSET_JPG, MAX_EDGE).unwrap();
        assert_eq!((photo.width, photo.height), (48, 64));
        let small = to_jpeg(PLAIN_PNG, 32).unwrap();
        assert_eq!((small.width, small.height), (32, 24));
        assert_eq!(to_jpeg(b"GIF89a....", MAX_EDGE).unwrap_err().code(), "PHOTO_UNSUPPORTED");
    }

    #[test]
    fn rejects_oversized_image_dimensions_before_decoding() {
        assert!(check_dimensions(8000, 6000).is_ok()); // 48 MP phone photo
        assert_eq!(check_dimensions(10_000, 10_000).unwrap_err().code(), "PHOTO_TOO_LARGE");
        assert_eq!(check_dimensions(0, 100).unwrap_err().code(), "PHOTO_INVALID");
        // Patch PLAIN_PNG's IHDR width/height to 10_000 x 10_000 (100 MP) with a valid CRC so the
        // PNG header parses cleanly and decode_raster rejects it before allocating pixel buffers.
        let mut bomb = PLAIN_PNG.to_vec();
        bomb[16..20].copy_from_slice(&10_000u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&10_000u32.to_be_bytes());
        let crc = crc32(&bomb[12..29]);
        bomb[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(to_jpeg(&bomb, MAX_EDGE).unwrap_err().code(), "PHOTO_TOO_LARGE");
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[cfg(feature = "heic")]
    #[test]
    fn decodes_heic() {
        let heic = include_bytes!("testdata/photo/offset.heic");
        assert!(is_heif(heic));
        let photo = to_jpeg(heic, MAX_EDGE).unwrap();
        assert!(photo.jpeg.starts_with(&[0xFF, 0xD8]) && photo.width.max(photo.height) == 64);
    }
}
