//! Dev-only diagnostic: decodes a HEIC exactly like `heic::decode_image` does (same WIC
//! calls, same orientation handling) and saves it straight to PNG with no other
//! processing, so we can look at it directly and see whether the content itself comes
//! out right-side-up or rotated - independent of paper detection entirely.
//!
//! Usage: cargo run --release --example decode_check -- <file.heic> [out.png]

use image::RgbaImage;
use std::env;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapFrameDecode,
    IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::PCWSTR;

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn read_orientation(frame: &IWICBitmapFrameDecode) -> u32 {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_UI2;
    use windows::core::w;

    let candidates = [
        w!("/app1/ifd/{ushort=274}"),
        w!("/ifd/{ushort=274}"),
        w!("/app1/ifd/exif/{ushort=274}"),
        w!("/ifd/exif/{ushort=274}"),
    ];

    let Ok(reader) = (unsafe { frame.GetMetadataQueryReader() }) else {
        println!("  (no metadata query reader at all)");
        return 1;
    };

    for name in candidates {
        unsafe {
            let mut prop = PROPVARIANT::default();
            if reader.GetMetadataByName(name, &mut prop).is_ok() {
                let vt = prop.Anonymous.Anonymous.vt;
                println!("  orientation query hit: vt={vt:?}");
                if vt == VT_UI2 {
                    return prop.Anonymous.Anonymous.Anonymous.uiVal as u32;
                }
            }
        }
    }
    println!("  (no orientation tag found by any query path - defaulting to 1/identity)");
    1
}

fn decode(path: &Path) -> windows::core::Result<(RgbaImage, u32)> {
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let wide = to_wide(path);
        let decoder = factory.CreateDecoderFromFilename(PCWSTR(wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;

        let orientation = read_orientation(&frame);

        let mut width = 0u32;
        let mut height = 0u32;
        frame.GetSize(&mut width, &mut height)?;
        println!("  raw WIC size: {width}x{height}");

        let converter: IWICFormatConverter = factory.CreateFormatConverter()?;
        converter.Initialize(&frame, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;

        let stride = width * 4;
        let mut buffer = vec![0u8; (stride * height) as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut buffer)?;

        let img = RgbaImage::from_raw(width, height, buffer).expect("buffer size mismatch");
        Ok((img, orientation))
    }
}

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

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let mut args = env::args().skip(1);
    let input = args.next().expect("usage: decode_check <file.heic> [out.png]");
    let input = Path::new(&input);
    let output: PathBuf = args.next().map(PathBuf::from).unwrap_or_else(|| input.with_extension("decoded.png"));

    println!("{}", input.display());
    match decode(input) {
        Ok((img, orientation)) => {
            println!("  detected EXIF orientation tag value: {orientation}");
            let final_img = apply_orientation(img, orientation);
            println!("  final size after orientation correction: {}x{}", final_img.width(), final_img.height());
            final_img.save(&output).expect("save png");
            println!("  wrote {}", output.display());
        }
        Err(e) => eprintln!("  ERROR: {e}"),
    }
}
