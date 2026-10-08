use crate::enhance::ScanMode;
use clap::Parser;
use std::path::PathBuf;

/// Turns photos of paper documents into a clean, perspective-corrected, shadow-free
/// scanned PDF.
///
/// By default, pulls every photo waiting in your Cloudflare R2 bucket (configured via
/// `.env`), scans them into one PDF in your Downloads folder, and deletes the originals
/// from the bucket once they're safely in the PDF. Pass `--local` to scan local files
/// instead - either the default OneDrive Camera Roll (including its dated `YYYY\MM`
/// subfolders), or specific files/folders you name.
///
/// Pass `--edit` or `-edit` to place screenshots, images, and text on the scan
/// before the PDF is saved.
#[derive(Parser, Debug)]
#[command(name = "paper_scanner", version, about, long_about = None)]
pub struct Args {
    /// Which items to process. If any of these already exist as local files or
    /// directories, local mode is used automatically (no need for `--local`). Otherwise,
    /// in the default Cloudflare mode, these are treated as R2 object keys/filenames -
    /// see `--list`; matching is done by exact key or by filename alone so you don't
    /// have to type any prefix. If omitted entirely: Cloudflare mode processes every
    /// object in the bucket; local mode (via `--local`) scans the default Camera Roll
    /// directory.
    pub items: Vec<String>,

    /// Force local-filesystem mode instead of Cloudflare R2. Only needed when passing
    /// no items (to scan the default Camera Roll directory) - naming an existing local
    /// file or directory already switches to local mode automatically.
    #[arg(long)]
    pub local: bool,

    /// List the available files (R2 objects by default, or local files with `--local`)
    /// and exit. Nothing is downloaded, processed, or deleted.
    #[arg(long)]
    pub list: bool,

    /// Only consider R2 objects whose key starts with this prefix (i.e. a "folder"
    /// inside the bucket). Ignored in `--local` mode.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Only include files created/modified within the last N minutes. `--local` mode
    /// defaults to 5 minutes when scanning the default Camera Roll directory (ignored
    /// for files/directories named explicitly on the command line). Cloudflare mode has
    /// no time filter by default - every matching object is included - unless this is
    /// set.
    #[arg(short = 'm', long)]
    pub minutes: Option<u64>,

    /// Process every matching file, ignoring `--minutes`.
    #[arg(long)]
    pub all: bool,

    /// Leave the originals in the R2 bucket after a successful scan instead of deleting
    /// them. Ignored in `--local` mode (local files are never deleted).
    #[arg(long)]
    pub keep_remote: bool,

    /// Directory to save the PDF into. Defaults to your Downloads folder. If this ends
    /// in `.pdf` it's used as the exact output file path instead (and `--name` is
    /// ignored).
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Output PDF filename, with or without the `.pdf` extension. Defaults to
    /// `Scan_<timestamp>.pdf`.
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Visual style of the output pages.
    #[arg(long, value_enum, default_value_t = ScanMode::Color)]
    pub mode: ScanMode,

    /// Write intermediate debug images (perspective-warped and final) into this
    /// directory, one pair per input photo. Handy for tuning/troubleshooting detection.
    #[arg(long)]
    pub debug: Option<PathBuf>,

    /// After scanning, open an editor to paste screenshots (Ctrl+V), place
    /// PNG/JPEG images, and add text boxes before the PDF is written.
    /// `-edit` is accepted as well as `--edit`.
    #[arg(long)]
    pub edit: bool,
}

/// Rewrites the single-dash form `-edit` into clap's `--edit`.
///
/// The rest of this CLI uses clap's usual `--long` / `-s` forms. `-edit` is
/// accepted too because that's the flag name used when invoking the tool.
pub fn normalize_args<I, T>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    args.into_iter()
        .map(|arg| {
            let arg = arg.into();
            if arg == "-edit" { "--edit".into() } else { arg }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Args, normalize_args};
    use clap::Parser;

    #[test]
    fn edit_flag_accepts_single_and_double_dash() {
        let from_long = Args::parse_from(["paper_scanner", "--edit", "page.heic"]);
        assert!(from_long.edit);
        assert_eq!(from_long.items, ["page.heic"]);

        let from_short = Args::parse_from(normalize_args([
            "paper_scanner",
            "-edit",
            "--local",
            "page.heic",
        ]));
        assert!(from_short.edit);
        assert!(from_short.local);
        assert_eq!(from_short.items, ["page.heic"]);
    }
}
