//! Minimal HEIF/ISOBMFF geometry parser used by the HEIC pre-decode guard.
//!
//! `heif-oxide` exposes no decoder limit and allocates its YUV/RGB/RGBA
//! buffers from *file-declared* geometry, so an untrusted HEIC could drive a
//! huge allocation before we get a chance to resize. This module reads just
//! enough of the container to bound that geometry without decoding any pixels:
//!
//!   - every `ispe` (ImageSpatialExtentsProperty) extent — the per-item frame
//!     size a single coded image or grid tile is decoded from; and
//!   - every `grid` derived item's canvas — the declared tile `rows`/`cols`
//!     (from the grid item payload, located via `iinf`/`iloc`) multiplied by
//!     the tile extent, which is the amplification vector a per-item cap alone
//!     does not close.
//!
//! All reads are bounds-checked and return `None` on any malformation — and a
//! malformed or un-GUARDABLE container is rejected rather than decoded, the
//! safe default (a valid HEIF always carries `ispe` geometry, and the grid
//! payload is the authoritative canvas size).
//!
//! Box traversal is deliberately careful about **full boxes**: `meta` and
//! `iinf` carry a 4-byte `version/flags` prefix (and `iinf` an item count)
//! before their child boxes, so a naive "recurse into every box's content"
//! walk would misparse the full box header as a child box header. Only
//! `meta`/`iinf` are full-box containers in the set we descend into; the
//! rest (`iprp`/`ipco`/`infe`) are either plain containers or leaf boxes we
//! read directly.

use std::collections::HashMap;

/// A `grid` item's declared tile geometry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GridGeometry {
    pub rows: u32,
    pub cols: u32,
    pub out_w: u32,
    pub out_h: u32,
}

/// The allocation-driving geometry of a HEIF container.
#[derive(Debug)]
pub(crate) struct HeifGeometry {
    /// Largest `ispe` extent in the file (bounds every single image / tile).
    pub max_ispe_w: u32,
    pub max_ispe_h: u32,
    /// One entry per `grid` derived item.
    pub grids: Vec<GridGeometry>,
}

/// Where an item's payload bytes live (single-extent form, as HEIF images use).
#[derive(Clone, Copy)]
struct ItemLoc {
    construction_method: u8,
    data_reference_index: u16,
    base_offset: u64,
    extent_offset: u64,
    extent_length: u64,
}

/// Parse the geometry `heif-oxide` will allocate from. `None` when the file is
/// not a GUARDABLE HEIF still image (no `meta`, no `ispe`, or a `grid` whose
/// declared canvas we cannot read).
pub(crate) fn heif_geometry(data: &[u8]) -> Option<HeifGeometry> {
    let meta_children = find_meta_children(data)?;

    let mut max_ispe_w = 0u32;
    let mut max_ispe_h = 0u32;
    let mut found_ispe = false;
    let mut grid_ids: Vec<u32> = Vec::new();
    let mut locations: HashMap<u32, ItemLoc> = HashMap::new();
    let mut idat: Option<&[u8]> = None;
    let mut invalid = false;

    for_each_box(meta_children, |btype, content| {
        match &btype {
            // `iprp` (plain) → `ipco` (plain) → `ispe`* (full box leaf).
            b"iprp" => for_each_box(content, |t2, c2| {
                if t2 == *b"ipco" {
                    for_each_box(c2, |t3, c3| {
                        // `ispe` (FullBox): version/flags(4) + w(4) + h(4).
                        if t3 == *b"ispe"
                            && let (Some(w), Some(h)) = (u32_at(c3, 4), u32_at(c3, 8))
                        {
                            max_ispe_w = max_ispe_w.max(w);
                            max_ispe_h = max_ispe_h.max(h);
                            found_ispe = true;
                        }
                        true
                    });
                }
                true
            }),
            b"iinf" => collect_grid_items(content, &mut grid_ids),
            b"iloc" => parse_iloc(content, &mut locations),
            b"idat" => idat = Some(content),
            _ => {}
        }
        true
    });

    // A valid still image must carry ispe geometry (every coded item / grid
    // tile does); without it we cannot prove the size, so we reject.
    if !found_ispe {
        return None;
    }

    // Read every grid item's payload and derive its canvas. If a grid payload
    // cannot be located or parsed (unsupported construction, external file,
    // corrupt), we cannot bound it — reject rather than decode.
    let mut grids = Vec::new();
    for &gid in &grid_ids {
        match grid_payload(&locations, idat, data, gid).and_then(parse_grid_payload) {
            Some(g) => grids.push(g),
            None => {
                invalid = true;
                break;
            }
        }
    }
    if invalid {
        return None;
    }

    Some(HeifGeometry {
        max_ispe_w,
        max_ispe_h,
        grids,
    })
}

