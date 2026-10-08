mod cli;
mod detect;
mod editor;
mod enhance;
mod geometry;
mod heic;
mod pdf;
mod r2;

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, TimeZone};
use clap::Parser;
use cli::Args;
use enhance::ScanMode;
use image::RgbImage;
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default folder new phone photos land in via OneDrive Camera Upload (used only in
/// `--local` mode).
const DEFAULT_INPUT_DIR: &str = r"C:\Users\miskh\OneDrive\Pictures\Camera Roll 1";

fn main() -> Result<()> {
    // Non-fatal if missing: local mode needs no Cloudflare config at all.
    let _ = dotenvy::dotenv();

    let args = Args::parse_from(cli::normalize_args(std::env::args_os()));
    heic::init_com();

    if is_local_mode(&args) {
        run_local(&args)
    } else {
        run_remote(&args)
    }
}

/// Local mode is used when `--local` is passed, or when any named item already exists
/// on disk - so existing habits (pointing the tool straight at a file) keep working
/// without having to remember a new flag.
fn is_local_mode(args: &Args) -> bool {
    args.local || args.items.iter().any(|s| Path::new(s).exists())
}

/// Scans are unchanged without `--edit`. With the flag, the page images open in
/// the editor first and whatever comes back (screenshots, images, text) is what
/// gets written into the PDF.
fn open_editor_if_requested(
    edit: bool,
    pages: Vec<RgbImage>,
    output: &Path,
) -> Result<Vec<RgbImage>> {
    if !edit {
        return Ok(pages);
    }
    println!(
        "Opening the editor window.\n\
         Ctrl+V pastes a screenshot onto the page. You can also add a PNG/JPEG or a text box.\n\
         Save PDF, or close the window, to write:\n  {}",
        output.display()
    );
    editor::run(pages, output)
}

// ================================ Local filesystem mode =================================

fn run_local(args: &Args) -> Result<()> {
    let now = SystemTime::now();
    let inputs = gather_local_inputs(args, now)?;

    if args.list {
        if inputs.is_empty() {
            println!("No local files found.");
        } else {
            println!("{} local file(s) found:", inputs.len());
            for p in &inputs {
                println!("  - {}", p.display());
            }
        }
        return Ok(());
    }

    if inputs.is_empty() {
        if args.items.is_empty() {
            let minutes = args.minutes.unwrap_or(5);
            let month_dir = current_month_dir(now);
            println!(
                "No .heic/.heif files found in the last {minutes} minute(s) under:\n  {}\n\
                 including dated folders such as:\n  {}\n\
                 (Pass --all to ignore the time window, or --minutes to widen it.)",
                DEFAULT_INPUT_DIR,
                month_dir.display()
            );
        } else {
            println!("No matching .heic/.heif files found in the given paths.");
        }
        return Ok(());
    }

    println!("Found {} photo(s) to scan:", inputs.len());
    for p in &inputs {
        println!("  - {}", p.display());
    }
    println!();

    let mut pages = Vec::new();
    for path in &inputs {
        match process_one(path, args.mode, &args.debug) {
            Ok(img) => pages.push(img),
            Err(e) => eprintln!("Skipping '{}': {e:#}", path.display()),
        }
    }

    if pages.is_empty() {
        anyhow::bail!("None of the input photos could be processed.");
    }

    let output_path = resolve_output(&args.output, &args.name)?;
    let pages = open_editor_if_requested(args.edit, pages, &output_path)?;
    pdf::write_pdf(&pages, &output_path)?;

    println!(
        "\nSaved a {}-page scanned PDF to:\n  {}",
        pages.len(),
        output_path.display()
    );
    Ok(())
}

/// Resolves the final list of local input files. Explicit files are always processed.
/// Directories — including the default Camera Roll folder — are walked recursively so
/// dated `YYYY\MM` subfolders are included, then filtered by the time window unless
/// `--all` is set.
fn gather_local_inputs(args: &Args, now: SystemTime) -> Result<Vec<PathBuf>> {
    let minutes = args.minutes.unwrap_or(5);
    let window = Duration::from_secs(minutes.saturating_mul(60));
    let mut files = Vec::new();

    if args.items.is_empty() {
        let dir = PathBuf::from(DEFAULT_INPUT_DIR);
        if dir.is_dir() {
            collect_from_dir_recursive(&dir, window, args.all, now, &mut files)?;
        }
    } else {
        for item in &args.items {
            let p = PathBuf::from(item);
            if p.is_dir() {
                collect_from_dir_recursive(&p, window, args.all, now, &mut files)?;
            } else if p.is_file() {
                files.push(p);
            } else {
                eprintln!("Warning: '{item}' doesn't exist, skipping.");
            }
        }
    }

    // De-duplicate by filename, not just full path: OneDrive can transiently show the
    // same photo in both the flat root and a dated subfolder while it's mid-archive, and
    // we'd otherwise scan it twice and duplicate it in the output PDF.
    let mut seen_names = std::collections::HashSet::new();
    files.retain(|p| {
        p.file_name()
            .map(|name| seen_names.insert(name.to_os_string()))
            .unwrap_or(true)
    });

    // Chronological order, so PDF pages come out in the order the photos were taken.
    // Prefer the timestamp encoded in typical camera filenames (e.g.
    // `20260909_045421598_iOS.heic`) over filesystem mtime: OneDrive's sync/archiving
    // can rewrite mtimes out of true capture order, but the filename's own timestamp
    // doesn't move.
    files.sort_by_key(|p| {
        let filename_ts = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_filename_timestamp);
        let mtime = std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        (filename_ts.unwrap_or(0), mtime)
    });
    Ok(files)
}

