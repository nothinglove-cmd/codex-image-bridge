use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context, Result};
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::session::SessionImage;

pub const MAX_ENCODED_BYTES: usize = 128 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = MAX_ENCODED_BYTES as u64;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const PNG_CRC_TABLE: [u32; 256] = png_crc_table();

#[derive(Clone, Debug)]
pub struct SavedImage {
    pub path: PathBuf,
    pub sha256: String,
}

pub fn decode_and_save(
    output_root: &Path,
    thread_id: &str,
    image: &SessionImage,
) -> Result<SavedImage> {
    let decoded = image.result.as_deref().map(decode).transpose()?;

    if let Some(saved_path) = image.saved_path.as_deref() {
        if let Ok(existing) = inspect_file(saved_path) {
            let hash_matches = decoded
                .as_ref()
                .is_none_or(|decoded| decoded.sha256 == existing.sha256);
            if hash_matches {
                return Ok(existing);
            }
        }
    }

    let decoded = decoded.context("image has neither valid saved_path nor result")?;
    let thread_dir = output_root.join(sanitize(thread_id));
    fs::create_dir_all(&thread_dir)
        .with_context(|| format!("failed to create {}", thread_dir.display()))?;
    let destination = thread_dir.join(format!("{}.{}", decoded.sha256, decoded.extension));
    atomic_write_if_changed(&destination, &decoded.bytes, &decoded.sha256)?;
    let path = destination
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", destination.display()))?;
    Ok(SavedImage {
        path,
        sha256: decoded.sha256,
    })
}

pub fn default_output_dir() -> PathBuf {
    codex_home().join("generated-images")
}

pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|path| path.join(".codex"))
        })
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

