//! Dev-only helper: generates a photo with no distinguishable paper region at all
//! (uniform noisy color, no bright/dark blob), to exercise `paper_scanner`'s
//! full-image fallback path when detection can't confidently find a page.
//!
//! Usage: cargo run --release --example make_blank_photo -- <output.heic>

use image::{ImageBuffer, Rgb, RgbImage};
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

fn make_blank_photo() -> RgbImage {
    let (w, h) = (1200u32, 1600u32);
    let mut rng = Lcg(0x9999_1111);
    ImageBuffer::from_fn(w, h, |_, _| {
        let grain = rng.range(30) - 15;
        let v = (128 + grain).clamp(0, 255) as u8;
        Rgb([v, v, v])
    })
}

fn encode_heic(img: &RgbImage, path: &Path) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
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
        let source_bitmap = factory.CreateBitmapFromMemory(img.width(), img.height(), &GUID_WICPixelFormat24bppRGB, stride, img.as_raw())?;
        let converter: IWICFormatConverter = factory.CreateFormatConverter()?;
        converter.Initialize(&source_bitmap, &format, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        frame.WriteSource(&converter, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

fn main() {
    let out: PathBuf = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("test_assets/blank.heic"));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create output dir");
    }
    let img = make_blank_photo();
    encode_heic(&img, &out).expect("encode failed");
    println!("Wrote {}", out.display());
}
