//! Dev-only helper: generates a synthetic PORTRAIT photo containing a PORTRAIT sheet of
//! paper tilted well past 45 degrees (phone held at a steep angle relative to the page),
//! to directly reproduce and verify the fix for the "portrait page comes out landscape"
//! orientation bug end-to-end (decode -> detect -> warp -> enhance -> pdf), not just at
//! the unit-test level.
//!
//! Usage: cargo run --release --example make_rotated_test_photo -- <output.heic> [degrees]

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
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::PCWSTR;

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

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

fn make_flat_page(w: u32, h: u32) -> RgbImage {
    let mut rng = Lcg(0xabcd_ef01);
    let mut page = ImageBuffer::from_fn(w, h, |_, _| {
        let g = 248 + (rng.range(6) - 3);
        Rgb([g as u8, g as u8, (g - 3).max(0) as u8])
    });
    let margin = 60;
    for y in 40..65 {
        for x in margin..(w - margin) {
            page.put_pixel(x, y, Rgb([15, 15, 20]));
        }
    }
    let mut y = 130;
    while y + 8 < h - margin {
        for yy in y..(y + 6) {
            for x in margin..(w - margin) {
                page.put_pixel(x, yy, Rgb([25, 25, 30]));
            }
        }
        y += 42;
    }
    page
}

fn main() {
    let mut args = env::args().skip(1);
    let out: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test_assets/rotated_test.heic"));
    let degrees: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(75.0);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create output dir");
    }

    // Portrait photo (matches "I took it vertical").
    let (pw, ph) = (1200u32, 1600u32);
    let mut rng = Lcg(0x1234_5678);
    let mut photo: RgbImage = ImageBuffer::from_fn(pw, ph, |_, _| {
        let grain = rng.range(20) - 10;
        Rgb([
            (55 + grain).clamp(0, 255) as u8,
            (40 + grain).clamp(0, 255) as u8,
            (32 + grain).clamp(0, 255) as u8,
        ])
    });

    // Portrait page (narrower than tall), tilted by `degrees` - past the 45-degree
    // point where the old heuristic would mislabel corners and swap width/height.
    let (fw, fh) = (700u32, 950u32);
    let theta = degrees.to_radians();
    let (c, s) = (theta.cos(), theta.sin());
    let rotate = |x: f32, y: f32| (x * c - y * s, x * s + y * c);
    let raw = [
        rotate(0.0, 0.0),
        rotate(fw as f32, 0.0),
        rotate(fw as f32, fh as f32),
        rotate(0.0, fh as f32),
    ];

    let min_x = raw.iter().map(|p| p.0).fold(f32::MAX, f32::min);
    let max_x = raw.iter().map(|p| p.0).fold(f32::MIN, f32::max);
    let min_y = raw.iter().map(|p| p.1).fold(f32::MAX, f32::min);
    let max_y = raw.iter().map(|p| p.1).fold(f32::MIN, f32::max);
    let cx = (pw as f32 - (max_x - min_x)) / 2.0 - min_x;
    let cy = (ph as f32 - (max_y - min_y)) / 2.0 - min_y;
    let quad_f32: [(f32, f32); 4] = raw.map(|(x, y)| (x + cx, y + cy));

    println!("Rotation: {degrees} degrees. Quad corners in photo space: {quad_f32:?}");
    println!(
        "(Flat page is {fw}x{fh} portrait; quad's own long axis is now tilted {degrees} degrees from vertical.)"
    );

    let flat_page = make_flat_page(fw, fh);
    let flat_corners: [(f32, f32); 4] = [
        (0.0, 0.0),
        (fw as f32 - 1.0, 0.0),
        (fw as f32 - 1.0, fh as f32 - 1.0),
        (0.0, fh as f32 - 1.0),
    ];
    let projection = Projection::from_control_points(flat_corners, quad_f32).expect("projection");

    let mut warped_page: RgbImage = ImageBuffer::new(pw, ph);
    warp_into(
        &flat_page,
        projection,
        Interpolation::Bilinear,
        Border::Constant(Rgb([0, 0, 0])),
        &mut warped_page,
    );

    let flat_mask: GrayImage = ImageBuffer::from_pixel(fw, fh, Luma([255]));
    let mut mask_photo: GrayImage = ImageBuffer::new(pw, ph);
    warp_into(
        &flat_mask,
        projection,
        Interpolation::Nearest,
        Border::Constant(Luma([0])),
        &mut mask_photo,
    );

    for y in 0..ph {
        for x in 0..pw {
            if mask_photo.get_pixel(x, y)[0] > 127 {
                photo.put_pixel(x, y, *warped_page.get_pixel(x, y));
            }
        }
    }

    let preview = out.with_extension("preview.png");
    let _ = photo.save(&preview);
    println!("Wrote PNG preview to {}", preview.display());

    encode_heic(&photo, &out).expect("encode failed");
    println!("Wrote synthetic test photo to {}", out.display());
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
        converter.Initialize(
            &source_bitmap,
            &format,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        frame.WriteSource(&converter, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}