/// Walks `dir` and every subdirectory. OneDrive Camera Roll photos live both at the root
/// *and* in dated `YYYY\MM` folders; we have to recurse or the month folders are skipped.
fn collect_from_dir_recursive(
    dir: &Path,
    window: Duration,
    all: bool,
    now: SystemTime,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_from_dir_recursive(&path, window, all, now, out)?;
            continue;
        }
        if !is_image_file(&path) {
            continue;
        }
        if all {
            out.push(path);
            continue;
        }
        let meta = entry.metadata()?;
        if is_recent_photo(&path, &meta, window, now) {
            out.push(path);
        }
    }
    Ok(())
}

fn current_month_dir(now: SystemTime) -> PathBuf {
    let dt: chrono::DateTime<chrono::Local> = now.into();
    PathBuf::from(DEFAULT_INPUT_DIR)
        .join(format!("{:04}", dt.year()))
        .join(format!("{:02}", dt.month()))
}

// =================================== Cloudflare R2 mode ===================================

fn run_remote(args: &Args) -> Result<()> {
    let cfg = r2::R2Config::from_env()?;
    let client = r2::build_client(&cfg);
    let rt = tokio::runtime::Runtime::new().context("starting the async runtime for R2 access")?;

    let mut objects = rt
        .block_on(r2::list_objects(
            &client,
            &cfg.bucket,
            args.prefix.as_deref(),
        ))
        .with_context(|| format!("connecting to R2 bucket '{}'", cfg.bucket))?;
    objects.sort_by(|a, b| a.key.cmp(&b.key));

    if args.list {
        if objects.is_empty() {
            println!("R2 bucket '{}' is empty.", cfg.bucket);
        } else {
            println!("{} file(s) in R2 bucket '{}':", objects.len(), cfg.bucket);
            for o in &objects {
                println!(
                    "  {:<42} {:>9}   {}",
                    o.key,
                    format_size(o.size),
                    format_time(o.last_modified)
                );
            }
        }
        return Ok(());
    }

    let image_objects: Vec<r2::RemoteObject> = objects
        .into_iter()
        .filter(|o| is_image_file(Path::new(&o.key)))
        .collect();

    let (mut selected, not_found) = if args.items.is_empty() {
        (image_objects, Vec::new())
    } else {
        select_remote_objects(image_objects, &args.items)
    };

    for missing in &not_found {
        eprintln!("Warning: '{missing}' wasn't found in the R2 bucket, skipping.");
    }

    // Explicitly-named items are always processed regardless of age, same as local mode.
    // With no selection, Cloudflare mode defaults to "everything in the bucket" - only
    // apply a time filter if the user actually asked for one.
    if args.items.is_empty()
        && !args.all
        && let Some(minutes) = args.minutes
    {
        let now = SystemTime::now();
        let window = Duration::from_secs(minutes.saturating_mul(60));
        selected.retain(|o| object_is_recent(o, window, now));
    }

    if selected.is_empty() {
        println!(
            "No matching files found in R2 bucket '{}'.\n\
             (Pass --list to see what's there, or --local to scan local files instead.)",
            cfg.bucket
        );
        return Ok(());
    }

    println!(
        "Found {} photo(s) in R2 bucket '{}':",
        selected.len(),
        cfg.bucket
    );
    for o in &selected {
        println!("  - {}", o.key);
    }
    println!();

    let temp_dir = std::env::temp_dir().join("paper_scanner_r2");
    std::fs::create_dir_all(&temp_dir).context("creating a temp directory for downloads")?;

    let mut pages = Vec::new();
    let mut processed_keys = Vec::new();

    for (idx, obj) in selected.iter().enumerate() {
        println!("Downloading {}...", obj.key);
        let bytes = match rt.block_on(r2::download_object(&client, &cfg.bucket, &obj.key)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Skipping '{}': {e:#}", obj.key);
                continue;
            }
        };

        let temp_path = temp_dir.join(format!("{idx}_{}", temp_filename_for_key(&obj.key)));
        if let Err(e) = std::fs::write(&temp_path, &bytes) {
            eprintln!("Skipping '{}': couldn't write a temp file ({e:#})", obj.key);
            continue;
        }

        let result = process_one(&temp_path, args.mode, &args.debug);
        let _ = std::fs::remove_file(&temp_path);

        match result {
            Ok(img) => {
                pages.push(img);
                processed_keys.push(obj.key.clone());
            }
            Err(e) => eprintln!("Skipping '{}': {e:#}", obj.key),
        }
    }

    if pages.is_empty() {
        anyhow::bail!("None of the R2 photos could be processed.");
    }

    let output_path = resolve_output(&args.output, &args.name)?;
    let pages = open_editor_if_requested(args.edit, pages, &output_path)?;
    pdf::write_pdf(&pages, &output_path)?;

    println!(
        "\nSaved a {}-page scanned PDF to:\n  {}",
        pages.len(),
        output_path.display()
    );

    if args.keep_remote {
        println!(
            "Left {} file(s) in the R2 bucket (--keep-remote was set).",
            processed_keys.len()
        );
    } else {
        println!("Deleting {} file(s) from R2...", processed_keys.len());
        let mut failures = 0u32;
        for key in &processed_keys {
            if let Err(e) = rt.block_on(r2::delete_object(&client, &cfg.bucket, key)) {
                eprintln!("  warning: couldn't delete '{key}' from R2: {e:#}");
                failures += 1;
            }
        }
        if failures == 0 {
            println!("Done - the R2 bucket is clear of the scanned photos.");
        } else {
            println!(
                "Done, but {failures} file(s) could not be deleted from R2 (see warnings above)."
            );
        }
    }

    Ok(())
}

