//! gentux-rs rasterizes the Font Awesome "linux" (Tux) glyph into a grayscale
//! coverage mask plus a hole mask and writes them as `assets/tux.bin`, so the
//! tray app itself needs no vector rasterizer or font at runtime.
//!
//! This is a port of `tools/gentux/main.go`; the Go tool is the behavioural
//! reference. The only intentional difference is the rasterizer:
//! `golang.org/x/image/vector` there, `tiny-skia` here, so anti-aliased edge
//! pixels may differ by a few units.
//!
//! Output format (`assets/tux.bin`): u16 LE width, u16 LE height, then
//! `w*h` coverage bytes, then `w*h` hole bytes (255 = enclosed transparent
//! area such as the belly or face).
//!
//! ```text
//! cd tools/gentux-rs && cargo run --release
//! ```
//!
//! Paths are resolved against `CARGO_MANIFEST_DIR`, so it works from any cwd.

use std::path::Path;

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// Glyph height in pixels before cropping; width follows the viewBox aspect.
const MASK_H: usize = 128;
/// Dilation radius in mask pixels.
const STROKE_GROW: i32 = 3;
/// Margin around the rasterized glyph so the dilation has room.
const PAD: usize = 8;
const VIEW_BOX_W: f64 = 448.0;
const VIEW_BOX_H: f64 = 512.0;
/// Coverage at or above this counts as solid for hole detection.
const SOLID: u8 = 128;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let svg_path = root.join("linux.svg");
    let svg = std::fs::read_to_string(&svg_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", svg_path.display()));
    let d = path_data(&svg).expect("linux.svg: no d=\"...\" attribute");

    // Rasterize onto a canvas with a margin so the dilation below has room.
    let mask_w = (MASK_H as f64 * VIEW_BOX_W / VIEW_BOX_H).round() as usize;
    let (cw, ch) = (mask_w + 2 * PAD, MASK_H + 2 * PAD);
    let scale = MASK_H as f64 / VIEW_BOX_H;
    let full = rasterize(d, scale, PAD as f64, cw, ch);

    // Holes = transparent pixels not reachable from the border (belly, face).
    let mut full_holes = find_holes(&full, cw, ch);
    // Thicken the strokes a little: the glyph's outline is ~1 px at 24 px tray
    // size, which looks wispy next to the taskbar's bold text.
    let full_mask = dilate(&full, cw, ch, STROKE_GROW);
    for (hole, &m) in full_holes.iter_mut().zip(&full_mask) {
        if m >= SOLID {
            *hole = 0;
        }
    }

    // Crop to the glyph's bounding box so the icon uses its whole height.
    let (x0, y0, x1, y1) = bounding_box(&full_mask, cw, ch).expect("mask is empty");
    let (w, h) = (x1 - x0, y1 - y0);
    let mut mask = Vec::with_capacity(w * h);
    let mut holes = Vec::with_capacity(w * h);
    for y in y0..y1 {
        let src = y * cw + x0;
        mask.extend_from_slice(&full_mask[src..src + w]);
        holes.extend_from_slice(&full_holes[src..src + w]);
    }

    let mut out = Vec::with_capacity(4 + 2 * w * h);
    out.extend_from_slice(&u16::try_from(w).expect("width fits u16").to_le_bytes());
    out.extend_from_slice(&u16::try_from(h).expect("height fits u16").to_le_bytes());
    out.extend_from_slice(&mask);
    out.extend_from_slice(&holes);

    let assets = root.join("..").join("..").join("assets");
    std::fs::create_dir_all(&assets).unwrap_or_else(|e| panic!("create {}: {e}", assets.display()));
    let out_path = assets.join("tux.bin");
    std::fs::write(&out_path, &out).unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
    println!("wrote {} ({w}x{h})", out_path.display());
}

/// Returns the contents of the first `d="..."` attribute, like the Go tool's
/// `regexp.MustCompile(`d="([^"]*)"`)`.
fn path_data(svg: &str) -> Option<&str> {
    let start = svg.find("d=\"")? + 3;
    let len = svg[start..].find('"')?;
    Some(&svg[start..start + len])
}

/// Fills the SVG path onto a `w`x`h` transparent canvas and returns the alpha
/// channel (0..=255) as the coverage mask.
fn rasterize(d: &str, k: f64, off: f64, w: usize, h: usize) -> Vec<u8> {
    let path = walk_path(d, k, off);
    let mut pixmap = Pixmap::new(w as u32, h as u32).expect("canvas size");
    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 255);
    paint.anti_alias = true;
    // Winding matches x/image/vector, which accumulates signed area and clamps.
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    pixmap.pixels().iter().map(|p| p.alpha()).collect()
}