/// Locate the `meta` box at the top level and return its child region
/// (skipping the 4-byte full-box `version/flags` header).
fn find_meta_children(data: &[u8]) -> Option<&[u8]> {
    let mut meta_children = None;
    for_each_box(data, |btype, content| {
        if btype == *b"meta" {
            meta_children = content.get(4..); // skip version/flags
            return false; // stop after the first meta
        }
        true
    });
    meta_children
}

/// Collect `grid` item ids from `iinf`'s `infe` entries.
fn collect_grid_items(content: &[u8], grid_ids: &mut Vec<u32>) {
    // iinf (full box): version/flags then an entry_count (u16 in v0, u32 else)
    // before its `infe` children.
    let version = match content.first() {
        Some(v) => *v,
        None => return,
    };
    let child_start = 4 + if version == 0 { 2 } else { 4 };
    let Some(region) = content.get(child_start..) else {
        return;
    };
    for_each_box(region, |btype, child| {
        if btype == *b"infe"
            && let Some((item_id, item_type)) = parse_infe(child)
            && item_type == *b"grid"
        {
            grid_ids.push(item_id);
        }
        true
    });
}

/// Parse one `infe` (full box) entry → `(item_id, item_type)`.
/// Versions 0/1 predate HEIF item types and are skipped.
fn parse_infe(content: &[u8]) -> Option<(u32, [u8; 4])> {
    let version = *content.first()?;
    if version < 2 {
        return None;
    }
    let (item_id, type_off) = if version == 2 {
        (u16_at(content, 4)? as u32, 8)
    } else {
        (u32_at(content, 4)?, 10)
    };
    let item_type: [u8; 4] = content.get(type_off..type_off + 4)?.try_into().ok()?;
    Some((item_id, item_type))
}

/// Parse `iloc` (full box) into the single-extent location per item.
fn parse_iloc(content: &[u8], locations: &mut HashMap<u32, ItemLoc>) {
    let mut r = ByteCursor::new(content);
    let Some(version) = r.u8() else {
        return;
    };
    if version > 2 {
        return;
    }
    r.skip(3); // flags
    let Some(header) = r.u8() else {
        return;
    };
    let offset_size = header >> 4;
    let length_size = header & 0xF;
    let Some(header2) = r.u8() else {
        return;
    };
    let base_offset_size = header2 >> 4;
    let index_size = if version >= 1 { header2 & 0xF } else { 0 };
    let Some(item_count) = (if version < 2 {
        r.u16().map(|v| v as u32)
    } else {
        r.u32()
    }) else {
        return;
    };

    for _ in 0..item_count {
        let Some(item_id) = (if version < 2 {
            r.u16().map(|v| v as u32)
        } else {
            r.u32()
        }) else {
            return;
        };
        let construction_method = if version >= 1 {
            let Some(v) = r.u16() else { return };
            (v & 0xF) as u8
        } else {
            0
        };
        let Some(data_reference_index) = r.u16() else {
            return;
        };
        let Some(base_offset) = r.uint(base_offset_size) else {
            return;
        };
        let Some(extent_count) = r.u16() else {
            return;
        };
        // HEIF still images use a single extent; if more are present we keep
        // the first (matching heif-oxide's concatenation convention) but only
        // when there is exactly one — a multi-extent grid is unusual, so we
        // leave it empty (unlocatable) and let the caller reject.
        let mut extent = None;
        for _ in 0..extent_count {
            if index_size > 0 {
                // extent_index — only used by construction_method 2 (rejected).
                let _ = r.uint(index_size);
            }
            let Some(extent_offset) = r.uint(offset_size) else {
                return;
            };
            let Some(extent_length) = r.uint(length_size) else {
                return;
            };
            if extent.is_none() {
                extent = Some((extent_offset, extent_length));
            }
        }
        if let Some((extent_offset, extent_length)) = extent
            && extent_count == 1
        {
            locations.insert(
                item_id,
                ItemLoc {
                    construction_method,
                    data_reference_index,
                    base_offset,
                    extent_offset,
                    extent_length,
                },
            );
        }
    }
}

/// Assemble a single-extent item payload (construction_method 0 or 1) as a
/// borrow into the file (or `idat`) buffer — no copy, `heif-oxide`-style
/// single-extent read.
fn grid_payload<'a>(
    locations: &HashMap<u32, ItemLoc>,
    idat: Option<&'a [u8]>,
    data: &'a [u8],
    item_id: u32,
) -> Option<&'a [u8]> {
    let loc = locations.get(&item_id)?;
    if loc.data_reference_index != 0 {
        return None; // external file — unsupported, reject
    }
    let source = match loc.construction_method {
        0 => data,
        1 => idat?,
        _ => return None, // construction_method 2 (into another item) — reject
    };
    let start = loc.base_offset.checked_add(loc.extent_offset)? as usize;
    let end = if loc.extent_length == 0 {
        source.len()
    } else {
        start.checked_add(loc.extent_length as usize)?
    };
    if start > end || end > source.len() {
        return None;
    }
    Some(&source[start..end])
}