/// Matches user-provided item names against bucket keys, by exact key or by filename
/// alone (so you don't have to type a folder-style prefix). Returns the matched objects
/// plus any requested names that weren't found in the bucket.
fn select_remote_objects(
    all: Vec<r2::RemoteObject>,
    items: &[String],
) -> (Vec<r2::RemoteObject>, Vec<String>) {
    let mut selected = Vec::new();
    let mut not_found = Vec::new();

    for item in items {
        let found = all.iter().find(|o| {
            o.key.eq_ignore_ascii_case(item)
                || Path::new(&o.key)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.eq_ignore_ascii_case(item))
                    .unwrap_or(false)
        });
        match found {
            Some(o) => selected.push(o.clone()),
            None => not_found.push(item.clone()),
        }
    }
    (selected, not_found)
}

fn object_is_recent(obj: &r2::RemoteObject, window: Duration, now: SystemTime) -> bool {
    match obj.last_modified {
        Some(t) => now
            .duration_since(t)
            .map(|age| age <= window)
            .unwrap_or(true),
        None => true,
    }
}

fn temp_filename_for_key(key: &str) -> String {
    Path::new(key)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "download.heic".to_string())
}

fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1_000_000.0 {
        format!("{:.1} MB", b / 1_000_000.0)
    } else if b >= 1_000.0 {
        format!("{:.1} KB", b / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "-".to_string(),
    }
}

