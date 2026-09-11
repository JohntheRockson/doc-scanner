//! Dev-only helper (not part of the shipped tool): generates a synthetic "photo of a
//! sheet of paper on a desk" - complete with keystone perspective, a soft shadow, and
//! some fake printed lines that are properly embedded *in paper space* (so they rotate
//! and skew along with the page, like real ink would) - then encodes it as a real
//! `.heic` file via WIC so we can exercise the actual decode -> detect -> warp ->
//! enhance -> pdf pipeline end to end, since no real HEIC photos exist on this machine.
//!
//! The "flat page -> warp onto quad" step here uses the exact same
//! `imageproc::geometric_transformations` primitives as the real pipeline, just run in
//! the forward direction, which makes this a genuine round-trip test.
//!
//! Usage: cargo run --release --example make_test_photo -- <output.heic>

use image::{GrayImage, ImageBuffer, Luma, Rgb, RgbImage};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use std::env;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatHeif, GUID_WICPixelFormat24bppRGB,
    IWICBitmapFrameEncode, IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::PCWSTR;

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// A tiny deterministic PRNG so the test image is reproducible without pulling in `rand`.
struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }
    fn range(&mut self, n: i32) -> i32 {
        (self.next() % n as u32) as i32
    }
}

/// Builds the "ground truth" flat page: what the paper looks like shot face-on, with
/// perfectly horizontal printed lines. This is what we expect `paper_scanner`'s output
/// to approximately recover after perspective correction.
fn make_flat_page(w: u32, h: u32) -> RgbImage {
    let mut rng = Lcg(0xabcd_ef01);
    let mut page = ImageBuffer::from_fn(w, h, |_, _| {
        let g = 248 + (rng.range(6) - 3);
        Rgb([g as u8, g as u8, (g - 3).max(0) as u8])
    });

    let margin = 70;
    // Title bar.
    for y in 50..80 {
        for x in margin..(w - margin) {
            page.put_pixel(x, y, Rgb([15, 15, 20]));
        }
    }
    // Body "text" lines.
    let mut y = 160;
    while y + 8 < h - margin {
        for yy in y..(y + 6) {
            for x in margin..(w - margin) {
                page.put_pixel(x, yy, Rgb([25, 25, 30]));
            }
        }
        y += 50;
    }
    page
}

fn make_synthetic_photo() -> RgbImage {
    let (w, h): (u32, u32) = match env::var("PHOTO_SIZE").ok().as_deref() {
        Some("large") => (3024, 4032), // typical 12MP phone photo
        _ => (1200, 1600),
    };
    let mut rng = Lcg(0x1234_5678);

    // Desk background: dark, slightly noisy wood-like tone, filling the whole frame.
    let mut photo: RgbImage = ImageBuffer::from_fn(w, h, |_, _| {
        let grain = rng.range(20) - 10;
        Rgb([
            (55 + grain).clamp(0, 255) as u8,
            (40 + grain).clamp(0, 255) as u8,
            (32 + grain).clamp(0, 255) as u8,
        ])
    });

    let (pw, ph) = (
        (w as f32 * 0.708) as u32,
        (h as f32 * 0.6875) as u32,
    );
    let flat_page = make_flat_page(pw, ph);

    // Keystone quad: a clear perspective distortion, comfortably inside the frame.
    // Expressed as fractions of the photo size so this scales to any `PHOTO_SIZE`.
    let (wf, hf) = (w as f32, h as f32);
    let quad_f32: [(f32, f32); 4] = [
        (0.2167 * wf, 0.1125 * hf), // top-left
        (0.8417 * wf, 0.1625 * hf), // top-right (far edge -> converging)
        (0.8000 * wf, 0.8938 * hf), // bottom-right
        (0.1583 * wf, 0.8500 * hf), // bottom-left
    ];
    let flat_corners: [(f32, f32); 4] = [
        (0.0, 0.0),
        (pw as f32 - 1.0, 0.0),
        (pw as f32 - 1.0, ph as f32 - 1.0),
        (0.0, ph as f32 - 1.0),
    ];

    // Maps flat-page space -> photo space; `warp_into` uses its inverse to pull photo-space
    // pixels back from the flat page, i.e. exactly "paste the flat page onto that quad".
    let projection = Projection::from_control_points(flat_corners, quad_f32).expect("projection");

    let mut warped_page: RgbImage = ImageBuffer::new(w, h);
    warp_into(
        &flat_page,
        projection,
        Interpolation::Bilinear,
        Border::Constant(Rgb([0, 0, 0])),
        &mut warped_page,
    );

    // A matching coverage mask, so we only composite the warped page where it's actually
    // "on the page" and leave the desk texture alone everywhere else.
    let flat_mask: GrayImage = ImageBuffer::from_pixel(pw, ph, Luma([255]));
    let mut mask_photo: GrayImage = ImageBuffer::new(w, h);
    warp_into(
        &flat_mask,
        projection,
        Interpolation::Nearest,
        Border::Constant(Luma([0])),
        &mut mask_photo,
    );

    for y in 0..h {
        for x in 0..w {
            if mask_photo.get_pixel(x, y)[0] > 127 {
                let mut px = *warped_page.get_pixel(x, y);
                // Soft shadow sweeping across the left third of the *photo* (realistic:
                // real-world shadows fall in photo/world space, not paper-local space).
                let shadow = 0.55 + 0.45 * (x as f32 / w as f32).min(1.0);
                for c in 0..3 {
                    px.0[c] = (px.0[c] as f32 * shadow).clamp(0.0, 255.0) as u8;
                }
                photo.put_pixel(x, y, px);
            }
        }
    }

    photo
}

fn encode_heic(img: &RgbImage, path: &Path) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

        let stream = factory.CreateStream()?;
        let wide = to_wide(path);
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), GENERIC_WRITE.0)?;

        let encoder = factory.CreateEncoder(&GUID_ContainerFormatHeif, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

        let mut frame: Option<IWICBitmapFrameEncode> = None;
        let mut props: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut props)?;
        let frame = frame.expect("encoder returned no frame");

        frame.Initialize(props.as_ref())?;
        frame.SetSize(img.width(), img.height())?;

        // `SetPixelFormat` is an in/out negotiation: the HEIF encoder overwrites `format`
        // with whatever it actually wants (commonly some YCbCr-ish format), so instead of
        // assuming our raw RGB24 buffer already matches, we wrap it as a WIC bitmap and
        // let a format converter + WriteSource handle whatever conversion is needed.
        let mut format = GUID_WICPixelFormat24bppRGB;
        frame.SetPixelFormat(&mut format)?;

        let stride = img.width() * 3;
        let source_bitmap = factory.CreateBitmapFromMemory(
            img.width(),
            img.height(),
            &GUID_WICPixelFormat24bppRGB,
            stride,
            img.as_raw(),
        )?;
        let converter: IWICFormatConverter = factory.CreateFormatConverter()?;
        converter.Initialize(&source_bitmap, &format, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;

        frame.WriteSource(&converter, std::ptr::null())?;

        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

fn main() {
    let out: PathBuf = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test_assets/synthetic_test.heic"));

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create output dir");
    }

    let img = make_synthetic_photo();
    let png_preview = out.with_extension("preview.png");
    let _ = img.save(&png_preview);
    println!("Wrote PNG preview to {}", png_preview.display());

    match encode_heic(&img, &out) {
        Ok(()) => println!("Wrote synthetic test photo to {}", out.display()),
        Err(e) => {
            eprintln!("Failed to encode HEIC: {e}");
            std::process::exit(1);
        }
    }
}
