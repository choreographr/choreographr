//! Unit tests for the HEIF/ISOBMFF geometry parser (`choreo-image::heif`).
//!
//! Kept in a sibling module (mirroring `choreo-tui/src/selection/tests.rs`) so
//! the parser code in `heif.rs` stays focused; the helpers here build
//! hand-crafted minimal HEIF containers rather than decrypting real files, so
//! the geometry walking is exercised deterministically.

use super::*;

fn box_(box_type: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let size = (8 + content.len()) as u32;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(box_type);
    b.extend_from_slice(content);
    b
}

/// `meta` (full box) wrapping the given children.
fn meta_box(children: &[u8]) -> Vec<u8> {
    let mut c = vec![0, 0, 0, 0]; // version/flags
    c.extend_from_slice(children);
    box_(b"meta", &c)
}

/// A single `ispe` property box for a `w`×`h` extent.
fn ispe_box(w: u32, h: u32) -> Vec<u8> {
    let mut c = vec![0; 4]; // version/flags
    c.extend_from_slice(&w.to_be_bytes());
    c.extend_from_slice(&h.to_be_bytes());
    box_(b"ispe", &c)
}

/// `meta > iprp > ipco > ispe` wrapping one extent.
fn extents_container(w: u32, h: u32) -> Vec<u8> {
    let ipco = box_(b"ipco", &ispe_box(w, h));
    let iprp = box_(b"iprp", &ipco);
    meta_box(&iprp)
}

/// An `infe` entry (version 2) for `item_id` of the given item type.
fn infe2(item_id: u16, item_type: &[u8; 4]) -> Vec<u8> {
    let mut c = vec![2, 0, 0, 0]; // version/flags
    c.extend_from_slice(&item_id.to_be_bytes());
    c.extend_from_slice(&0u16.to_be_bytes()); // protection_index
    c.extend_from_slice(item_type);
    box_(b"infe", &c)
}

/// `iinf` (full box, version 2 — u32 entry_count) listing the given items
/// (each an `infe` full box).
fn iinf(items: &[Vec<u8>]) -> Vec<u8> {
    let mut c = vec![2u8, 0, 0, 0]; // version 2, flags 0
    c.extend_from_slice(&(items.len() as u32).to_be_bytes());
    for it in items {
        c.extend_from_slice(it);
    }
    box_(b"iinf", &c)
}

/// `iloc` (version 2) with `offset_size=4, length_size=4,
/// base_offset_size=4`, mapping `items: [(item_id, offset, length)]`.
/// Version 2 item ids are u32.
fn iloc_v2(items: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut c = vec![2, 0, 0, 0]; // version/flags
    c.push(0x44); // offset_size=4, length_size=4
    c.push(0x40); // base_offset_size=4, index_size=0
    c.extend_from_slice(&(items.len() as u32).to_be_bytes());
    for &(id, off, len) in items {
        c.extend_from_slice(&id.to_be_bytes()); // u32 item id
        c.extend_from_slice(&0u16.to_be_bytes()); // construction_method
        c.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
        c.extend_from_slice(&0u32.to_be_bytes()); // base_offset
        c.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        c.extend_from_slice(&off.to_be_bytes());
        c.extend_from_slice(&len.to_be_bytes());
    }
    box_(b"iloc", &c)
}

/// A grid item payload: version(1) flags(1) rows(1) cols(1) [out_w, out_h].
fn grid_payload(rows: u8, cols: u8, out_w: u16, out_h: u16) -> Vec<u8> {
    let mut p = vec![0, 0, rows, cols];
    p.extend_from_slice(&out_w.to_be_bytes());
    p.extend_from_slice(&out_h.to_be_bytes());
    p
}

/// The pixel budget production uses (kept in lockstep with
/// [`crate::MAX_DECODE_PIXELS`] so the guard test exercises the real wiring).
const PIXEL_BUDGET: u64 = crate::MAX_DECODE_PIXELS;
/// The per-side cap production uses.
const MAX_SIDE: u32 = crate::MAX_SOURCE_DIMENSION;

#[test]
fn ispe_geometry_is_read_across_the_meta_fullbox_header() {
    let data = extents_container(4000, 3000);
    let geo = heif_geometry(&data).expect("should parse");
    assert_eq!((geo.max_ispe_w, geo.max_ispe_h), (4000, 3000));
    assert!(geo.grids.is_empty());
}

#[test]
fn rejects_when_no_ispe_geometry_is_found() {
    assert!(heif_geometry(b"").is_none());
    assert!(heif_geometry(b"not heif").is_none());
}

#[test]
fn ignores_geometry_inside_mdat() {
    // An `mdat` sibling after `meta` whose payload is an `ispe` lookalike
    // must NOT be parsed — it is raw media data.
    let mut data = extents_container(4000, 3000);
    data.extend_from_slice(&box_(b"mdat", &ispe_box(0xFFFF_FFFF, 0xFFFF_FFFF)));
    let geo = heif_geometry(&data).expect("should parse");
    assert_eq!((geo.max_ispe_w, geo.max_ispe_h), (4000, 3000));
}