// ============================ Shared: per-photo processing pipeline ============================

/// Decodes, detects the paper, perspective-corrects, and cleans up shading for one photo.
/// Used identically whether the file came from disk or was just downloaded from R2.
fn process_one(path: &Path, mode: ScanMode, debug_dir: &Option<PathBuf>) -> Result<RgbImage> {
    let file_label = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    println!("Processing {file_label}...");

    let rgba = heic::decode_image(path).with_context(|| format!("decoding {}", path.display()))?;
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();

    let detection = detect::detect_paper(&rgb);
    if detection.used_fallback {
        eprintln!("  note: couldn't confidently find a paper edge, using the full photo instead.");
    }

    let (out_w, out_h) = geometry::quad_output_size(detection.corners);

    let src_pts = detection.corners.map(|p| (p.x as f32, p.y as f32));
    let dst_pts = [
        (0.0, 0.0),
        (out_w as f32 - 1.0, 0.0),
        (out_w as f32 - 1.0, out_h as f32 - 1.0),
        (0.0, out_h as f32 - 1.0),
    ];

    // `Projection` maps INPUT-image locations to OUTPUT-image locations; `warp_into`
    // uses its inverse internally to sample the input for every output pixel, so this
    // gives us exactly the "face-on rectangle" perspective correction we want.
    let projection = Projection::from_control_points(src_pts, dst_pts).context(
        "the detected paper corners were degenerate (couldn't compute a perspective transform)",
    )?;

    let mut warped: RgbImage = image::ImageBuffer::new(out_w, out_h);
    // Sampling outside the source photo (from the outward "fill edges" nudge) is filled
    // with white rather than black, so it blends into the paper instead of leaving a border.
    warp_into(
        &rgb,
        projection,
        Interpolation::Bilinear,
        Border::Constant(image::Rgb([255, 255, 255])),
        &mut warped,
    );

    save_debug(debug_dir, &file_label, "1_warped", &warped);

    // Only peel off leftover desk/background along the edges. A fixed percentage trim
    // was eating real page content (problem numbers, top lines) on close-up photos.
    let mut trimmed = trim_dark_borders(&warped);
    whiten_desk_fringe(&mut trimmed);

    let final_img = enhance::clean_scan(&trimmed, mode);
    save_debug(debug_dir, &file_label, "2_final", &final_img);

    Ok(final_img)
}

fn pixel_luma(p: &image::Rgb<u8>) -> u8 {
    (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).round() as u8
}

