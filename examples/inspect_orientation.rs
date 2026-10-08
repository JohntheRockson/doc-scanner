//! Dev-only diagnostic: for each given HEIC file, prints the RAW pixel dimensions WIC
//! decodes (before any rotation we apply), whatever EXIF orientation tag we can read,
//! and whether that raw buffer is already portrait or landscape - to figure out whether
//! WIC's HEIF codec already auto-applies rotation (in which case our own EXIF-based
//! correction would double-rotate it) or not.
//!
//! Usage: cargo run --release --example inspect_orientation -- <file1.heic> [file2 ...]

use std::env;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, IWICImagingFactory, WICDecodeMetadataCacheOnDemand,
};
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

fn try_read_orientation(
    frame: &windows::Win32::Graphics::Imaging::IWICBitmapFrameDecode,
) -> Vec<(String, String)> {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::core::w;

    let candidates: &[(&str, windows::core::PCWSTR)] = &[
        (
            "/app1/ifd/exif/{ushort=274}",
            w!("/app1/ifd/exif/{ushort=274}"),
        ),
        ("/ifd/exif/{ushort=274}", w!("/ifd/exif/{ushort=274}")),
        ("/ifd/{ushort=274}", w!("/ifd/{ushort=274}")),
        ("/xmp/tiff:Orientation", w!("/xmp/tiff:Orientation")),
        ("/ifd/exif/{ushort=274}", w!("/ifd0/{ushort=274}")),
    ];

    let mut results = Vec::new();
    let reader = match unsafe { frame.GetMetadataQueryReader() } {
        Ok(r) => r,
        Err(e) => {
            results.push(("GetMetadataQueryReader".to_string(), format!("FAILED: {e}")));
            return results;
        }
    };

    for (label, name) in candidates {
        unsafe {
            let mut prop = PROPVARIANT::default();
            match reader.GetMetadataByName(*name, &mut prop) {
                Ok(()) => {
                    let vt = prop.Anonymous.Anonymous.vt;
                    results.push((
                        label.to_string(),
                        format!(
                            "OK vt={vt:?} uiVal={}",
                            prop.Anonymous.Anonymous.Anonymous.uiVal
                        ),
                    ));
                }
                Err(e) => {
                    results.push((label.to_string(), format!("not found ({e})")));
                }
            }
        }
    }
    results
}

fn inspect(path: &Path) -> windows::core::Result<()> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let wide = to_wide(path);
        let decoder = factory.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;

        let mut width = 0u32;
        let mut height = 0u32;
        frame.GetSize(&mut width, &mut height)?;

        println!("{}", path.display());
        println!(
            "  RAW decoded size from WIC: {width} x {height}  ({})",
            if width > height {
                "landscape"
            } else if height > width {
                "portrait"
            } else {
                "square"
            }
        );

        for (label, result) in try_read_orientation(&frame) {
            println!("  {label}: {result}");
        }
        println!();
    }
    Ok(())
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    for arg in env::args().skip(1) {
        let path = Path::new(&arg);
        if let Err(e) = inspect(path) {
            eprintln!("{}: ERROR {e}", path.display());
        }
    }
}
