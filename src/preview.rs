//! Image files: which ones Kuana shows as pictures instead of text, their
//! size and dimensions, and temporary copies of image versions from Git
//! (GPUI decodes images from paths, with every format the `image` crate has).

use crate::fs::PathExt as _;
use std::path::{Path, PathBuf};

/// Raster formats GPUI can decode. SVG is left out on purpose: it's text
/// that people edit and diff line by line.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "qoi"];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    /// "PNG", "JPEG".
    pub format: String,
    pub dimensions: Option<(u32, u32)>,
    pub bytes: u64,
}

impl ImageInfo {
    pub fn read(path: &Path) -> Self {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_uppercase();
        ImageInfo {
            format: if ext == "JPG" { "JPEG".into() } else { ext },
            dimensions: image::image_dimensions(path).ok(),
            bytes: crate::fs::metadata(path).map_or(0, |m| m.len()),
        }
    }

    /// "PNG · 512 × 512 · 33.1 KB"
    pub fn label(&self) -> String {
        let mut parts = vec![self.format.clone()];
        if let Some((w, h)) = self.dimensions {
            parts.push(format!("{w} × {h}"));
        }
        parts.push(file_size(self.bytes));
        parts.join(" · ")
    }
}

pub fn file_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.),
    }
}

/// Writes `bytes` (an image version from Git) to a temporary file with the
/// right extension, named by content so each version is written once.
pub fn temp_copy(bytes: &[u8], extension: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let dir = std::env::temp_dir().join("kuana-images");
    crate::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{:016x}.{extension}", hasher.finish()));
    if !path.exists_on_host() {
        crate::fs::write(&path, bytes).ok()?;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_images_but_not_svg() {
        assert!(is_image(Path::new("a/logo.PNG")));
        assert!(is_image(Path::new("photo.jpeg")));
        assert!(!is_image(Path::new("icon.svg")));
        assert!(!is_image(Path::new("main.rs")));
    }

    #[test]
    fn labels_and_reads_a_real_image() {
        assert_eq!(file_size(512), "512 B");
        assert_eq!(file_size(33_894), "33.1 KB");
        // A real 2×1 PNG.
        let mut png = Vec::new();
        image::RgbaImage::new(2, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = temp_copy(&png, "png").unwrap();
        let info = ImageInfo::read(&path);
        assert_eq!((info.format.as_str(), info.dimensions), ("PNG", Some((2, 1))));
        assert!(info.label().starts_with("PNG · 2 × 1 · "));
    }
}
