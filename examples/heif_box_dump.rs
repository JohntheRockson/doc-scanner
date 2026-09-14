//! Dev-only helper: walks the raw ISOBMFF/HEIF box structure of a .heic file looking for
//! any orientation signal - the `irot`/`imir` transformative item properties in
//! `meta/iprp/ipco`, and a manual parse of the embedded `Exif` item's TIFF IFD0 tag 0x0112 -
//! completely independent of what WIC chooses to expose. Not part of the shipped tool.
//! Usage: cargo run --release --example heif_box_dump -- <file.heic>

use std::env;
use std::fs;

struct BoxHeader {
    kind: [u8; 4],
    // Byte range of the box's *content* (after the header) within the parent buffer.
    start: usize,
    end: usize,
}

fn read_boxes(buf: &[u8]) -> Vec<BoxHeader> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let size32 = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as u64;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&buf[pos + 4..pos + 8]);
        let (header_len, total_size) = if size32 == 1 {
            if pos + 16 > buf.len() {
                break;
            }
            let size64 = u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
            (16u64, size64)
        } else if size32 == 0 {
            (8u64, (buf.len() - pos) as u64)
        } else {
            (8u64, size32)
        };
        if total_size < header_len || pos as u64 + total_size > buf.len() as u64 {
            break;
        }
        let content_start = pos + header_len as usize;
        let content_end = pos + total_size as usize;
        out.push(BoxHeader { kind, start: content_start, end: content_end });
        pos = content_end;
    }
    out
}

fn kind_str(k: &[u8; 4]) -> String {
    String::from_utf8_lossy(k).to_string()
}

fn find<'a>(boxes: &'a [BoxHeader], name: &str) -> Option<&'a BoxHeader> {
    boxes.iter().find(|b| kind_str(&b.kind) == name)
}

fn main() {
    let path = env::args().nth(1).expect("usage: heif_box_dump <file.heic>");
    let buf = fs::read(&path).expect("read file");
    println!("== {path} ({} bytes) ==", buf.len());

    print!("First 64 bytes hex: ");
    for b in buf.iter().take(64) {
        print!("{b:02x} ");
    }
    println!();
    print!("First 64 bytes ascii: ");
    for b in buf.iter().take(64) {
        let c = *b as char;
        print!("{}", if c.is_ascii_graphic() { c } else { '.' });
    }
    println!();

    let top = read_boxes(&buf);
    println!("Top-level boxes: {}", top.iter().map(|b| kind_str(&b.kind)).collect::<Vec<_>>().join(", "));

    let meta = match find(&top, "meta") {
        Some(b) => b,
        None => {
            println!("No 'meta' box found at all - no HEIF item metadata present.");
            return;
        }
    };
    // meta is a FullBox: 4 bytes version+flags before its children.
    let meta_children_start = meta.start + 4;
    let meta_children = read_boxes(&buf[meta_children_start..meta.end]);
    println!(
        "meta children: {}",
        meta_children.iter().map(|b| kind_str(&b.kind)).collect::<Vec<_>>().join(", ")
    );

    // ---- iprp/ipco: look for irot/imir transform properties ----
    if let Some(iprp) = find(&meta_children, "iprp") {
        let iprp_abs_start = meta_children_start + iprp.start;
        let iprp_abs_end = meta_children_start + iprp.end;
        let iprp_children = read_boxes(&buf[iprp_abs_start..iprp_abs_end]);
        println!("iprp children: {}", iprp_children.iter().map(|b| kind_str(&b.kind)).collect::<Vec<_>>().join(", "));
        if let Some(ipco) = find(&iprp_children, "ipco") {
            let ipco_abs_start = iprp_abs_start + ipco.start;
            let ipco_abs_end = iprp_abs_start + ipco.end;
            let props = read_boxes(&buf[ipco_abs_start..ipco_abs_end]);
            println!("ipco properties ({} total):", props.len());
            let mut found_transform = false;
            for (i, p) in props.iter().enumerate() {
                let name = kind_str(&p.kind);
                let abs_start = ipco_abs_start + p.start;
                let abs_end = ipco_abs_start + p.end;
                if name == "irot" {
                    found_transform = true;
                    let angle_byte = buf[abs_start] & 0x03;
                    println!("  [{i}] irot  angle_field={angle_byte} (=> {} degrees CW per HEIF spec)", angle_byte as u32 * 90);
                } else if name == "imir" {
                    found_transform = true;
                    let axis_byte = buf[abs_start] & 0x01;
                    println!("  [{i}] imir  axis_field={axis_byte}");
                } else if name == "ispe" {
                    if abs_end - abs_start >= 12 {
                        let w = u32::from_be_bytes(buf[abs_start + 4..abs_start + 8].try_into().unwrap());
                        let h = u32::from_be_bytes(buf[abs_start + 8..abs_start + 12].try_into().unwrap());
                        println!("  [{i}] ispe  (declared image size) {w}x{h}");
                    }
                } else {
                    println!("  [{i}] {name}");
                }
            }
            if !found_transform {
                println!("  ===> NO irot/imir property present anywhere in this file's ipco. There is zero container-level rotation signal.");
            }
        } else {
            println!("No 'ipco' box inside iprp.");
        }
    } else {
        println!("No 'iprp' box found - no item properties (incl. no irot/imir) at all.");
    }

    // ---- Exif item: find via iinf + iloc, then manually parse TIFF IFD0 tag 0x0112 ----
    dump_exif_orientation(&buf, meta, meta_children_start, &meta_children);
}