#[test]
fn grid_canvas_is_bounded_from_grid_payload() {
    // A grid of 4x5 tiles (rows byte 3, cols byte 4), out 12000x9000, with
    // 3000x2000 tile extents → canvas 15000x8000, which must be rejected.
    let grid_payload = grid_payload(3, 4, 12000, 9000); // rows=4, cols=5
    let mut meta_children = Vec::new();
    meta_children.extend_from_slice(&iinf(&[infe2(1, b"hvc1"), infe2(100, b"grid")]));
    meta_children.extend_from_slice(&iloc_v2(&[(100, 0, grid_payload.len() as u32)]));
    let ipco = box_(b"ipco", &ispe_box(3000, 2000)); // tile extents
    let iprp = box_(b"iprp", &ipco);
    meta_children.extend_from_slice(&iprp);
    // The grid payload is appended after the meta box; its absolute offset
    // is the meta box's length.
    let off = meta_box(&meta_children).len();
    let mut meta_children2 = Vec::new();
    meta_children2.extend_from_slice(&iinf(&[infe2(1, b"hvc1"), infe2(100, b"grid")]));
    meta_children2.extend_from_slice(&iloc_v2(&[(100, off as u32, grid_payload.len() as u32)]));
    let ipco2 = box_(b"ipco", &ispe_box(3000, 2000));
    let iprp2 = box_(b"iprp", &ipco2);
    meta_children2.extend_from_slice(&iprp2);
    let mut file = meta_box(&meta_children2);
    assert_eq!(file.len(), off);
    file.extend_from_slice(&grid_payload);

    let geo = heif_geometry(&file).expect("should parse");
    assert_eq!(geo.grids.len(), 1);
    assert_eq!((geo.grids[0].rows, geo.grids[0].cols), (4, 5));
    assert_eq!(geo.grids[0].out_w, 12000);
    assert_eq!(geo.grids[0].out_h, 9000);
    assert_eq!((geo.max_ispe_w, geo.max_ispe_h), (3000, 2000));
    // Canvas 3000x5 x 2000x4 = 15000x8000 → rejected (side cap first).
    assert!(!geometry_within_limits(&file, MAX_SIDE, PIXEL_BUDGET));
}

#[test]
fn in_limits_grid_passes_the_guard() {
    // 2x2 grid of 500x250 tiles → canvas 1000x500, in-limits.
    let grid_payload = grid_payload(1, 1, 2000, 1000); // rows=2, cols=2
    let mut meta_children = Vec::new();
    meta_children.extend_from_slice(&iinf(&[infe2(1, b"hvc1"), infe2(100, b"grid")]));
    meta_children.extend_from_slice(&iloc_v2(&[(100, 0, grid_payload.len() as u32)]));
    let ipco = box_(b"ipco", &ispe_box(500, 250));
    let iprp = box_(b"iprp", &ipco);
    meta_children.extend_from_slice(&iprp);
    // Determine the grid payload's absolute offset (appended after meta).
    let off = meta_box(&meta_children).len();
    // Rebuild with the correct extent offset.
    let mut meta_children2 = Vec::new();
    meta_children2.extend_from_slice(&iinf(&[infe2(1, b"hvc1"), infe2(100, b"grid")]));
    meta_children2.extend_from_slice(&iloc_v2(&[(100, off as u32, grid_payload.len() as u32)]));
    let ipco2 = box_(b"ipco", &ispe_box(500, 250));
    let iprp2 = box_(b"iprp", &ipco2);
    meta_children2.extend_from_slice(&iprp2);
    let mut file = meta_box(&meta_children2);
    assert_eq!(file.len(), off);
    file.extend_from_slice(&grid_payload);

    assert!(geometry_within_limits(&file, MAX_SIDE, PIXEL_BUDGET));
}

#[test]
fn non_zero_grid_version_is_rejected() {
    // A grid payload with version != 0 must be rejected, not misparsed — the
    // parser only understands the version-0 layout `heif-oxide` decodes.
    let mut p = vec![1, 1, 1, 1]; // version=1, flags=1, rows, cols
    p.extend_from_slice(&2000u32.to_be_bytes());
    p.extend_from_slice(&1000u32.to_be_bytes());
    assert!(parse_grid_payload(&p).is_none());
}

#[test]
fn grid_payload_returns_a_borrow_not_a_copy() {
    // The assembled payload must borrow into the file buffer (no copy), so a
    // single grid payload is read once and never re-allocated.
    let payload = grid_payload(1, 1, 2000, 1000);
    let mut children = Vec::new();
    children.extend_from_slice(&iinf(&[infe2(1, b"hvc1"), infe2(100, b"grid")]));
    children.extend_from_slice(&iloc_v2(&[(100, 0, payload.len() as u32)]));
    let ipco = box_(b"ipco", &ispe_box(500, 250));
    let iprp = box_(b"iprp", &ipco);
    children.extend_from_slice(&iprp);
    let off = meta_box(&children).len();
    let mut children2 = Vec::new();
    children2.extend_from_slice(&iinf(&[infe2(1, b"hvc1"), infe2(100, b"grid")]));
    children2.extend_from_slice(&iloc_v2(&[(100, off as u32, payload.len() as u32)]));
    children2.extend_from_slice(&iprp);
    let mut file = meta_box(&children2);
    assert_eq!(file.len(), off);
    file.extend_from_slice(&payload);

    // `heif_geometry` reads the grid payload through `grid_payload`, so this
    // exercises the borrow path end-to-end.
    let geo = heif_geometry(&file).expect("should parse");
    assert_eq!(geo.grids.len(), 1);
    assert_eq!(geo.grids[0].out_w, 2000);
}
