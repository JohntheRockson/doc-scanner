use crate::enhance::ScanMode;
use clap::Parser;
use std::path::PathBuf;

/// Turns recent photos of paper documents into a clean, perspective-corrected, shadow-free
/// scanned PDF.
///
/// By default, recursively scans `C:\Users\<you>\OneDrive\Pictures\Camera Roll 1` for
/// `.heic`/`.heif` files created or modified in the last few minutes — including dated
/// subfolders such as `2026\09` — and saves the resulting PDF to your Downloads folder.
/// Pass specific files or folders to override either behavior.
#[derive(Parser, Debug)]
#[command(name = "paper_scanner", version, about, long_about = None)]
pub struct Args {
    /// Specific .heic/.heif files and/or directories to scan. If omitted, the default
    /// Camera Roll directory is scanned recursively (root plus dated `YYYY\MM` folders).
    /// Files passed explicitly are always processed regardless of age; directories are
    /// still filtered by `--minutes` unless `--all` is given.
    pub paths: Vec<PathBuf>,

    /// Only include files created/modified within the last N minutes when scanning a
    /// directory. Ignored for files passed explicitly on the command line.
    #[arg(short = 'm', long, default_value_t = 5)]
    pub minutes: u64,

    /// Process every matching file in scanned directories, ignoring `--minutes`.
    #[arg(long)]
    pub all: bool,

    /// Where to save the PDF: an exact `.pdf` file path, or a directory to place an
    /// auto-named PDF into. Defaults to your Downloads folder.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Visual style of the output pages.
    #[arg(long, value_enum, default_value_t = ScanMode::Color)]
    pub mode: ScanMode,

    /// Write intermediate debug images (perspective-warped and final) into this
    /// directory, one pair per input photo. Handy for tuning/troubleshooting detection.
    #[arg(long)]
    pub debug: Option<PathBuf>,
}