fn dump_exif_orientation(buf: &[u8], _meta: &BoxHeader, meta_children_start: usize, meta_children: &[BoxHeader]) {
    let iinf = match find(meta_children, "iinf") {
        Some(b) => b,
        None => {
            println!("No 'iinf' box - can't locate an Exif item.");
            return;
        }
    };
    let iinf_abs_start = meta_children_start + iinf.start;
    // iinf: version(1)+flags(3), then either u16 or u32 entry count, then a sequence of
    // 'infe' FullBoxes.
    let version = buf[iinf_abs_start];
    let mut p = iinf_abs_start + 4;
    let count = if version == 0 {
        let c = u16::from_be_bytes(buf[p..p + 2].try_into().unwrap()) as u32;
        p += 2;
        c
    } else {
        let c = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap());
        p += 4;
        c
    };
    let infe_boxes = read_boxes(&buf[p..meta_children_start + iinf.end]);
    let mut exif_item_id: Option<u32> = None;
    for b in infe_boxes.iter().take(count as usize) {
        if kind_str(&b.kind) != "infe" {
            continue;
        }
        let abs_start = p + b.start;
        let infe_ver = buf[abs_start];
        // version >= 2 is what modern HEIC uses.
        let (item_id, type_off) = if infe_ver == 2 {
            let id = u16::from_be_bytes(buf[abs_start + 4..abs_start + 6].try_into().unwrap()) as u32;
            (id, abs_start + 8)
        } else if infe_ver == 3 {
            let id = u32::from_be_bytes(buf[abs_start + 4..abs_start + 8].try_into().unwrap());
            (id, abs_start + 10)
        } else {
            continue;
        };
        let item_type = kind_str(&buf[type_off..type_off + 4].try_into().unwrap());
        if item_type == "Exif" {
            exif_item_id = Some(item_id);
        }
    }
    let exif_item_id = match exif_item_id {
        Some(id) => {
            println!("Found Exif item, id={id}");
            id
        }
        None => {
            println!("No item of type 'Exif' found in iinf - there is no EXIF blob in this file at all.");
            return;
        }
    };

    let iloc = match find(meta_children, "iloc") {
        Some(b) => b,
        None => {
            println!("No 'iloc' box - can't resolve the Exif item's location.");
            return;
        }
    };
    let iloc_abs_start = meta_children_start + iloc.start;
    let iloc_ver = buf[iloc_abs_start];
    let b4 = buf[iloc_abs_start + 4];
    let offset_size = (b4 >> 4) & 0xF;
    let length_size = b4 & 0xF;
    let b5 = buf[iloc_abs_start + 5];
    let base_offset_size = (b5 >> 4) & 0xF;
    let mut q = iloc_abs_start + 6;
    if iloc_ver == 1 || iloc_ver == 2 {
        q += 0; // index_size shares byte 5's low nibble in some specs; keep simple, not needed here.
    }
    let item_count = if iloc_ver < 2 {
        let c = u16::from_be_bytes(buf[q..q + 2].try_into().unwrap()) as u32;
        q += 2;
        c
    } else {
        let c = u32::from_be_bytes(buf[q..q + 4].try_into().unwrap());
        q += 4;
        c
    };
    for _ in 0..item_count {
        let item_id = if iloc_ver < 2 {
            let v = u16::from_be_bytes(buf[q..q + 2].try_into().unwrap()) as u32;
            q += 2;
            v
        } else {
            let v = u32::from_be_bytes(buf[q..q + 4].try_into().unwrap());
            q += 4;
            v
        };
        if iloc_ver == 1 || iloc_ver == 2 {
            q += 2; // construction_method
        }
        q += 2; // data_reference_index
        let base_offset = read_sized(buf, &mut q, base_offset_size);
        let extent_count = u16::from_be_bytes(buf[q..q + 2].try_into().unwrap()) as u32;
        q += 2;
        let mut first_extent: Option<(u64, u64)> = None;
        for _ in 0..extent_count {
            let ext_offset = read_sized(buf, &mut q, offset_size);
            let ext_len = read_sized(buf, &mut q, length_size);
            if first_extent.is_none() {
                first_extent = Some((ext_offset, ext_len));
            }
        }
        if item_id == exif_item_id {
            if let Some((ext_offset, _ext_len)) = first_extent {
                let abs = (base_offset + ext_offset) as usize;
                // Per HEIF: first 4 bytes = big-endian offset to the TIFF header from the
                // byte immediately following this 4-byte field.
                let tiff_rel_offset = u32::from_be_bytes(buf[abs..abs + 4].try_into().unwrap()) as usize;
                let tiff_start = abs + 4 + tiff_rel_offset;
                parse_tiff_orientation(buf, tiff_start);
            }
            return;
        }
    }
    println!("Exif item id {exif_item_id} not found among iloc entries.");
}

