//! Turns a perspective-corrected photo of a sheet of paper into a clean "scanned" look:
//! removes uneven shading/shadows, then auto-stretches contrast. Optionally converts to
//! grayscale or a binarized black & white "text document" style.

use clap::ValueEnum;
use image::{ImageBuffer, Rgb, RgbImage};
use imageproc::filter::gaussian_blur_f32;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum ScanMode {
    /// Clean, shadow-corrected color (like a typical "auto color" mobile scanner app).
    Color,
    /// Same cleanup, converted to grayscale.
    Gray,
    /// Binarized black & white, like a classic flatbed text scan.
    Bw,
}

pub fn clean_scan(img: &RgbImage, mode: ScanMode) -> RgbImage {
    let flattened = remove_shading(img);
    let contrasted = auto_contrast(&flattened);

    match mode {
        ScanMode::Color => contrasted,
        ScanMode::Gray => {
            let gray = image::imageops::grayscale(&contrasted);
            image::DynamicImage::ImageLuma8(gray).to_rgb8()
        }
        ScanMode::Bw => {
            let gray = image::imageops::grayscale(&contrasted);
            let radius = (gray.width().min(gray.height()) / 20).clamp(8, 35);
            let bw = imageproc::contrast::adaptive_threshold(&gray, radius, 6);
            image::DynamicImage::ImageLuma8(bw).to_rgb8()
        }
    }
}

/// Removes uneven lighting/shadows by dividing every pixel by a heavily-blurred estimate
/// of the *local* background brightness, then rescaling so that local background maps
/// back to white. This is the standard "flatten scanned-document lighting" trick.
///
/// The blur runs on a small downscaled copy (we only care about large-scale shading, not
/// fine detail) and is then upscaled back - this keeps it fast even on full-resolution
/// photos, since blurring at a huge radius directly on a 12+ megapixel image would be slow.
fn remove_shading(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }

    let small_dim = 260u32;
    let longest = w.max(h) as f32;
    let scale = (small_dim as f32 / longest).min(1.0);
    let (sw, sh) = (
        ((w as f32) * scale).max(1.0) as u32,
        ((h as f32) * scale).max(1.0) as u32,
    );

    let small = image::imageops::resize(img, sw, sh, image::imageops::FilterType::Triangle);
    let sigma = (sw.min(sh) as f32 / 5.0).max(6.0);
    let shading_small = gaussian_blur_f32(&small, sigma);
    let shading = image::imageops::resize(&shading_small, w, h, image::imageops::FilterType::Triangle);

    let mut out: RgbImage = ImageBuffer::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let src = img.get_pixel(x, y);
            let bg = shading.get_pixel(x, y);
            let mut px = [0u8; 3];
            for c in 0..3 {
                let bg_val = (bg[c] as f32).max(8.0);
                let normalized = src[c] as f32 * (255.0 / bg_val);
                px[c] = normalized.round().clamp(0.0, 255.0) as u8;
            }
            out.put_pixel(x, y, Rgb(px));
        }
    }
    out
}

/// Stretches the histogram so the darkest/lightest ~0.5% of pixels (by luma) become pure
/// black/white, the same "auto levels" trick a scanner or photo app would apply. Uses one
/// shared low/high point across channels so color balance is preserved.
fn auto_contrast(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }

    let mut histogram = [0u32; 256];
    for p in img.pixels() {
        let luma = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as usize;
        histogram[luma.min(255)] += 1;
    }

    let total = w * h;
    let low_cut = (total / 200).max(1); // ~0.5%
    let high_cut = total.saturating_sub(low_cut);

    let mut running = 0u32;
    let mut lo = 0u8;
    for (i, count) in histogram.iter().enumerate() {
        running += count;
        if running >= low_cut {
            lo = i as u8;
            break;
        }
    }

    running = 0;
    let mut hi = 255u8;
    for (i, count) in histogram.iter().enumerate() {
        running += count;
        if running >= high_cut {
            hi = i as u8;
            break;
        }
    }

    if hi <= lo {
        return img.clone();
    }

    let lo_f = lo as f32;
    let range = (hi as f32 - lo_f).max(1.0);

    let mut out: RgbImage = ImageBuffer::new(w, h);
    for (x, y, p) in img.enumerate_pixels() {
        let mut px = [0u8; 3];
        for c in 0..3 {
            let v = (p[c] as f32 - lo_f) / range * 255.0;
            px[c] = v.round().clamp(0.0, 255.0) as u8;
        }
        out.put_pixel(x, y, Rgb(px));
    }
    out
}