/// Grows coverage by taking the max over a disc of radius `r`.
fn dilate(mask: &[u8], w: usize, h: usize, r: i32) -> Vec<u8> {
    let mut out = vec![0u8; mask.len()];
    for y in 0..h {
        for x in 0..w {
            let mut m = 0u8;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    let (xx, yy) = (x as i64 + dx as i64, y as i64 + dy as i64);
                    if xx < 0 || yy < 0 || xx >= w as i64 || yy >= h as i64 {
                        continue;
                    }
                    m = m.max(mask[yy as usize * w + xx as usize]);
                }
            }
            out[y * w + x] = m;
        }
    }
    out
}

/// Flood-fills "outside" from the border through low-coverage pixels; whatever
/// low-coverage pixels remain are enclosed holes (255 = hole).
fn find_holes(mask: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut outside = vec![false; w * h];
    let mut stack: Vec<usize> = Vec::with_capacity(w * h);
    let push = |outside: &mut Vec<bool>, stack: &mut Vec<usize>, x: i64, y: i64| {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return;
        }
        let i = y as usize * w + x as usize;
        if outside[i] || mask[i] >= SOLID {
            return;
        }
        outside[i] = true;
        stack.push(i);
    };
    for x in 0..w as i64 {
        push(&mut outside, &mut stack, x, 0);
        push(&mut outside, &mut stack, x, h as i64 - 1);
    }
    for y in 0..h as i64 {
        push(&mut outside, &mut stack, 0, y);
        push(&mut outside, &mut stack, w as i64 - 1, y);
    }
    while let Some(i) = stack.pop() {
        let (x, y) = ((i % w) as i64, (i / w) as i64);
        push(&mut outside, &mut stack, x - 1, y);
        push(&mut outside, &mut stack, x + 1, y);
        push(&mut outside, &mut stack, x, y - 1);
        push(&mut outside, &mut stack, x, y + 1);
    }
    mask.iter()
        .zip(&outside)
        .map(|(&m, &out)| if !out && m < SOLID { 255 } else { 0 })
        .collect()
}

/// Bounding box `(x0, y0, x1, y1)` (max exclusive) of all non-zero pixels.
fn bounding_box(mask: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut bb: Option<(usize, usize, usize, usize)> = None;
    for y in 0..h {
        for x in 0..w {
            if mask[y * w + x] == 0 {
                continue;
            }
            bb = Some(match bb {
                None => (x, y, x + 1, y + 1),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)),
            });
        }
    }
    bb
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Cmd(u8),
    Num(f64),
}

const COMMANDS: &[u8] = b"MmLlHhVvCcSsZz";

/// Splits SVG path data into command letters and numbers, mirroring the Go
/// regexp `[MmLlHhVvCcSsZz]|-?\d*\.?\d+(?:e-?\d+)?` with `FindAllString`
/// semantics: anything that matches neither alternative is skipped.
fn tokenize(d: &str) -> Vec<Tok> {
    let b = d.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if COMMANDS.contains(&b[i]) {
            toks.push(Tok::Cmd(b[i]));
            i += 1;
        } else if let Some(end) = number_end(b, i) {
            let v = d[i..end]
                .parse::<f64>()
                .unwrap_or_else(|e| panic!("bad number {:?}: {e}", &d[i..end]));
            toks.push(Tok::Num(v));
            i = end;
        } else {
            i += 1;
        }
    }
    toks
}

/// Returns the end of the number matching `-?\d*\.?\d+(?:e-?\d+)?` at `start`,
/// or `None` when nothing matches there.
fn number_end(b: &[u8], start: usize) -> Option<usize> {
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let mut i = start;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while digit(i) {
        i += 1;
    }
    let has_int = i > int_start;
    if b.get(i) == Some(&b'.') && digit(i + 1) {
        i += 1;
        while digit(i) {
            i += 1;
        }
    } else if !has_int {
        return None;
    }
    if b.get(i) == Some(&b'e') {
        let mut j = i + 1;
        if b.get(j) == Some(&b'-') {
            j += 1;
        }
        if digit(j) {
            while digit(j) {
                j += 1;
            }
            i = j;
        }
    }
    Some(i)
}

