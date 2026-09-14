//! HEIC/HEIF decoding via the Windows Imaging Component (WIC).
//!
//! We deliberately don't use a Rust HEIF-decoding crate here: those need `libheif`
//! compiled for Windows (via vcpkg), which is a heavy, slow, and fragile build-time
//! dependency. Windows itself can already decode HEIC through WIC as long as the
//! "HEIF Image Extensions" package is installed (Microsoft Store) - which it almost
//! always is on any machine that has viewed iPhone photos in the Photos app or File
//! Explorer, since that's what powers their thumbnails. WIC is what this program uses.

use anyhow::{Context, Result};
use image::RgbaImage;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapFrameDecode,
    IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::PCWSTR;

/// Must be called once per thread before any WIC calls below.
pub fn init_com() {
    unsafe {
        // Returns S_FALSE if COM is already initialized on this thread with compatible
        // flags - that's fine, not an error we care about for a short-lived CLI tool.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Decodes any WIC-supported image (HEIC/HEIF, JPEG, PNG, ...) into an RGBA image,
/// applying EXIF orientation if we can read it.
pub fn decode_image(path: &Path) -> Result<RgbaImage> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .context("could not create the Windows Imaging Component factory")?;

        let wide = to_wide(path);
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .with_context(|| {
                format!(
                    "Windows couldn't open '{}' as an image. If this is a HEIC/HEIC file, \
                     install \"HEIF Image Extensions\" from the Microsoft Store, then try again.",
                    path.display()
                )
            })?;

        let frame = decoder
            .GetFrame(0)
            .context("could not read the first frame of the image")?;

        let orientation = read_orientation(&frame);

        let mut width = 0u32;
        let mut height = 0u32;
        frame
            .GetSize(&mut width, &mut height)
            .context("could not read image dimensions")?;

        let converter: IWICFormatConverter = factory
            .CreateFormatConverter()
            .context("could not create a WIC format converter")?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .context("could not convert the decoded image to RGBA")?;

        let stride = width
            .checked_mul(4)
            .context("image is implausibly wide")?;
        let buffer_len = (stride as u64) * (height as u64);
        let mut buffer = vec![0u8; buffer_len as usize];
        converter
            .CopyPixels(std::ptr::null(), stride, &mut buffer)
            .context("could not copy decoded pixel data out of WIC")?;

        let img = RgbaImage::from_raw(width, height, buffer)
            .context("decoded pixel buffer didn't match the reported image size")?;

        Ok(apply_orientation(img, orientation))
    }
}

/// Best-effort EXIF orientation lookup (tag 0x0112 / 274). Returns `1` (identity) if
/// the codec doesn't expose it or anything about this fails - this only affects whether
/// the final page is rotated to "true up", it never affects whether the program succeeds.
fn read_orientation(frame: &IWICBitmapFrameDecode) -> u32 {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_UI2;
    use windows::core::w;

    // Different container formats expose EXIF metadata at slightly different query
    // paths. Orientation (tag 0x0112/274) lives directly in IFD0, NOT in the nested
    // "Exif" sub-IFD (tag 0x8769, which holds things like ExposureTime/FNumber) - so
    // paths with an `/exif/` segment never match it. `/app1/ifd/{ushort=274}` is the
    // real JPEG (APP1) path; some files carrying a `.heic` extension are actually
    // plain re-encoded JPEGs under the hood (e.g. from iOS Shortcuts pipelines), so we
    // must check it. `/ifd/{ushort=274}` covers bare-TIFF/HEIF-style roots. The
    // `/exif/` variants are kept as harmless extra fallbacks in case some encoder
    // duplicates the tag there too.
    let candidates = [
        w!("/app1/ifd/{ushort=274}"),
        w!("/ifd/{ushort=274}"),
        w!("/app1/ifd/exif/{ushort=274}"),
        w!("/ifd/exif/{ushort=274}"),
    ];

    let reader = match unsafe { frame.GetMetadataQueryReader() } {
        Ok(r) => r,
        Err(_) => return 1,
    };

    for name in candidates {
        unsafe {
            let mut prop = PROPVARIANT::default();
            if reader.GetMetadataByName(name, &mut prop).is_ok() {
                let vt = prop.Anonymous.Anonymous.vt;
                if vt == VT_UI2 {
                    return prop.Anonymous.Anonymous.Anonymous.uiVal as u32;
                }
            }
        }
    }
    1
}

/// Applies the standard EXIF orientation values (1-8) to a decoded image.
fn apply_orientation(img: RgbaImage, orientation: u32) -> RgbaImage {
    use image::imageops::{flip_horizontal, flip_vertical, rotate90, rotate180, rotate270};
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => flip_horizontal(&rotate90(&img)),
        6 => rotate90(&img),
        7 => flip_horizontal(&rotate270(&img)),
        8 => rotate270(&img),
        _ => img,
    }
}
