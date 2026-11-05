use eframe::egui::{self, ColorImage, TextureHandle, Vec2};
use crate::friends::Profile;
use std::io::Cursor;
use std::hash::{Hash, Hasher};

fn decode(bytes: &[u8], max: u32) -> Option<image::DynamicImage> {
    if bytes.is_empty() || bytes.len() > 20_000_000 { return None; }
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max); limits.max_image_height = Some(max);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()
}
pub fn choose_photo(owner: isize) -> Result<Option<Vec<u8>>, String> {
    use windows::Win32::UI::Controls::Dialogs::*;
    use windows::Win32::Foundation::HWND;
    use windows::core::{w, PWSTR};
    let mut path = [0u16; 32768];
    let mut dialog = OPENFILENAMEW { lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: HWND(owner as *mut _), lpstrFile: PWSTR(path.as_mut_ptr()), nMaxFile: path.len() as u32,
        lpstrTitle: w!("Выберите аватарку"),
        lpstrFilter: w!("Изображения (PNG, JPEG)\0*.png;*.jpg;*.jpeg\0\0"),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR, ..Default::default() };
    if !unsafe { GetOpenFileNameW(&mut dialog) }.as_bool() { return Ok(None); }
    let end = path.iter().position(|v| *v == 0).unwrap_or(path.len());
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&path[..end]));
    if std::fs::metadata(&path).map_err(|e| e.to_string())?.len() > 20_000_000 { return Err("Файл слишком большой (максимум 20 МБ)".into()); }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    thumbnail(&bytes).map(Some).ok_or_else(|| "Не удалось открыть фото. Выберите PNG/JPEG до 8192×8192.".into())
}
pub fn thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let photo = decode(bytes, 8192)?;
    for side in [64, 48, 32] {
        let rgb = photo.resize_to_fill(side, side, image::imageops::FilterType::Triangle).to_rgb8();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 55).encode_image(&rgb).ok()?;
        if out.len() <= 1600 { return Some(out); }
    }
    None
}
pub fn avatar(ui: &mut egui::Ui, profile: &Profile, key: impl Hash + std::fmt::Debug, size: f32) -> egui::Response {
    let id = egui::Id::unique(("profile-avatar", key));
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    profile.avatar.hash(&mut hash); profile.photo.hash(&mut hash); let stamp = hash.finish();
    let old = ui.ctx().data_mut(|d| d.get_temp::<(u64, TextureHandle)>(id));
    let texture = if let Some((_, tex)) = old.filter(|v| v.0 == stamp) { tex } else {
        let pixels = if profile.photo.len() <= 1600 { decode(&profile.photo, 64) } else { None };
        let image = if let Some(img) = pixels {
            let rgba = img.to_rgba8(); ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
        } else {
            let img = crate::stickers::get(profile.avatar as usize, 128, std::time::Duration::ZERO);
            let rgba: Vec<u8> = img.px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
            ColorImage::from_rgba_premultiplied([img.size as usize, img.size as usize], &rgba)
        };
        let tex = ui.ctx().load_texture("profile", image, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(id, (stamp, tex.clone()))); tex
    };
    ui.add(egui::Image::new((texture.id(), Vec2::splat(size))).corner_radius((size / 2.0) as u8).sense(egui::Sense::click()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn photos_are_bounded_and_invalid_images_fail() {
        let img = image::DynamicImage::new_rgb8(256, 200);
        let mut bytes=Cursor::new(Vec::new()); img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let thumb=thumbnail(bytes.get_ref()).unwrap(); assert!(thumb.len() <= 1600);
        assert!(decode(&thumb, 64).is_some());
        assert!(thumbnail(b"not an image").is_none());
        assert!(decode(bytes.get_ref(), 64).is_none());
    }
}
