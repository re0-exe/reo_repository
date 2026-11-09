//! Drawing the mouse pointer into a captured frame. Desktop Duplication
//! delivers the pointer separately from the desktop image, so without this
//! the tablet would show a desktop with no cursor.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    /// 1 bpp AND mask followed by a 1 bpp XOR mask.
    Monochrome,
    /// 32 bpp BGRA with straight alpha.
    Color,
    /// 32 bpp BGRA where alpha 0 means "replace" and 0xFF means "XOR".
    MaskedColor,
}

impl CursorKind {
    /// Maps `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`.
    pub fn from_dxgi(ty: u32) -> Option<CursorKind> {
        match ty {
            1 => Some(CursorKind::Monochrome),
            2 => Some(CursorKind::Color),
            4 => Some(CursorKind::MaskedColor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub kind: CursorKind,
    pub width: u32,
    /// Visible height. For `Monochrome` the data holds twice as many rows.
    pub height: u32,
    /// Bytes per row of `data`.
    pub pitch: u32,
    pub data: Vec<u8>,
}

impl CursorShape {
    /// Whether `data` is large enough for the declared geometry; shapes that
    /// are not are ignored rather than read out of bounds.
    pub fn is_valid(&self) -> bool {
        let (rows, min_pitch) = match self.kind {
            CursorKind::Monochrome => (self.height as usize * 2, (self.width as usize).div_ceil(8)),
            _ => (self.height as usize, self.width as usize * 4),
        };
        self.width > 0 && self.height > 0 && self.pitch as usize >= min_pitch && self.data.len() >= rows * self.pitch as usize
    }
}

/// The part of a cursor that falls inside a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clip {
    /// Top-left of the region in frame coordinates.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Offset of the region inside the cursor image.
    pub src_x: u32,
    pub src_y: u32,
}

/// Intersects a cursor at (`pos_x`, `pos_y`) with the frame. `max` bounds the
/// region to the size of the scratch surface used for blending.
pub fn clip(pos_x: i32, pos_y: i32, shape_w: u32, shape_h: u32, frame_w: u32, frame_h: u32, max: u32) -> Option<Clip> {
    let x0 = pos_x.max(0) as i64;
    let y0 = pos_y.max(0) as i64;
    let x1 = (pos_x as i64 + shape_w as i64).min(frame_w as i64).min(x0 + max as i64);
    let y1 = (pos_y as i64 + shape_h as i64).min(frame_h as i64).min(y0 + max as i64);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Clip {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
        src_x: (x0 - pos_x as i64) as u32,
        src_y: (y0 - pos_y as i64) as u32,
    })
}

/// Blends the cursor into `dst`, a BGRA region of `region_w` x `region_h`
/// pixels with `dst_stride` bytes per row, whose origin corresponds to
/// (`src_x`, `src_y`) in the cursor image.
pub fn blend(dst: &mut [u8], dst_stride: usize, region_w: u32, region_h: u32, shape: &CursorShape, src_x: u32, src_y: u32) {
    if !shape.is_valid() {
        return;
    }
    let pitch = shape.pitch as usize;
    for row in 0..region_h {
        let sy = src_y + row;
        if sy >= shape.height {
            break;
        }
        for col in 0..region_w {
            let sx = src_x + col;
            if sx >= shape.width {
                break;
            }
            let d = row as usize * dst_stride + col as usize * 4;
            let Some(px) = dst.get_mut(d..d + 3) else { return };
            match shape.kind {
                CursorKind::Color => {
                    let s = sy as usize * pitch + sx as usize * 4;
                    let a = shape.data[s + 3] as u32;
                    for c in 0..3 {
                        px[c] = ((shape.data[s + c] as u32 * a + px[c] as u32 * (255 - a) + 127) / 255) as u8;
                    }
                }
                CursorKind::MaskedColor => {
                    let s = sy as usize * pitch + sx as usize * 4;
                    let xor = shape.data[s + 3] != 0;
                    for c in 0..3 {
                        px[c] = if xor { px[c] ^ shape.data[s + c] } else { shape.data[s + c] };
                    }
                }
                CursorKind::Monochrome => {
                    let bit = |r: u32| shape.data[r as usize * pitch + (sx / 8) as usize] & (0x80 >> (sx % 8)) != 0;
                    let (and, xor) = (bit(sy), bit(sy + shape.height));
                    for c in px.iter_mut() {
                        *c = match (and, xor) {
                            (false, false) => 0,
                            (false, true) => 255,
                            (true, false) => *c,
                            (true, true) => !*c,
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(pixels: &[[u8; 4]], w: u32, h: u32, kind: CursorKind) -> CursorShape {
        CursorShape { kind, width: w, height: h, pitch: w * 4, data: pixels.concat() }
    }

    #[test]
    fn color_cursor_alpha_blends() {
        // opaque white, transparent, half red
        let shape = color(&[[255, 255, 255, 255], [9, 9, 9, 0], [0, 0, 255, 128]], 3, 1, CursorKind::Color);
        let mut dst = vec![10, 20, 30, 255, 10, 20, 30, 255, 100, 100, 100, 255];
        blend(&mut dst, 12, 3, 1, &shape, 0, 0);
        assert_eq!(&dst[0..4], [255, 255, 255, 255]);
        assert_eq!(&dst[4..8], [10, 20, 30, 255]);
        assert_eq!(&dst[8..12], [50, 50, 178, 255]);
    }

    #[test]
    fn masked_color_replaces_or_xors() {
        let shape = color(&[[1, 2, 3, 0], [0xFF, 0xFF, 0xFF, 0xFF]], 2, 1, CursorKind::MaskedColor);
        let mut dst = vec![50, 60, 70, 255, 0x0F, 0xF0, 0x00, 255];
        blend(&mut dst, 8, 2, 1, &shape, 0, 0);
        assert_eq!(dst, [1, 2, 3, 255, 0xF0, 0x0F, 0xFF, 255]);
    }

    #[test]
    fn monochrome_applies_and_then_xor_mask() {
        // 4 px wide, 1 row. AND = 0011, XOR = 0101 -> black, white, keep, invert
        let shape = CursorShape { kind: CursorKind::Monochrome, width: 4, height: 1, pitch: 1, data: vec![0b0011_0000, 0b0101_0000] };
        let mut dst = [7u8, 7, 7, 255].repeat(4);
        blend(&mut dst, 16, 4, 1, &shape, 0, 0);
        assert_eq!(dst, [0, 0, 0, 255, 255, 255, 255, 255, 7, 7, 7, 255, 248, 248, 248, 255]);
    }

    #[test]
    fn blend_honours_source_offset_and_stride() {
        let shape = color(&[[1, 1, 1, 255], [2, 2, 2, 255], [3, 3, 3, 255], [4, 4, 4, 255]], 2, 2, CursorKind::Color);
        // 1x1 region showing the cursor's bottom-right pixel; stride wider than the region.
        let mut dst = vec![0u8; 8];
        blend(&mut dst, 8, 1, 1, &shape, 1, 1);
        assert_eq!(dst, [4, 4, 4, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn truncated_shape_is_ignored() {
        let shape = CursorShape { kind: CursorKind::Color, width: 4, height: 4, pitch: 16, data: vec![255; 10] };
        let mut dst = vec![5u8; 64];
        blend(&mut dst, 16, 4, 4, &shape, 0, 0);
        assert_eq!(dst, vec![5u8; 64]);
    }

    #[test]
    fn clip_inside_frame() {
        assert_eq!(
            clip(10, 20, 32, 32, 2800, 1752, 256),
            Some(Clip { x: 10, y: 20, width: 32, height: 32, src_x: 0, src_y: 0 })
        );
    }

    #[test]
    fn clip_at_edges() {
        assert_eq!(
            clip(-8, -4, 32, 32, 2800, 1752, 256),
            Some(Clip { x: 0, y: 0, width: 24, height: 28, src_x: 8, src_y: 4 })
        );
        assert_eq!(
            clip(2790, 1740, 32, 32, 2800, 1752, 256),
            Some(Clip { x: 2790, y: 1740, width: 10, height: 12, src_x: 0, src_y: 0 })
        );
    }

    #[test]
    fn clip_outside_frame_is_none() {
        assert_eq!(clip(2800, 0, 32, 32, 2800, 1752, 256), None);
        assert_eq!(clip(-32, 0, 32, 32, 2800, 1752, 256), None);
    }

    #[test]
    fn clip_is_bounded_by_scratch_size() {
        let c = clip(0, 0, 512, 512, 2800, 1752, 256).unwrap();
        assert_eq!((c.width, c.height), (256, 256));
    }
}