fn read_sized(buf: &[u8], pos: &mut usize, size: u8) -> u64 {
    let v = match size {
        0 => 0,
        4 => u32::from_be_bytes(buf[*pos..*pos + 4].try_into().unwrap()) as u64,
        8 => u64::from_be_bytes(buf[*pos..*pos + 8].try_into().unwrap()),
        _ => 0,
    };
    *pos += size as usize;
    v
}

fn parse_tiff_orientation(buf: &[u8], tiff_start: usize) {
    let byte_order = &buf[tiff_start..tiff_start + 2];
    let little_endian = byte_order == b"II";
    println!("TIFF header at file offset {tiff_start}, byte order = {}", if little_endian { "little (II)" } else { "big (MM)" });
    let read_u16 = |p: usize| -> u16 {
        if little_endian {
            u16::from_le_bytes(buf[p..p + 2].try_into().unwrap())
        } else {
            u16::from_be_bytes(buf[p..p + 2].try_into().unwrap())
        }
    };
    let read_u32 = |p: usize| -> u32 {
        if little_endian {
            u32::from_le_bytes(buf[p..p + 4].try_into().unwrap())
        } else {
            u32::from_be_bytes(buf[p..p + 4].try_into().unwrap())
        }
    };
    let ifd0_offset = read_u32(tiff_start + 4) as usize;
    let ifd0 = tiff_start + ifd0_offset;
    let num_entries = read_u16(ifd0);
    println!("IFD0 has {num_entries} entries:");
    let mut found = false;
    for i in 0..num_entries as usize {
        let entry = ifd0 + 2 + i * 12;
        let tag = read_u16(entry);
        let value = read_u16(entry + 8); // for SHORT type, value sits in first 2 bytes of the 4-byte value field
        if tag == 0x0112 {
            found = true;
            println!("  ===> Orientation tag (0x0112) = {value}");
        }
    }
    if !found {
        println!("  ===> No Orientation tag (0x0112) present in IFD0. Zero EXIF rotation signal.");
    }
}