pub fn sanitize(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => character,
            _ => '_',
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn decode(value: &str) -> Result<DecodedImage> {
    let (encoded, declared_extension) = if value.starts_with("data:") {
        let (metadata, body) = value.split_once(',').context("invalid image data URI")?;
        let extension = match metadata.to_ascii_lowercase().as_str() {
            "data:image/png;base64" => "png",
            "data:image/jpeg;base64" => "jpg",
            "data:image/webp;base64" => "webp",
            _ => bail!("unsupported image data URI"),
        };
        (body, Some(extension))
    } else {
        (value, None)
    };
    if encoded.len() > MAX_ENCODED_BYTES {
        bail!("encoded image exceeds 128 MiB limit");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("invalid image base64")?;
    let extension = validate_image(&bytes)?;
    if declared_extension.is_some_and(|declared| declared != extension) {
        bail!("image data URI type does not match its content");
    }
    let sha256 = sha256(&bytes);
    Ok(DecodedImage {
        bytes,
        extension,
        sha256,
    })
}

fn inspect_file(path: &Path) -> Result<SavedImage> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat saved image {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
        bail!("saved image is not a supported regular file");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .with_context(|| format!("failed to open saved image {}", path.display()))?
        .read_to_end(&mut bytes)?;
    validate_image(&bytes)?;
    let sha256 = sha256(&bytes);
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize saved image {}", path.display()))?;
    Ok(SavedImage { path, sha256 })
}

fn validate_image(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        validate_png(bytes)?;
        Ok("png")
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        validate_jpeg(bytes)?;
        Ok("jpg")
    } else if bytes.starts_with(b"RIFF") {
        validate_webp(bytes)?;
        Ok("webp")
    } else {
        bail!("unsupported image signature")
    }
}

fn validate_png(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 45 {
        bail!("truncated PNG");
    }
    let mut offset = 8usize;
    let mut first_chunk = true;
    let mut saw_idat = false;
    loop {
        let header_end = offset.checked_add(8).context("PNG offset overflow")?;
        if header_end > bytes.len() {
            bail!("truncated PNG chunk header");
        }
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let chunk_type = &bytes[offset + 4..offset + 8];
        if !chunk_type.iter().all(u8::is_ascii_alphabetic) {
            bail!("invalid PNG chunk type");
        }
        let data_end = header_end
            .checked_add(length)
            .context("PNG chunk length overflow")?;
        let chunk_end = data_end.checked_add(4).context("PNG CRC overflow")?;
        if chunk_end > bytes.len() {
            bail!("truncated PNG chunk");
        }
        let expected_crc = u32::from_be_bytes(bytes[data_end..chunk_end].try_into().unwrap());
        if png_crc32(chunk_type, &bytes[header_end..data_end]) != expected_crc {
            bail!("PNG CRC mismatch");
        }

        match chunk_type {
            b"IHDR" => {
                if !first_chunk || length != 13 {
                    bail!("invalid PNG IHDR");
                }
                let data = &bytes[header_end..data_end];
                let width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let bit_depth = data[8];
                let color_type = data[9];
                let valid_depth = match color_type {
                    0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
                    2 | 4 | 6 => matches!(bit_depth, 8 | 16),
                    3 => matches!(bit_depth, 1 | 2 | 4 | 8),
                    _ => false,
                };
                if width == 0
                    || height == 0
                    || !valid_depth
                    || data[10] != 0
                    || data[11] != 0
                    || data[12] > 1
                {
                    bail!("invalid PNG IHDR fields");
                }
            }
            b"IDAT" => saw_idat = true,
            b"IEND" => {
                if length != 0 || !saw_idat || chunk_end != bytes.len() {
                    bail!("invalid PNG IEND");
                }
                return Ok(());
            }
            _ if first_chunk => bail!("PNG does not start with IHDR"),
            _ => {}
        }
        first_chunk = false;
        offset = chunk_end;
    }
}

fn png_crc32(chunk_type: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in chunk_type.iter().chain(data) {
        let index = ((crc ^ u32::from(*byte)) & 0xff) as usize;
        crc = PNG_CRC_TABLE[index] ^ (crc >> 8);
    }
    !crc
}

const fn png_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ 0xedb8_8320
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

fn validate_jpeg(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 4 || bytes[..2] != [0xFF, 0xD8] {
        bail!("invalid JPEG SOI");
    }
    let mut offset = 2usize;
    let mut pending_marker = None;
    let mut saw_frame = false;
    loop {
        let marker = if let Some(marker) = pending_marker.take() {
            marker
        } else {
            if offset >= bytes.len() || bytes[offset] != 0xFF {
                bail!("invalid JPEG marker boundary");
            }
            while offset < bytes.len() && bytes[offset] == 0xFF {
                offset += 1;
            }
            if offset >= bytes.len() {
                bail!("truncated JPEG marker");
            }
            let marker = bytes[offset];
            offset += 1;
            marker
        };

        match marker {
            0xD9 => {
                if !saw_frame || offset != bytes.len() {
                    bail!("invalid JPEG EOI");
                }
                return Ok(());
            }
            0xD8 | 0x00 | 0xFF => bail!("invalid JPEG marker"),
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }

        let length_end = offset.checked_add(2).context("JPEG offset overflow")?;
        if length_end > bytes.len() {
            bail!("truncated JPEG segment length");
        }
        let length = u16::from_be_bytes(bytes[offset..length_end].try_into().unwrap()) as usize;
        if length < 2 {
            bail!("invalid JPEG segment length");
        }
        let segment_end = offset
            .checked_add(length)
            .context("JPEG segment length overflow")?;
        if segment_end > bytes.len() {
            bail!("truncated JPEG segment");
        }
        let data = &bytes[length_end..segment_end];

        if is_jpeg_frame_marker(marker) {
            if data.len() < 6 {
                bail!("truncated JPEG frame");
            }
            let height = u16::from_be_bytes(data[1..3].try_into().unwrap());
            let width = u16::from_be_bytes(data[3..5].try_into().unwrap());
            let components = usize::from(data[5]);
            if width == 0 || height == 0 || components == 0 || data.len() != 6 + 3 * components {
                bail!("invalid JPEG frame");
            }
            saw_frame = true;
        }

        offset = segment_end;
        if marker == 0xDA {
            if data.len() < 4 {
                bail!("invalid JPEG scan header");
            }
            let components = usize::from(data[0]);
            if components == 0 || data.len() != 4 + 2 * components {
                bail!("invalid JPEG scan header");
            }
            let (marker, next_offset) = next_jpeg_scan_marker(bytes, offset)?;
            pending_marker = Some(marker);
            offset = next_offset;
        }
    }
}

fn is_jpeg_frame_marker(marker: u8) -> bool {
    matches!(
        marker,
        0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
    )
}

fn next_jpeg_scan_marker(bytes: &[u8], mut offset: usize) -> Result<(u8, usize)> {
    while offset < bytes.len() {
        if bytes[offset] != 0xFF {
            offset += 1;
            continue;
        }
        offset += 1;
        while offset < bytes.len() && bytes[offset] == 0xFF {
            offset += 1;
        }
        if offset >= bytes.len() {
            bail!("truncated JPEG scan");
        }
        let marker = bytes[offset];
        offset += 1;
        match marker {
            0x00 | 0xD0..=0xD7 => continue,
            _ => return Ok((marker, offset)),
        }
    }
    bail!("JPEG has no EOI")
}

fn validate_webp(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        bail!("invalid WebP header");
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if riff_size.checked_add(8) != Some(bytes.len()) {
        bail!("invalid WebP RIFF length");
    }

    let mut offset = 12usize;
    let mut saw_image = false;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).context("WebP offset overflow")?;
        if header_end > bytes.len() {
            bail!("truncated WebP chunk header");
        }
        let chunk_type = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(bytes[offset + 4..header_end].try_into().unwrap()) as usize;
        let data_end = header_end
            .checked_add(length)
            .context("WebP chunk length overflow")?;
        let chunk_end = data_end
            .checked_add(length & 1)
            .context("WebP padding overflow")?;
        if chunk_end > bytes.len() {
            bail!("truncated WebP chunk");
        }
        if length & 1 == 1 && bytes[data_end] != 0 {
            bail!("invalid WebP padding");
        }
        let data = &bytes[header_end..data_end];
        match chunk_type {
            b"VP8 " => {
                validate_vp8(data)?;
                saw_image = true;
            }
            b"VP8L" => {
                validate_vp8l(data)?;
                saw_image = true;
            }
            b"VP8X" => validate_vp8x(data)?,
            b"ANMF" => {
                validate_anmf(data)?;
                saw_image = true;
            }
            _ => {}
        }
        offset = chunk_end;
    }
    if !saw_image {
        bail!("WebP has no image data");
    }
    Ok(())
}