/// Fraction of pixels on a row/column that look like paper (bright) rather than desk.
fn bright_fraction(values: impl Iterator<Item = u8>, threshold: u8) -> f64 {
    let mut total = 0u32;
    let mut bright = 0u32;
    for v in values {
        total += 1;
        if v >= threshold {
            bright += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        bright as f64 / total as f64
    }
}

/// Crops dark leftover background from the edges after the perspective warp.
/// Stops as soon as a strip looks like paper, so handwriting in the margins is kept.
/// Caps how far it can walk inward so a shadowed page can't get eaten alive.
fn trim_dark_borders(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    if w < 20 || h < 20 {
        return img.clone();
    }

    let max_x = (w as f64 * 0.04).round() as u32;
    let max_y = (h as f64 * 0.04).round() as u32;
    // Paper is bright even in light shadow; desk/wood/keyboard are much darker.
    const PAPER_LUMA: u8 = 165;
    const PAPER_COVERAGE: f64 = 0.42;

    let row_is_paper = |y: u32| {
        bright_fraction((0..w).map(|x| pixel_luma(img.get_pixel(x, y))), PAPER_LUMA)
            >= PAPER_COVERAGE
    };
    let col_is_paper = |x: u32| {
        bright_fraction((0..h).map(|y| pixel_luma(img.get_pixel(x, y))), PAPER_LUMA)
            >= PAPER_COVERAGE
    };

    let mut top = 0u32;
    while top < max_y && !row_is_paper(top) {
        top += 1;
    }
    let mut bottom = 0u32;
    while bottom < max_y && !row_is_paper(h - 1 - bottom) {
        bottom += 1;
    }
    let mut left = 0u32;
    while left < max_x && !col_is_paper(left) {
        left += 1;
    }
    let mut right = 0u32;
    while right < max_x && !col_is_paper(w - 1 - right) {
        right += 1;
    }

    let new_w = w.saturating_sub(left + right);
    let new_h = h.saturating_sub(top + bottom);
    if new_w == 0 || new_h == 0 || (left == 0 && right == 0 && top == 0 && bottom == 0) {
        return img.clone();
    }
    image::imageops::crop_imm(img, left, top, new_w, new_h).to_image()
}

/// Wood/desk leftover usually sits in the corners after a perspective warp. Black ink is
/// nearly grey (low chroma); desk is brown (higher chroma). Only the brown pixels near
/// the border are painted white, so handwriting in the margin is left alone.
fn whiten_desk_fringe(img: &mut RgbImage) {
    let (w, h) = img.dimensions();
    let mx = ((w as f64) * 0.035).round() as u32;
    let my = ((h as f64) * 0.035).round() as u32;

    for y in 0..h {
        for x in 0..w {
            let dx = x.min(w - 1 - x);
            let dy = y.min(h - 1 - y);
            if dx > mx && dy > my {
                continue;
            }
            let p = img.get_pixel(x, y);
            if looks_like_desk(p) {
                img.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
    }
}

fn looks_like_desk(p: &image::Rgb<u8>) -> bool {
    let luma = pixel_luma(p);
    if luma >= 175 {
        return false;
    }
    let maxc = p[0].max(p[1]).max(p[2]) as i16;
    let minc = p[0].min(p[1]).min(p[2]) as i16;
    maxc - minc > 12
}

fn save_debug(debug_dir: &Option<PathBuf>, file_label: &str, tag: &str, img: &RgbImage) {
    let Some(dir) = debug_dir else { return };
    if std::fs::create_dir_all(dir).is_ok() {
        let stem = Path::new(file_label)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let out_path = dir.join(format!("{stem}_{tag}.png"));
        if let Err(e) = img.save(&out_path) {
            eprintln!("  (debug image save failed: {e})");
        }
    }
}

fn is_image_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("heic") | Some("heif")
    )
}

/// Picks the more recent of a file's modified/created times, if either is readable.
fn newest_timestamp(meta: &std::fs::Metadata) -> Option<SystemTime> {
    match (meta.modified().ok(), meta.created().ok()) {
        (Some(m), Some(c)) => Some(m.max(c)),
        (Some(m), None) => Some(m),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

fn is_recent(meta: &std::fs::Metadata, window: Duration, now: SystemTime) -> bool {
    match newest_timestamp(meta) {
        // If we can't read either timestamp, err on the side of including the file
        // rather than silently dropping a photo the user expected to see scanned.
        Some(t) => now
            .duration_since(t)
            .map(|age| age <= window)
            .unwrap_or(true),
        None => true,
    }
}

/// True if the photo is inside the time window by filesystem time *or* by the timestamp
/// baked into typical iPhone filenames (`YYYYMMDD_HHMMSS`). OneDrive's archive step can
/// leave a file in `2026\09` whose mtime doesn't match when it was actually taken, and
/// those filenames are often UTC, so we accept either local or UTC interpretation.
fn is_recent_photo(
    path: &Path,
    meta: &std::fs::Metadata,
    window: Duration,
    now: SystemTime,
) -> bool {
    if is_recent(meta, window, now) {
        return true;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|name| filename_is_recent(name, window, now))
        .unwrap_or(false)
}

fn filename_is_recent(name: &str, window: Duration, now: SystemTime) -> bool {
    let Some((year, month, day, hour, min, sec)) = filename_datetime_parts(name) else {
        return false;
    };
    let Some(naive) =
        NaiveDate::from_ymd_opt(year, month, day).and_then(|d| d.and_hms_opt(hour, min, sec))
    else {
        return false;
    };

    let mut candidates = Vec::new();
    candidates.push(naive.and_utc().into());
    if let Some(local) = chrono::Local.from_local_datetime(&naive).single() {
        candidates.push(local.into());
    }

    candidates.into_iter().any(|t| {
        now.duration_since(t)
            .map(|age| age <= window)
            .unwrap_or(true) // timestamp in the future: clock skew / UTC vs local, keep it
    })
}

fn filename_datetime_parts(name: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let bytes = name.as_bytes();
    if bytes.len() < 15 || bytes[8] != b'_' {
        return None;
    }
    let is_digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    if !is_digits(&bytes[0..8]) || !is_digits(&bytes[9..15]) {
        return None;
    }
    let year: i32 = name[0..4].parse().ok()?;
    let month: u32 = name[4..6].parse().ok()?;
    let day: u32 = name[6..8].parse().ok()?;
    let hour: u32 = name[9..11].parse().ok()?;
    let min: u32 = name[11..13].parse().ok()?;
    let sec: u32 = name[13..15].parse().ok()?;
    Some((year, month, day, hour, min, sec))
}

/// Parses a leading `YYYYMMDD_HHMMSS` (optionally `...mmm` milliseconds) prefix, as used
/// by iOS/Android camera exports, into a single comparable integer. Returns `None` for
/// filenames that don't start with that pattern.
fn parse_filename_timestamp(name: &str) -> Option<i64> {
    let bytes = name.as_bytes();
    if bytes.len() < 15 || bytes[8] != b'_' {
        return None;
    }
    let is_digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    if !is_digits(&bytes[0..8]) || !is_digits(&bytes[9..15]) {
        return None;
    }
    let millis = if bytes.len() >= 18 && is_digits(&bytes[15..18]) {
        &name[15..18]
    } else {
        "000"
    };
    format!("{}{}{}", &name[0..8], &name[9..15], millis)
        .parse::<i64>()
        .ok()
}

// ==================================== Shared: output path ====================================

/// Resolves the output PDF path.
/// - An explicit `--output` ending in `.pdf` is used as-is (and `--name` is ignored).
/// - Otherwise `--output` (or the Downloads folder, by default) is treated as a
///   directory, and `--name` (or an auto-generated timestamp) supplies the filename.
fn resolve_output(output: &Option<PathBuf>, name: &Option<String>) -> Result<PathBuf> {
    let filename = match name {
        Some(n) => {
            let n = n.trim();
            if n.to_ascii_lowercase().ends_with(".pdf") {
                n.to_string()
            } else {
                format!("{n}.pdf")
            }
        }
        None => {
            let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
            format!("Scan_{stamp}.pdf")
        }
    };

    let resolved = match output {
        Some(p)
            if p.extension()
                .map(|e| e.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false) =>
        {
            if name.is_some() {
                eprintln!(
                    "Note: --output already names a file ('{}'), ignoring --name.",
                    p.display()
                );
            }
            p.clone()
        }
        Some(dir) => dir.join(&filename),
        None => {
            let downloads = dirs::download_dir().unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("Downloads")
            });
            downloads.join(&filename)
        }
    };
    Ok(resolved)
}