/// Feeds an SVG path (M/m, L/l, H/h, V/v, C/c, S/s, Z/z) into a path builder,
/// scaling by `k` and offsetting by `off` on both axes.
fn walk_path(d: &str, k: f64, off: f64) -> tiny_skia::Path {
    let toks = tokenize(d);
    let mut pb = PathBuilder::new();
    let mut cmd = 0u8;
    // Current point, subpath start, last control point (user units).
    let (mut cx, mut cy, mut sx, mut sy, mut lcx, mut lcy) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0);
    // The Go tool silently reads a command letter as 0 here; a well-formed path
    // never hits that case, so fail loudly instead.
    let num = |i: &mut usize| -> f64 {
        match toks.get(*i) {
            Some(Tok::Num(v)) => {
                *i += 1;
                *v
            }
            other => panic!("expected number at token {i}, got {other:?}"),
        }
    };
    let px = |v: f64| (v * k + off) as f32;

    let mut i = 0;
    while i < toks.len() {
        if let Tok::Cmd(c) = toks[i] {
            cmd = c;
            i += 1;
            if cmd == b'Z' || cmd == b'z' {
                pb.close();
                cx = sx;
                cy = sy;
                lcx = cx;
                lcy = cy;
                continue;
            }
        }
        let rel = cmd >= b'a';
        match cmd {
            b'M' | b'm' => {
                let (mut x, mut y) = (num(&mut i), num(&mut i));
                if rel {
                    x += cx;
                    y += cy;
                }
                pb.move_to(px(x), px(y));
                cx = x;
                cy = y;
                sx = x;
                sy = y;
                // Subsequent pairs are implicit LineTo.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' | b'l' => {
                let (mut x, mut y) = (num(&mut i), num(&mut i));
                if rel {
                    x += cx;
                    y += cy;
                }
                pb.line_to(px(x), px(y));
                cx = x;
                cy = y;
            }
            b'H' | b'h' => {
                let mut x = num(&mut i);
                if rel {
                    x += cx;
                }
                pb.line_to(px(x), px(cy));
                cx = x;
            }
            b'V' | b'v' => {
                let mut y = num(&mut i);
                if rel {
                    y += cy;
                }
                pb.line_to(px(cx), px(y));
                cy = y;
            }
            b'C' | b'c' => {
                let (mut x1, mut y1) = (num(&mut i), num(&mut i));
                let (mut x2, mut y2) = (num(&mut i), num(&mut i));
                let (mut x, mut y) = (num(&mut i), num(&mut i));
                if rel {
                    x1 += cx;
                    y1 += cy;
                    x2 += cx;
                    y2 += cy;
                    x += cx;
                    y += cy;
                }
                pb.cubic_to(px(x1), px(y1), px(x2), px(y2), px(x), px(y));
                lcx = x2;
                lcy = y2;
                cx = x;
                cy = y;
            }
            b'S' | b's' => {
                let (mut x2, mut y2) = (num(&mut i), num(&mut i));
                let (mut x, mut y) = (num(&mut i), num(&mut i));
                if rel {
                    x2 += cx;
                    y2 += cy;
                    x += cx;
                    y += cy;
                }
                // Reflection of the previous control point.
                let (x1, y1) = (2.0 * cx - lcx, 2.0 * cy - lcy);
                pb.cubic_to(px(x1), px(y1), px(x2), px(y2), px(x), px(y));
                lcx = x2;
                lcy = y2;
                cx = x;
                cy = y;
            }
            _ => panic!("unsupported path command {:?}", cmd as char),
        }
        if !matches!(cmd, b'C' | b'c' | b'S' | b's') {
            lcx = cx;
            lcy = cy;
        }
    }
    pb.finish().expect("path has no segments")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_mirrors_go_regexp() {
        let toks = tokenize("M220.8 123.3c1 .5-.4.2 1e2,3z 5.");
        assert_eq!(
            toks,
            vec![
                Tok::Cmd(b'M'),
                Tok::Num(220.8),
                Tok::Num(123.3),
                Tok::Cmd(b'c'),
                Tok::Num(1.0),
                Tok::Num(0.5),
                Tok::Num(-0.4),
                Tok::Num(0.2),
                Tok::Num(100.0),
                Tok::Num(3.0),
                Tok::Cmd(b'z'),
                Tok::Num(5.0),
            ]
        );
    }

    #[test]
    fn path_data_extracts_first_d_attribute() {
        assert_eq!(path_data(r#"<svg><path d="M1 2z"/></svg>"#), Some("M1 2z"));
        assert_eq!(path_data("<svg/>"), None);
    }

    #[test]
    fn holes_are_enclosed_low_coverage_pixels() {
        // 5x5 ring of solid pixels around one transparent centre pixel.
        let mut m = vec![0u8; 25];
        for y in 1..4 {
            for x in 1..4 {
                m[y * 5 + x] = 255;
            }
        }
        m[2 * 5 + 2] = 0;
        let holes = find_holes(&m, 5, 5);
        assert_eq!(holes.iter().filter(|&&h| h == 255).count(), 1);
        assert_eq!(holes[2 * 5 + 2], 255);
    }

    #[test]
    fn dilate_uses_disc_neighbourhood() {
        let mut m = vec![0u8; 49];
        m[3 * 7 + 3] = 200;
        let d = dilate(&m, 7, 7, 3);
        assert_eq!(d[3 * 7 + 0], 200); // distance 3 on axis is inside
        assert_eq!(d[0], 0); // corner (3,3 away) is outside r*r=9
        assert_eq!(d[1 * 7 + 1], 200); // (2,2): 8 <= 9
    }
}