fn validate_vp8(data: &[u8]) -> Result<()> {
    if data.len() < 10 || data[3..6] != [0x9D, 0x01, 0x2A] {
        bail!("invalid WebP VP8 frame");
    }
    let width = u16::from_le_bytes(data[6..8].try_into().unwrap()) & 0x3fff;
    let height = u16::from_le_bytes(data[8..10].try_into().unwrap()) & 0x3fff;
    if width == 0 || height == 0 {
        bail!("invalid WebP VP8 dimensions");
    }
    Ok(())
}

fn validate_vp8l(data: &[u8]) -> Result<()> {
    if data.len() < 5 || data[0] != 0x2f || data[4] >> 5 != 0 {
        bail!("invalid WebP VP8L frame");
    }
    let width = 1 + u32::from(data[1]) + ((u32::from(data[2]) & 0x3f) << 8);
    let height = 1
        + (u32::from(data[2]) >> 6)
        + (u32::from(data[3]) << 2)
        + ((u32::from(data[4]) & 0x0f) << 10);
    if width == 0 || height == 0 {
        bail!("invalid WebP VP8L dimensions");
    }
    Ok(())
}

fn validate_vp8x(data: &[u8]) -> Result<()> {
    if data.len() != 10 || data[1..4] != [0, 0, 0] {
        bail!("invalid WebP VP8X header");
    }
    let width = 1 + read_u24_le(&data[4..7]);
    let height = 1 + read_u24_le(&data[7..10]);
    if width == 0 || height == 0 {
        bail!("invalid WebP VP8X dimensions");
    }
    Ok(())
}

fn validate_anmf(data: &[u8]) -> Result<()> {
    if data.len() < 24 {
        bail!("truncated WebP animation frame");
    }
    let width = 1 + read_u24_le(&data[6..9]);
    let height = 1 + read_u24_le(&data[9..12]);
    if width == 0 || height == 0 {
        bail!("invalid WebP animation dimensions");
    }
    let mut offset = 16usize;
    let mut saw_image = false;
    while offset < data.len() {
        let header_end = offset.checked_add(8).context("WebP ANMF overflow")?;
        if header_end > data.len() {
            bail!("truncated WebP ANMF chunk");
        }
        let length = u32::from_le_bytes(data[offset + 4..header_end].try_into().unwrap()) as usize;
        let data_end = header_end
            .checked_add(length)
            .context("WebP ANMF length overflow")?;
        let chunk_end = data_end
            .checked_add(length & 1)
            .context("WebP ANMF padding overflow")?;
        if chunk_end > data.len() {
            bail!("truncated WebP ANMF data");
        }
        match &data[offset..offset + 4] {
            b"VP8 " => {
                validate_vp8(&data[header_end..data_end])?;
                saw_image = true;
            }
            b"VP8L" => {
                validate_vp8l(&data[header_end..data_end])?;
                saw_image = true;
            }
            _ => {}
        }
        offset = chunk_end;
    }
    if !saw_image {
        bail!("WebP animation frame has no image data");
    }
    Ok(())
}

