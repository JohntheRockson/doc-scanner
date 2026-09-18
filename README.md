# paper_scanner

A Windows command-line tool that turns photos of paper into a clean, perspective-corrected, shadow-free PDF — the kind of result you would expect from a document-scanner app, without a flatbed.

The intended loop is: photograph homework (or any sheet of paper) on an iPhone, upload the photos to a Cloudflare R2 bucket (typically via an iOS Shortcut), run this binary on a Windows laptop, and get a scanned PDF in Downloads. After a successful scan the originals are deleted from the bucket so it stays empty for the next batch.

Local OneDrive Camera Roll photos still work as a fallback.

The Cargo package is named `paper_scanner`; this GitHub repo is `doc-scanner`.

---

## Table of contents

- [What it does](#what-it-does)
- [Typical workflow](#typical-workflow)
- [Requirements](#requirements)
- [Setup](#setup)
- [Build](#build)
- [Quick start](#quick-start)
- [Command-line reference](#command-line-reference)
- [Input sources](#input-sources)
- [Output](#output)
- [How a photo becomes a page](#how-a-photo-becomes-a-page)
- [Orientation](#orientation)
- [Project layout](#project-layout)
- [Development helpers](#development-helpers)
- [Troubleshooting](#troubleshooting)
- [Privacy](#privacy)

---

## What it does

For each input photo the tool:

1. Decodes the file (HEIC/HEIF, and JPEG-in-disguise files that still use a `.heic` extension).
2. Applies EXIF orientation so the page matches how the phone was held.
3. Finds the sheet of paper in the frame, even when it is photographed at an angle (a trapezoid / keystone).
4. Warps that quadrilateral into a face-on rectangle.
5. Trims leftover desk/background around the edges without eating handwriting in the margins.
6. Flattens shadows and stretches contrast into a “scanned” look.
7. Appends the page to a multi-page PDF.

If paper detection is not confident, the whole photo is used rather than failing the run.

---

## Typical workflow

```text
iPhone camera
    │  (iOS Shortcut uploads each shot)
    ▼
Cloudflare R2 bucket  ──paper_scanner.exe──►  Downloads\Scan_<timestamp>.pdf
    │                                           ▲
    └──── originals deleted after success ──────┘
```

1. Take photos of each page. Portrait or landscape is fine; hold the phone the way you want the PDF page to read.
2. Let the Shortcut upload them to R2. Filenames like `scan_20260913_232143_p1.heic` are typical.
3. On the Windows laptop, run `paper_scanner.exe` with no arguments.
4. Open the PDF that lands in your Downloads folder.
5. The scanned objects are removed from R2. Pass `--keep-remote` if you want to leave them there.

---

## Requirements

| Piece | Why |
| --- | --- |
| **Windows** | HEIC decoding uses the Windows Imaging Component (WIC). There is no Linux/macOS build. |
| **Rust toolchain** | Rust 2024 edition (`rustc` 1.85 or newer). [rustup](https://rustup.rs/) is the usual install. |
| **HEIF Image Extensions** | Free Microsoft Store package that teaches Windows how to decode HEIC. Already present on most PCs that have previewed iPhone photos in Explorer or the Photos app. |
| **Cloudflare R2 bucket** | Default input source. Not required if you only use `--local`. |
| **`.env` file** | R2 endpoint + API token. Never commit this file. |

---

## Setup

### 1. Clone and enter the repo

```powershell
git clone https://github.com/JohntheRockson/doc-scanner.git
cd doc-scanner
```

### 2. Install HEIF Image Extensions (if needed)

Open Microsoft Store, search for **HEIF Image Extensions**, and install it. If File Explorer already shows iPhone `.heic` thumbnails, you already have it.

### 3. Configure Cloudflare R2

Create a file named `.env` in the repo root (the same folder as `Cargo.toml`). It is gitignored.

```env
R2_ENDPOINT_URL=https://<ACCOUNT_ID>.r2.cloudflarestorage.com
R2_ACCESS_KEY_ID=your_r2_access_key_id
R2_SECRET_KEY=your_r2_secret_access_key
R2_BUCKET_NAME=your-bucket-name
```

`paper_scanner` reads exactly these four:

| Variable | Meaning |
| --- | --- |
| `R2_ENDPOINT_URL` | S3-compatible R2 endpoint, e.g. `https://<accountid>.r2.cloudflarestorage.com` |
| `R2_ACCESS_KEY_ID` | R2 API token access key |
| `R2_SECRET_KEY` | R2 API token secret |
| `R2_BUCKET_NAME` | Bucket that the Shortcut uploads into |

Create the token in the Cloudflare dashboard under **R2 → Manage R2 API Tokens**. The token needs permission to list, read, and delete objects in that bucket. Region is always `auto`; the code sets that for you.

The `.env` is loaded at startup via `dotenvy`. Missing variables only matter in R2 mode — `--local` runs without them.

---

## Build

From the repo root, in PowerShell or cmd:

```powershell
cargo build --release
```

The binary is:

```text
target\release\paper_scanner.exe
```

On this machine it has also been built with `CARGO_TARGET_DIR` pointed at `C:\Users\miskh\pdf\target`. Either path is the same program; use whichever `target` directory you actually built into.

Rebuild after pulling new commits:

```powershell
cargo build --release
```

---

## Quick start

List what is waiting in R2 (nothing is downloaded or deleted):

```powershell
.\target\release\paper_scanner.exe --list
```

Scan **everything** in the bucket into one PDF in Downloads, then delete the scanned objects:

```powershell
.\target\release\paper_scanner.exe
```

Scan two specific files, keep them in R2, and name the PDF:

```powershell
.\target\release\paper_scanner.exe scan_20260913_232143_p1.heic scan_20260913_232143_p2.heic --keep-remote -n homework_1
```

Scan local Camera Roll photos from the last 5 minutes:

```powershell
.\target\release\paper_scanner.exe --local
```

Scan a specific local file or folder (this switches to local mode automatically — no `--local` flag needed):

```powershell
.\target\release\paper_scanner.exe "C:\Users\miskh\OneDrive\Pictures\Camera Roll 1\2026\09"
```

---

## Command-line reference

```text
paper_scanner [OPTIONS] [ITEMS]...
```

`ITEMS` is zero or more names. How they are interpreted depends on the mode (see [Input sources](#input-sources)).

| Flag | Default | What it does |
| --- | --- | --- |
| `ITEMS...` | *(all matching files)* | In R2 mode: object keys or filenames. Matching is case-insensitive by full key **or** by the filename alone, so you do not have to type a prefix. In local mode: files or directories on disk. |
| `--local` | off | Force local-filesystem mode. Only required when you pass **no** items and want the default Camera Roll folder. Naming an existing local path already switches to local mode. |
| `--list` | off | Print the available files and exit. Nothing is downloaded, processed, or deleted. |
| `--prefix <STR>` | none | R2 only. Restrict listing to keys that start with this prefix (a “folder” inside the bucket). Ignored with `--local`. |
| `-m`, `--minutes <N>` | R2: none / local default folder: **5** | Only include files created or modified in the last *N* minutes. Named items on the command line always bypass the time filter. |
| `--all` | off | Process every matching file, ignoring `--minutes`. |
| `--keep-remote` | off | Leave originals in R2 after a successful scan. Ignored in local mode — local files are **never** deleted. |
| `-o`, `--output <PATH>` | your Downloads folder | Directory to write the PDF into. If this path ends in `.pdf`, it is used as the exact output file and `--name` is ignored. |
| `-n`, `--name <NAME>` | `Scan_<YYYY-MM-DD_HHMMSS>.pdf` | Output filename, with or without `.pdf`. |
| `--mode <MODE>` | `color` | Page look: `color`, `gray`, or `bw`. |
| `--debug <DIR>` | off | Write intermediate PNGs (`*_1_warped.png` and `*_2_final.png`) per photo into this directory. Useful when detection or cropping looks wrong. |
| `-h`, `--help` | | Full clap help text. |
| `-V`, `--version` | | Crate version (`0.1.0`). |

### `--mode` values

| Value | Result |
| --- | --- |
| `color` | Shadow-corrected color, like a typical phone scanner app’s “auto color”. |
| `gray` | Same cleanup, then grayscale. |
| `bw` | Adaptive-threshold black and white, like a classic flatbed text scan. |

### Sample commands

```powershell
# Everything in R2 → Downloads\Scan_<timestamp>.pdf, then delete from R2
.\target\release\paper_scanner.exe

# Peek at the bucket
.\target\release\paper_scanner.exe --list

# Only objects under a prefix
.\target\release\paper_scanner.exe --prefix homework/ --list

# Choose files by filename (prefix optional)
.\target\release\paper_scanner.exe p1.heic p2.heic

# Keep the R2 originals
.\target\release\paper_scanner.exe --keep-remote

# Custom destination + name
.\target\release\paper_scanner.exe -o D:\scans -n CCE203_HW1

# Exact output path (overrides --name)
.\target\release\paper_scanner.exe -o D:\scans\assignment.pdf

# Only photos uploaded in the last 15 minutes
.\target\release\paper_scanner.exe --minutes 15

# Local Camera Roll, last 5 minutes (the local default)
.\target\release\paper_scanner.exe --local

# Local Camera Roll, no time window
.\target\release\paper_scanner.exe --local --all

# Black-and-white pages, plus debug images
.\target\release\paper_scanner.exe --mode bw --debug .\debug_out
```

---

## Input sources

The program picks a mode once at startup:

```text
--local  OR  any ITEM that already exists on disk  →  local mode
otherwise                                          →  Cloudflare R2 mode
```

### Cloudflare R2 (default)

1. Connects with the `.env` credentials.
2. Lists the bucket (`--prefix` if given), following S3 pagination.
3. `--list` prints key, size, and last-modified, then exits.
4. Keeps objects whose key ends in `.heic` or `.heif` (case-insensitive).
5. If you named `ITEMS`, selects by exact key or by filename. Missing names are warned and skipped.
6. If you named nothing, every image object is selected. `--minutes` is applied only in this “process the whole bucket” case, and only if you actually passed it. R2 has **no** implicit 5-minute window.
7. Each object is downloaded to `%TEMP%\paper_scanner_r2\`, processed, and the temp file is deleted.
8. Surviving pages are written to one PDF.
9. Unless `--keep-remote` is set, each **successfully processed** key is deleted from the bucket. A photo that failed to decode is left in place.

R2 access is plain HTTPS + the S3 REST API (`aws-sdk-s3` pointed at the R2 endpoint). There is no Worker, WASM wrapper, or browser involved.

### Local filesystem

Used when `--local` is set, or when any `ITEM` is an existing file or directory.

| How you invoke it | What is scanned |
| --- | --- |
| `--local` with no items | `C:\Users\miskh\OneDrive\Pictures\Camera Roll 1`, recursively, including dated `YYYY\MM` subfolders. Default time window: **last 5 minutes**. |
| Existing file path(s) | Those files, no time filter. |
| Existing directory path(s) | Recursed like the default folder. Time window applies unless `--all`. |

The default Camera Roll path is a constant in `src/main.rs` (`DEFAULT_INPUT_DIR`). Point the tool at a different folder by passing that folder as an argument.

Local extras:

- **Dedup by filename.** OneDrive can briefly show the same photo in both the Camera Roll root and a dated subfolder. The second copy is skipped so it does not become a duplicate PDF page.
- **Sort by capture time.** Prefer the `YYYYMMDD_HHMMSS` prefix used by iPhone exports (e.g. `20260909_045421598_iOS.heic`) over filesystem mtime, because OneDrive sync can rewrite mtimes. Pages come out in the order the photos were taken.
- **Time window uses both mtime and the filename timestamp**, tried as UTC and as local time, so a UTC-named iPhone file still counts as “recent.”
- **Local files are never deleted.**

### Which files count as photos

Only keys/paths whose extension is `.heic` or `.heif` are selected. A `.jpg` sitting in the bucket is ignored.

The decoder itself is not limited to HEIF: WIC sniffs file contents. An iOS Shortcut that **re-encodes JPEG** and then names the file `.heic` still works. That case is why EXIF orientation is read from JPEG APP1 (`/app1/ifd/{ushort=274}`) as well as from HEIF-style paths.

---

## Output

| Setting | Default |
| --- | --- |
| Folder | Windows Downloads (`dirs::download_dir()`), falling back to `%USERPROFILE%\Downloads` |
| Filename | `Scan_YYYY-MM-DD_HHMMSS.pdf` |
| Page size | Whatever the warped page actually is, at **200 DPI** — portrait photos become portrait pages, landscape stay landscape |
| Pages | One PDF page per successfully processed photo, in input order |

`-o` / `--output`:

- Directory → PDF is written inside it, using `--name` or the timestamped default.
- Path ending in `.pdf` → used as the exact file; `--name` is ignored (a note is printed).

Parent directories are created if missing.

---

## How a photo becomes a page

```text
.heic / .heif
    │
    ▼
WIC decode + EXIF orientation          src/heic.rs
    │
    ▼
Find paper quad                        src/detect.rs
    │  downscale → blur → Otsu (both polarities)
    │  morphological close → contours → convex hull
    │  4 extreme corners → sanity checks
    │  relabel corners to match photo portrait/landscape
    │
    ▼
Perspective warp to a rectangle        src/main.rs + src/geometry.rs
    │  2.5% outward nudge so page edges are not clipped
    │  samples outside the photo fill white
    │
    ▼
Trim dark desk borders                 src/main.rs
Whiten brown desk fringe in corners
    │
    ▼
Flatten shadows + auto-contrast        src/enhance.rs
Optional gray / bw
    │
    ▼
Append page to PDF @ 200 DPI           src/pdf.rs
```

Detection details that matter in practice:

- Runs on a copy whose longest side is 1100 px. The warp always samples the full-resolution original.
- Both “paper brighter than background” and “paper darker than background” masks are tried, plus a second, stricter split of the bright side (so white paper on a light desk does not merge into one blob).
- A candidate that covers less than ~6% or more than ~92% of the frame, or that hugs all four borders, is rejected. That stops “the whole desk, keyboard and all” from being treated as the page.
- If nothing passes, the full photo is used and a note is printed.

Cleanup details:

- Shadow removal divides each pixel by a heavily blurred estimate of local background brightness, then stretches so the background maps to white.
- Auto-contrast clips about 0.5% of the luma histogram at each end, with one shared low/high so color balance is preserved.
- Border trim walks inward at most 4% of width/height and stops as soon as a strip looks like paper, so problem numbers and top lines are not cropped off.

---

## Orientation

PDF pages follow **how the picture was taken**, not a forced landscape layout.

Two separate issues were involved; both are handled:

1. **EXIF / IFD0 orientation.** Real Camera Roll HEICs are often already rotated by WIC. Shortcut exports, however, are frequently **JPEG bytes stored under a `.heic` name**, with `Orientation = 6` (rotate 90° CW) sitting in the main TIFF IFD0. The decoder queries `/app1/ifd/{ushort=274}` first, then `/ifd/{ushort=274}`, then the older `/exif/`-suffixed paths as fallbacks. Missing the JPEG path used to leave every Shortcut photo at the sensor-native 4032×3024 landscape size.
2. **Corner labeling on steep tilts.** Reducing a hull to four corners with the min/max(x±y) heuristic silently swaps width and height when the page is tilted more than about 45°. After the warp size is computed, the four labels are rotated a quarter-turn if that size disagrees with the source photo’s portrait/landscape.

---

## Project layout

```text
doc-scanner/
├── Cargo.toml              crate name: paper_scanner
├── .env                    R2 secrets (gitignored — you create this)
├── .gitignore
├── src/
│   ├── main.rs             CLI dispatch, local/R2 orchestration, trim/cleanup
│   ├── cli.rs              clap flags and help text
│   ├── heic.rs             WIC decode + EXIF orientation
│   ├── detect.rs           paper-quad detection
│   ├── geometry.rs         hull, corners, quad sizing
│   ├── enhance.rs          shadows, contrast, color/gray/bw
│   ├── pdf.rs              multi-page PDF writer
│   └── r2.rs               list / download / delete against R2
└── examples/               dev-only helpers, not the shipped tool
```

---

## Development helpers

These are `cargo run --release --example …` programs used while tuning detection and R2. None of them ship as `paper_scanner.exe`.

| Example | Purpose |
| --- | --- |
| `make_test_photo` | Synthetic desk photo with keystone, shadow, and “ink,” encoded as real HEIC via WIC. |
| `make_rotated_test_photo` | Portrait photo of a steeply tilted portrait page, for the corner-labeling fix. |
| `make_blank_photo` | No paper region at all — exercises the full-image fallback. |
| `r2_test_upload` | Uploads local files to R2 under `TEST_`-prefixed keys for a safe round trip. |
| `r2_download` | Downloads named keys **without deleting** them, for inspection. |
| `decode_check` | Decode + orientation only, write a PNG. Independent of paper detection. |
| `inspect_orientation` | Print raw WIC size and the EXIF orientation tag. |
| `heif_box_dump` | Walk ISOBMFF boxes / JPEG headers and dump `irot`/`imir` plus TIFF Orientation, without WIC. |

Examples:

```powershell
cargo run --release --example decode_check -- photo.heic out.png
cargo run --release --example r2_download -- scan_20260913_232143_p1.heic
cargo run --release --example heif_box_dump -- photo.heic
```

---

## Troubleshooting

**`missing R2_ENDPOINT_URL` (or another R2 variable)**  
Create `.env` in the repo root, or pass `--local`. The error text names the missing variable.

**`Windows couldn't open '…' as an image`**  
Install **HEIF Image Extensions** from the Microsoft Store and retry. Also confirm the file is actually an image; a 0-byte or truncated upload will fail the same way.

**Pages come out sideways / always landscape**  
Rebuild from a commit that includes the IFD0 orientation query (`/app1/ifd/{ushort=274}`). Shortcut files that are JPEG-under-`.heic` need that path. Run `decode_check` on one file: you want a non-1 orientation tag and a final size that matches how you held the phone.

**A page includes the desk, keyboard, or the whole scene**  
Detection fell back to the full photo. Re-shoot with the page filling more of the frame and some margin of desk visible on at least one side. `--debug some_dir` writes the warped and final PNGs so you can see whether the quad was wrong or the trim was.

**A page is cropped too tightly (problem numbers missing)**  
Unlikely with the current content-aware trim (capped at 4%). If it still happens, the detected quad was inside the ink. `--debug` again; a slightly farther camera position usually fixes it.

**`--list` shows files but a scan skips them**  
Only `.heic` / `.heif` extensions are processed. Rename `.jpg` uploads or have the Shortcut use `.heic`.

**R2 objects remain after a scan**  
Either the photo failed to process (it is left on purpose), `--keep-remote` was set, or delete returned an error (a warning is printed per key). Check the API token has delete permission.

**Local mode does not see this month’s Camera Roll**  
It recurses into `YYYY\MM`. Confirm the photos are under `C:\Users\miskh\OneDrive\Pictures\Camera Roll 1` (or pass that folder explicitly). Default window is 5 minutes — use `--all` or `--minutes 60`.

**Duplicates in the PDF**  
Should not happen for the same filename in two OneDrive locations. Two genuinely different files with different names are two pages.

**I only wanted to look, not scan**  
`--list`. It never downloads, writes a PDF, or deletes.

---

## Privacy

- `.env`, `.env.*`, and `*.env` are gitignored. Do not commit R2 keys.
- R2 deletes happen only after the PDF has been written, and only for photos that actually made it into that PDF.
- Local Camera Roll files are never removed.
- Debug PNGs (`--debug`) are full-resolution page images; they can contain handwriting. Keep that folder out of git (`/test_assets` is already ignored).
