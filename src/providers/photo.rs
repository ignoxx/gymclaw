//! Inbound photos/documents (JPEG, PNG, HEIC/HEIF) → JPEG for the vision model, and the EXIF
//! capture time for weigh-in backfill (port of `services/body.py::photo_taken_at`).
//!
//! HEIC needs the `heic` feature (default; system libheif). Without it HEIC is PHOTO_UNSUPPORTED.
use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use jiff::civil;
use jiff::tz::{Offset, TimeZone};

use crate::error::{Error, Result};
use crate::time::Ts;

/// Longest edge sent to the vision model.
pub const MAX_EDGE: u32 = 1600;
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
    Ok(Photo {
        jpeg,
        width: rgb.width(),
        height: rgb.height(),
    })
}

fn decode_raster(bytes: &[u8]) -> Result<DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(unsupported());
    }
    let mut decoder = reader.into_decoder().map_err(|_| invalid())?;
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

/// When the photo was taken: EXIF DateTimeOriginal (else DateTime), with OffsetTimeOriginal when
/// present, else in `zone`. Files without readable EXIF fall back to the first EXIF-style stamp in
/// the first 256 KiB, like the Python version did for HEIC. Fails with PHOTO_DATE_UNKNOWN.
pub fn taken_at(bytes: &[u8], zone: &TimeZone) -> Result<Ts> {
    let (stamp, offset) = match exif::Reader::new().read_from_container(&mut Cursor::new(bytes)) {
        Ok(data) => {
            let text = |tag: exif::Tag, ifd: exif::In| data.get_field(tag, ifd).and_then(|f| ascii(&f.value));
            let stamp = text(exif::Tag::DateTimeOriginal, exif::In::PRIMARY).or_else(|| text(exif::Tag::DateTime, exif::In::PRIMARY));
            (stamp, text(exif::Tag::OffsetTimeOriginal, exif::In::PRIMARY))
        }
        Err(_) => (first_stamp(&bytes[..bytes.len().min(1 << 18)]), None),
    };
    let unknown = || {
        Error::domain(
            "PHOTO_DATE_UNKNOWN",
            "Photo has no capture date (Telegram strips it from compressed photos). Ask for the original sent as a file, or pass the date with --at.",
        )
    };
    let stamp = stamp.ok_or_else(unknown)?;
    let taken = civil::DateTime::strptime("%Y:%m:%d %H:%M:%S", stamp.trim_matches(['\0', ' '])).map_err(|_| unknown())?;
    let zoned = match offset.as_deref().and_then(parse_offset) {
        Some(offset) => taken.to_zoned(TimeZone::fixed(offset))?,
        None => taken.to_zoned(zone.clone())?,
    };
    Ok(Ts(zoned.timestamp()))
}

fn ascii(value: &exif::Value) -> Option<String> {
    match value {
        exif::Value::Ascii(parts) => parts
            .first()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .filter(|s| !s.trim_matches(['\0', ' ']).is_empty()),
        _ => None,
    }
}

/// `+02:00` / `-0530` (Python `%z`).
fn parse_offset(text: &str) -> Option<Offset> {
    let text = text.trim_matches(['\0', ' ']);
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = text[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (hours, minutes): (i32, i32) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()
}

/// First `YYYY:MM:DD HH:MM:SS` in raw bytes.
fn first_stamp(bytes: &[u8]) -> Option<String> {
    let pattern = regex::bytes::Regex::new(r"(\d{4}):(\d{2}):(\d{2}) (\d{2}):(\d{2}):(\d{2})").expect("valid regex");
    pattern.find(bytes).map(|m| String::from_utf8_lossy(m.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFSET_JPG: &[u8] = include_bytes!("testdata/photo/offset.jpg");
    const LOCAL_JPG: &[u8] = include_bytes!("testdata/photo/local.jpg");
    const PLAIN_PNG: &[u8] = include_bytes!("testdata/photo/plain.png");
    const OFFSET_HEIC: &[u8] = include_bytes!("testdata/photo/offset.heic");

    /// Fixtures from Pillow (`heif-enc` for the HEIC); expectations from Python's `photo_taken_at`.
    #[test]
    fn capture_time_matches_python() {
        let berlin = TimeZone::get("Europe/Berlin").unwrap();
        assert_eq!(taken_at(OFFSET_JPG, &berlin).unwrap().iso(), "2026-10-12T05:41:05+00:00");
        assert_eq!(taken_at(LOCAL_JPG, &berlin).unwrap().iso(), "2026-03-29T04:15:00+00:00");
        assert_eq!(taken_at(PLAIN_PNG, &berlin).unwrap_err().code(), "PHOTO_DATE_UNKNOWN");
        // Python read the first stamp (DateTime, 2026:10:01 09:00:00) from HEIC; EXIF parsing gets the real capture time.
        assert_eq!(taken_at(OFFSET_HEIC, &berlin).unwrap().iso(), "2026-10-12T05:41:05+00:00");
    }

    #[test]
    fn decodes_upright_jpeg() {
        // EXIF orientation 6: the 64x48 pixels display as 48x64.
        let photo = to_jpeg(OFFSET_JPG, MAX_EDGE).unwrap();
        assert_eq!((photo.width, photo.height), (48, 64));
        let small = to_jpeg(PLAIN_PNG, 32).unwrap();
        assert_eq!((small.width, small.height), (32, 24));
        assert_eq!(to_jpeg(b"GIF89a....", MAX_EDGE).unwrap_err().code(), "PHOTO_UNSUPPORTED");
    }

    #[cfg(feature = "heic")]
    #[test]
    fn decodes_heic() {
        assert!(is_heif(OFFSET_HEIC));
        let photo = to_jpeg(OFFSET_HEIC, MAX_EDGE).unwrap();
        assert!(photo.jpeg.starts_with(&[0xFF, 0xD8]) && photo.width.max(photo.height) == 64);
    }
}