/// Parse a `grid` item payload: version(1) flags(1) rows(1) cols(1) then the
/// output size (u32×2 when `flags&1`, else u16×2). `rows`/`cols` are stored as
/// value+1. Version must be 0 (the only `grid` layout `heif-oxide` decodes);
/// any other version is rejected rather than misparsed. Rejects a zero output
/// size, which heif-oxide also does.
fn parse_grid_payload(p: &[u8]) -> Option<GridGeometry> {
    if p.len() < 8 || p[0] != 0 {
        return None;
    }
    let flags = p[1];
    let rows = p[2] as u32 + 1;
    let cols = p[3] as u32 + 1;
    let (out_w, out_h) = if flags & 1 != 0 {
        (u32_at(p, 4)?, u32_at(p, 8)?)
    } else {
        (u16_at(p, 4)? as u32, u16_at(p, 6)? as u32)
    };
    if out_w == 0 || out_h == 0 {
        return None;
    }
    Some(GridGeometry {
        rows,
        cols,
        out_w,
        out_h,
    })
}

/// True when the declared geometry stays within the decompression-bomb guard:
/// every single-item extent, every grid's declared output, and every grid's
/// canvas (tile extent × rows/cols) are all within [`MAX_SOURCE_DIMENSION`]
/// per side and within the pixel budget.
pub(crate) fn geometry_within_limits(data: &[u8], max_side: u32, max_pixels: u64) -> bool {
    let Some(geo) = heif_geometry(data) else {
        return false;
    };
    let mut max_w = geo.max_ispe_w;
    let mut max_h = geo.max_ispe_h;
    for g in &geo.grids {
        // The decoded canvas is the tile extent × the grid's rows/cols. The
        // largest ispe in the file bounds every tile (conservative — exact
        // for the common single-grid file).
        let canvas_w = geo.max_ispe_w.saturating_mul(g.cols);
        let canvas_h = geo.max_ispe_h.saturating_mul(g.rows);
        max_w = max_w.max(canvas_w).max(g.out_w);
        max_h = max_h.max(canvas_h).max(g.out_h);
    }
    max_w <= max_side && max_h <= max_side && (max_w as u64) * (max_h as u64) <= max_pixels
}

/// Iterate sibling boxes in `bytes`, calling `f(box_type, content)` for each.
/// Return `false` from `f` to stop early. `content` excludes the box header
/// and borrows from `bytes`, so `f` may store it (e.g. to find `idat`).
/// Bounds-checked and total (never panics: malformed sizes just stop).
fn for_each_box<'a>(bytes: &'a [u8], mut f: impl FnMut([u8; 4], &'a [u8]) -> bool) {
    let mut off = 0usize;
    while off + 8 <= bytes.len() {
        let Some(size) = u32_at(bytes, off) else {
            return;
        };
        let size = size as usize;
        let btype: [u8; 4] = match bytes.get(off + 4..off + 8) {
            Some(s) => s.try_into().unwrap_or([0; 4]),
            None => return,
        };
        let header = if size == 1 { 16 } else { 8 };
        if off + header > bytes.len() {
            return;
        }
        let content_start = off + header;
        let next = match size {
            0 => bytes.len(),
            1 => match u64_at(bytes, off + 8) {
                Some(large) => off.saturating_add(large as usize),
                None => return,
            },
            n => off + n,
        };
        let content_end = next.min(bytes.len());
        if content_end < content_start {
            return;
        }
        if !f(btype, &bytes[content_start..content_end]) {
            return;
        }
        if next <= off {
            return;
        }
        off = next;
    }
}

/// A minimal bounds-checked big-endian cursor (mirrors `heif-oxide`'s Reader).
struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        ByteCursor { data, pos: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = u16_at(self.data, self.pos)?;
        self.pos += 2;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32_at(self.data, self.pos)?;
        self.pos += 4;
        Some(v)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos = self.pos.checked_add(n)?;
        Some(())
    }
    /// Read an unsigned integer of width 0/4/8 bytes (the `iloc` `*_size`
    /// field widths); width 0 is defined by ISOBMFF to mean the value 0.
    fn uint(&mut self, width: u8) -> Option<u64> {
        match width {
            0 => Some(0),
            4 => self.u32().map(|v| v as u64),
            8 => {
                let v = u64_at(self.data, self.pos)?;
                self.pos += 8;
                Some(v)
            }
            _ => None,
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    let s = b.get(i..i + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    let s = b.get(i..i + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    let s = b.get(i..i + 8)?;
    Some(u64::from_be_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

#[cfg(test)]
mod tests;
