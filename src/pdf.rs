//! Assembles processed page images into a single "scanned" PDF.

use anyhow::{Context, Result};
use image::RgbImage;
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Px, RawImage, XObjectTransform};
use std::path::Path;

/// DPI used both to size each PDF page and to place the image on it. Must match on both
/// sides, otherwise the page and the image drawn on it disagree about physical size.
const DPI: f32 = 200.0;

pub fn write_pdf(pages: &[RgbImage], output: &Path) -> Result<()> {
    let mut doc = PdfDocument::new("Scanned Document");
    let mut pdf_pages = Vec::with_capacity(pages.len());

    for img in pages {
        let (w, h) = img.dimensions();
        let dyn_img = image::DynamicImage::ImageRgb8(img.clone());
        let raw = RawImage::from_dynamic_image(dyn_img)
            .map_err(|e| anyhow::anyhow!("could not prepare a page image for the PDF: {e}"))?;

        let width_mm: Mm = Px(w as usize).into_pt(DPI).into();
        let height_mm: Mm = Px(h as usize).into_pt(DPI).into();

        let image_id = doc.add_image(&raw);
        let ops = vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                dpi: Some(DPI),
                ..Default::default()
            },
        }];
        pdf_pages.push(PdfPage::new(width_mm, height_mm, ops));
    }

    let mut warnings = Vec::new();
    let bytes = doc
        .with_pages(pdf_pages)
        .save(&PdfSaveOptions::default(), &mut warnings);

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    std::fs::write(output, bytes).with_context(|| format!("writing {}", output.display()))?;
    Ok(())
}