fn read_u24_le(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn atomic_write_if_changed(destination: &Path, bytes: &[u8], expected_hash: &str) -> Result<()> {
    if destination.is_file()
        && inspect_file(destination).is_ok_and(|existing| existing.sha256 == expected_hash)
    {
        return Ok(());
    }

    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = destination.with_extension(format!("{}.{}.tmp", std::process::id(), suffix));
    {
        let mut file = File::options()
            .create_new(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    replace_file(&temp, destination).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("atomic image replace failed");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).context("atomic image replace failed")
}

struct DecodedImage {
    bytes: Vec<u8>,
    extension: &'static str,
    sha256: String,
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn sanitizes_windows_path_components() {
        assert_eq!(sanitize("abc:中文/def"), "abc____def");
    }

    #[test]
    fn validates_png_jpeg_and_webp_structures() {
        let png = base64::engine::general_purpose::STANDARD
            .decode(PNG_BASE64)
            .unwrap();
        assert_eq!(validate_image(&png).unwrap(), "png");

        let jpeg = minimal_jpeg();
        assert_eq!(validate_image(&jpeg).unwrap(), "jpg");

        let webp = minimal_webp();
        assert_eq!(validate_image(&webp).unwrap(), "webp");
    }

    #[test]
    fn rejects_truncated_or_corrupt_images() {
        let mut png = base64::engine::general_purpose::STANDARD
            .decode(PNG_BASE64)
            .unwrap();
        png[20] ^= 1;
        assert!(validate_image(&png).is_err());
        assert!(validate_image(&[0xFF, 0xD8, 0xFF]).is_err());
        assert!(validate_image(b"RIFF\x04\0\0\0WEBP").is_err());
    }

    #[test]
    fn reuses_saved_path_only_when_hash_matches_result() {
        let root = test_directory("saved-path");
        fs::create_dir_all(&root).unwrap();
        let existing = root.join("existing.png");
        let png = base64::engine::general_purpose::STANDARD
            .decode(PNG_BASE64)
            .unwrap();
        fs::write(&existing, &png).unwrap();
        let image = SessionImage {
            turn_id: Some("turn".into()),
            id: "image".into(),
            status: "generating".into(),
            revised_prompt: None,
            result: Some(PNG_BASE64.into()),
            saved_path: Some(existing.clone()),
        };
        let saved = decode_and_save(&root.join("output"), "thread", &image).unwrap();
        assert_eq!(saved.path, existing.canonicalize().unwrap());
        assert_eq!(saved.sha256, sha256(&png));
        assert!(!root.join("output").exists());

        fs::write(&existing, minimal_jpeg()).unwrap();
        let saved = decode_and_save(&root.join("output"), "thread", &image).unwrap();
        assert_ne!(saved.path, existing.canonicalize().unwrap());
        assert_eq!(saved.sha256, sha256(&png));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stores_images_by_sha256() {
        let root = test_directory("sha256");
        let image = SessionImage {
            turn_id: Some("turn".into()),
            id: "image".into(),
            status: "generating".into(),
            revised_prompt: None,
            result: Some(PNG_BASE64.into()),
            saved_path: None,
        };
        let first = decode_and_save(&root, "thread", &image).unwrap();
        let second = decode_and_save(&root, "thread", &image).unwrap();
        assert_eq!(first.path, second.path);
        assert!(first.path.ends_with(format!("{}.png", first.sha256)));
        fs::remove_dir_all(root).unwrap();
    }

    fn minimal_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11,
            0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFF, 0xD9,
        ]
    }

    fn minimal_webp() -> Vec<u8> {
        vec![
            b'R', b'I', b'F', b'F', 18, 0, 0, 0, b'W', b'E', b'B', b'P', b'V', b'P', b'8', b'L', 5,
            0, 0, 0, 0x2f, 0, 0, 0, 0, 0,
        ]
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("codex-image-fix-{label}-{unique}"))
    }
}
