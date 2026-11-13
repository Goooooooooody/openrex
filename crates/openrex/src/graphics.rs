//! Images that programs draw with the Kitty graphics protocol.
//!
//! Nothing here speaks the protocol. The server passes a program's graphics
//! commands through in the terminal stream, and its snapshot carries the
//! images already stored, so the local copy of the terminal (Ghostty's
//! engine, in `term.rs`) ends up holding the same images and placements as
//! the server's. This module reads them back out of the engine, turns each
//! image into a texture once, and draws every placement in the view at its
//! cells, in the layer its z-index puts it in.
//!
//! What it can't draw: images placed with Unicode placeholders (`U=1`) and
//! placements relative to another placement (`P=`), because the engine's
//! storage can't say where those are on screen (only its render state's
//! image list can, which ghostty-vt 0.8.0 doesn't wrap), and animation
//! frames past the first. Images a program sends as a file or a temporary
//! file need nothing special: the server reads the file on its machine and
//! rewrites the command to carry the pixels (`t=d`) before it reaches the
//! stream, so the local copy never reads a local file.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ghostty_vt::Terminal;
use ghostty_vt::kitty::graphics::{ImageFormat, PlacementIterator, RustPngDecoder, set_png_decoder};
use gpui::{Bounds, ContentMask, Context, Corners, Pixels, Point, RenderImage, Window, point, px, size};
use serde_json::Value;

use crate::actions::{Outcome, Target};
use crate::pane::Metrics;
use crate::term::Mirror;
use crate::workspace::Workspace;

/// The longest side of a texture. Larger images are scaled down to it, which
/// keeps them inside what any GPU's texture atlas takes.
const MAX_SIDE: u32 = 4096;

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    _ws: &mut Workspace,
    _action: &str,
    _args: &Value,
    _target: &Target,
    _window: &mut Window,
    _cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    None
}

thread_local! {
    static PNG_DECODER: Cell<bool> = const { Cell::new(false) };
}

/// Lets the terminal engine accept PNG images. A program usually sends PNG,
/// and the engine has no decoder of its own: it calls one per thread, the
/// thread that writes to the terminal, so this runs before each copy is fed.
pub(crate) fn install_png_decoder() {
    PNG_DECODER.with(|installed| {
        if !installed.replace(true) {
            if let Err(e) = set_png_decoder(Some(Box::new(RustPngDecoder::new()))) {
                log::warn!("PNG images won't show: {e:?}");
            }
        }
    });
}

/// Where a placement sits relative to the text, from its z-index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Under the cells' background colors.
    BelowBackground,
    /// Over the background colors, under the text.
    BelowText,
    AboveText,
}

impl Layer {
    /// The protocol's rule: a negative z-index goes under the text, and one
    /// below half of `i32::MIN` under non-default backgrounds too.
    pub fn of(z: i32) -> Layer {
        if z < i32::MIN / 2 {
            Layer::BelowBackground
        } else if z < 0 {
            Layer::BelowText
        } else {
            Layer::AboveText
        }
    }
}

/// A rectangle in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One image placement in the view, as the terminal engine reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub image_id: u32,
    /// Changes whenever the image's pixels may have, so it keys the texture.
    pub generation: u64,
    pub z: i32,
    /// The top-left cell, relative to the view. The row is negative when the
    /// image's top has scrolled above the view.
    pub col: i32,
    pub row: i32,
    /// How far the image starts from its top-left cell's corner, in the
    /// terminal's pixels.
    pub x_offset: u32,
    pub y_offset: u32,
    /// The size to draw it at, in the terminal's pixels.
    pub width: u32,
    pub height: u32,
    /// The part of the image to draw, in image pixels.
    pub source: Rect,
}

/// The placements with any part in the view, in drawing order: lowest
/// z-index first.
pub fn placements(term: &Terminal) -> Vec<Placement> {
    let Ok(graphics) = term.kitty_graphics() else { return Vec::new() };
    // Zero means nothing has ever been stored, the usual case.
    if graphics.generation().unwrap_or(0) == 0 {
        return Vec::new();
    }
    let Ok(mut iterator) = PlacementIterator::new() else { return Vec::new() };
    let Ok(mut iteration) = iterator.update(&graphics) else { return Vec::new() };
    let mut out = Vec::new();
    while let Some(p) = iteration.next() {
        let Ok(image_id) = p.image_id() else { continue };
        let Some(image) = graphics.image(image_id) else { continue };
        let Ok(info) = p.placement_render_info(&image, term) else { continue };
        if !info.viewport_visible || info.pixel_width == 0 || info.pixel_height == 0 {
            continue;
        }
        out.push(Placement {
            image_id,
            generation: image.generation().unwrap_or(0),
            z: p.z().unwrap_or(0),
            col: info.viewport_col,
            row: info.viewport_row,
            x_offset: p.x_offset().unwrap_or(0),
            y_offset: p.y_offset().unwrap_or(0),
            width: info.pixel_width,
            height: info.pixel_height,
            source: Rect {
                x: info.source_x,
                y: info.source_y,
                width: info.source_width,
                height: info.source_height,
            },
        });
    }
    // Stable, so placements with the same z-index keep the engine's order.
    out.sort_by_key(|p| p.z);
    out
}

/// The size of the terminal's cells in its own pixels: what the program was
/// told, which is what the placements' pixel sizes are measured in.
pub fn cell_px(term: &Terminal) -> (f32, f32) {
    let (cols, rows) = (term.cols().unwrap_or(0), term.rows().unwrap_or(0));
    let (w, h) = (term.width_px().unwrap_or(0), term.height_px().unwrap_or(0));
    let per = |px: u32, n: u16| if px == 0 || n == 0 { 1.0 } else { px as f32 / n as f32 };
    (per(w, cols), per(h, rows))
}

/// Where a placement is drawn, as (x, y, width, height) from the grid's
/// top-left corner in the window's pixels. The terminal's pixels are scaled
/// to the cells drawn here, so an image covers the same cells it covers on
/// the server even when the server was told a different cell size.
pub fn dest(p: &Placement, term_cell: (f32, f32), cell: (f32, f32)) -> (f32, f32, f32, f32) {
    let sx = cell.0 / term_cell.0.max(1.0);
    let sy = cell.1 / term_cell.1.max(1.0);
    (
        p.col as f32 * cell.0 + p.x_offset as f32 * sx,
        p.row as f32 * cell.1 + p.y_offset as f32 * sy,
        p.width as f32 * sx,
        p.height as f32 * sy,
    )
}

/// An image's pixels, ready for a texture.
#[derive(Clone, Debug, PartialEq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// Four bytes a pixel, blue, green, red, alpha, as gpui's textures take.
    pub bgra: Vec<u8>,
}

/// Converts the `source` part of an image the engine stores (RGB, RGBA,
/// gray, or gray and alpha, `channels` bytes a pixel) to a bitmap no larger
/// than `max_side` on either side. `None` when the pixels haven't all
/// arrived or the part is empty.
pub fn to_bgra(channels: usize, width: u32, height: u32, data: &[u8], source: Rect, max_side: u32) -> Option<Bitmap> {
    if !(1..=4).contains(&channels) || data.len() < width as usize * height as usize * channels {
        return None;
    }
    let x0 = source.x.min(width);
    let y0 = source.y.min(height);
    let sw = source.width.min(width - x0);
    let sh = source.height.min(height - y0);
    if sw == 0 || sh == 0 {
        return None;
    }
    // Scale down, nearest pixel, when the part is too big for a texture.
    let longest = sw.max(sh);
    let (ow, oh) = if longest > max_side.max(1) {
        let m = max_side.max(1) as u64;
        (((sw as u64 * m) / longest as u64).max(1) as u32, ((sh as u64 * m) / longest as u64).max(1) as u32)
    } else {
        (sw, sh)
    };
    let mut bgra = Vec::with_capacity(ow as usize * oh as usize * 4);
    for y in 0..oh {
        let sy = y0 + (y as u64 * sh as u64 / oh as u64) as u32;
        for x in 0..ow {
            let sx = x0 + (x as u64 * sw as u64 / ow as u64) as u32;
            let at = (sy as usize * width as usize + sx as usize) * channels;
            let px = &data[at..at + channels];
            let (r, g, b, a) = match channels {
                1 => (px[0], px[0], px[0], 255),
                2 => (px[0], px[0], px[0], px[1]),
                3 => (px[0], px[1], px[2], 255),
                _ => (px[0], px[1], px[2], px[3]),
            };
            bgra.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(Bitmap { width: ow, height: oh, bgra })
}

/// The bitmap for a placement, read from the engine's storage.
pub fn bitmap(term: &Terminal, p: &Placement) -> Option<Bitmap> {
    let graphics = term.kitty_graphics().ok()?;
    let image = graphics.image(p.image_id)?;
    // The engine decodes PNG and inflates compressed data before storing,
    // so these are the only layouts it holds.
    let channels = match image.format().ok()? {
        ImageFormat::Gray => 1,
        ImageFormat::GrayAlpha => 2,
        ImageFormat::Rgb => 3,
        ImageFormat::Rgba => 4,
        _ => return None,
    };
    to_bgra(channels, image.width().ok()?, image.height().ok()?, image.data().ok()?, p.source, MAX_SIDE)
}

/// What a texture is made from: the image's pixels at one generation, and
/// the part of it drawn.
type Key = (u32, u64, Rect);

/// A pane's image textures, kept between frames so each image is converted
/// and uploaded once.
#[derive(Default)]
pub struct Images {
    textures: HashMap<Key, Arc<RenderImage>>,
}

/// One placement ready to paint.
pub struct Drawn {
    layer: Layer,
    bounds: Bounds<Pixels>,
    image: Arc<RenderImage>,
}

/// The images to paint in one frame of a pane.
#[derive(Default)]
pub struct Layout {
    /// The grid, which images are clipped to.
    clip: Option<Bounds<Pixels>>,
    drawn: Vec<Drawn>,
}

impl Images {
    /// Reads the placements in the view and makes sure each has a texture.
    /// Textures no placement in the view uses are let go.
    pub fn layout(&mut self, mirror: Option<&Mirror>, origin: Point<Pixels>, metrics: &Metrics, window: &mut Window) -> Layout {
        let mut layout = Layout::default();
        let mut used = HashSet::new();
        if let Some(term) = mirror.filter(|m| m.is_live()).map(Mirror::terminal) {
            let placements = placements(term);
            if !placements.is_empty() {
                let (cols, rows) = (term.cols().unwrap_or(0), term.rows().unwrap_or(0));
                layout.clip = Some(Bounds::new(
                    origin,
                    size(metrics.cell_w * cols as f32, metrics.line_h * rows as f32),
                ));
                let term_cell = cell_px(term);
                let cell = (f32::from(metrics.cell_w), f32::from(metrics.line_h));
                for p in &placements {
                    let key = (p.image_id, p.generation, p.source);
                    let image = match self.textures.get(&key) {
                        Some(image) => image.clone(),
                        None => {
                            // Not all of it has arrived yet. Try again next frame.
                            let Some(image) = bitmap(term, p).and_then(render_image) else { continue };
                            self.textures.insert(key, image.clone());
                            image
                        }
                    };
                    used.insert(key);
                    let (x, y, w, h) = dest(p, term_cell, cell);
                    layout.drawn.push(Drawn {
                        layer: Layer::of(p.z),
                        bounds: Bounds::new(origin + point(px(x), px(y)), size(px(w), px(h))),
                        image,
                    });
                }
            }
        }
        let stale: Vec<Key> = self.textures.keys().filter(|k| !used.contains(*k)).copied().collect();
        for key in stale {
            if let Some(image) = self.textures.remove(&key) {
                window.drop_image(image).ok();
            }
        }
        layout
    }
}

fn render_image(bitmap: Bitmap) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

impl Layout {
    /// Paints the images in one layer, clipped to the grid.
    pub fn paint(&self, layer: Layer, window: &mut Window) {
        let Some(clip) = self.clip else { return };
        window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
            for drawn in self.drawn.iter().filter(|d| d.layer == layer) {
                if let Err(e) = window.paint_image(drawn.bounds, Corners::default(), drawn.image.clone(), 0, false) {
                    log::warn!("drawing an image: {e:#}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live copy of a terminal, 80 by 24, told its cells are 10 by 20 pixels.
    fn mirror() -> Mirror {
        let mut m = Mirror::new(5).unwrap();
        m.feed(b"\x1b_rex1;5;{\"type\":\"resize\",\"cols\":80,\"rows\":24,\"cell_width_px\":10,\"cell_height_px\":20}\x1b\\");
        assert!(m.is_live());
        m
    }

    /// The command that sends and places an image, with the pixels in base64.
    fn transmit(keys: &str, payload: &[u8]) -> Vec<u8> {
        format!("\x1b_Ga=T,q=2,{keys};{}\x1b\\", base64(payload)).into_bytes()
    }

    fn base64(data: &[u8]) -> String {
        const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    const RGB_2X2: [u8; 12] = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];

    #[test]
    fn layers_follow_the_z_index() {
        assert_eq!(Layer::of(0), Layer::AboveText);
        assert_eq!(Layer::of(7), Layer::AboveText);
        assert_eq!(Layer::of(-1), Layer::BelowText);
        assert_eq!(Layer::of(i32::MIN / 2), Layer::BelowText);
        assert_eq!(Layer::of(i32::MIN / 2 - 1), Layer::BelowBackground);
        assert_eq!(Layer::of(i32::MIN), Layer::BelowBackground);
    }

    #[test]
    fn reads_a_placed_image_and_its_pixels() {
        let mut m = mirror();
        m.feed(b"hi\r\n");
        m.feed(&transmit("i=7,f=24,s=2,v=2,c=4,r=2", &RGB_2X2));
        let ps = placements(m.terminal());
        assert_eq!(ps.len(), 1);
        let p = &ps[0];
        assert_eq!((p.image_id, p.col, p.row, p.z), (7, 0, 1, 0));
        // Four cells by two, at the 10 by 20 pixel cells the program was told.
        assert_eq!((p.width, p.height), (40, 40));
        assert_eq!(p.source, Rect { x: 0, y: 0, width: 2, height: 2 });
        assert_ne!(p.generation, 0);
        let bitmap = bitmap(m.terminal(), p).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (2, 2));
        assert_eq!(&bitmap.bgra[..8], &[0, 0, 255, 255, 0, 255, 0, 255]);
        assert_eq!(cell_px(m.terminal()), (10.0, 20.0));
    }

    #[test]
    fn decodes_png() {
        // A 1 by 1 PNG of opaque pure blue.
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde, 0x00,
            0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x60, 0xf8, 0x0f, 0x00, 0x01, 0x03,
            0x01, 0x00, 0x08, 0x89, 0xc2, 0xec, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
            0x82,
        ];
        let mut m = mirror();
        m.feed(&transmit("i=3,f=100", png));
        let ps = placements(m.terminal());
        assert_eq!(ps.len(), 1, "the PNG was stored and placed");
        let bitmap = bitmap(m.terminal(), &ps[0]).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (1, 1));
        assert_eq!(bitmap.bgra, vec![255, 0, 0, 255]);
    }

    #[test]
    fn orders_by_z_and_follows_scrolling() {
        let mut m = mirror();
        m.feed(&transmit("i=1,f=24,s=2,v=2,c=2,r=1,z=5", &RGB_2X2));
        m.feed(b"\x1b[21;1H");
        m.feed(&transmit("i=2,f=24,s=2,v=2,c=2,r=1,z=-3", &RGB_2X2));
        let rows = |m: &Mirror| placements(m.terminal()).iter().map(|p| (p.image_id, p.row)).collect::<Vec<_>>();
        // The lower z-index is drawn first.
        assert_eq!(rows(&m), vec![(2, 20), (1, 0)]);

        // Ten more lines push the first image above the view, and move the
        // second up with the text.
        m.feed(b"\x1b[24;1H");
        m.feed(&[b'\n'; 10]);
        assert_eq!(rows(&m), vec![(2, 10)]);

        // Scrolling back into the scrollback brings the first one back.
        m.scroll(-10);
        assert_eq!(rows(&m), vec![(2, 20), (1, 0)]);
        m.scroll_to_bottom();
        assert_eq!(rows(&m), vec![(2, 10)]);
    }

    #[test]
    fn deleting_removes_the_placement() {
        let mut m = mirror();
        m.feed(&transmit("i=9,f=24,s=2,v=2", &RGB_2X2));
        assert_eq!(placements(m.terminal()).len(), 1);
        m.feed(b"\x1b_Ga=d,d=I,i=9,q=2\x1b\\");
        assert!(placements(m.terminal()).is_empty());
    }

    #[test]
    fn scales_terminal_pixels_to_the_cells_drawn() {
        let p = Placement {
            image_id: 1,
            generation: 1,
            z: 0,
            col: 3,
            row: -2,
            x_offset: 5,
            y_offset: 10,
            width: 40,
            height: 40,
            source: Rect::default(),
        };
        // The server's cells are 10 by 20; these are 8 by 16.
        assert_eq!(dest(&p, (10.0, 20.0), (8.0, 16.0)), (28.0, -24.0, 32.0, 32.0));
        // An unknown cell size counts as one pixel, and doesn't divide by zero.
        assert_eq!(dest(&p, (0.0, 0.0), (8.0, 16.0)).2, 320.0);
    }

    #[test]
    fn converts_formats_and_crops() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let all = Rect { x: 0, y: 0, width: 2, height: 2 };
        let b = to_bgra(4, 2, 2, &rgba, all, 100).unwrap();
        assert_eq!(b.bgra, vec![3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]);
        // The bottom-right pixel alone.
        let b = to_bgra(4, 2, 2, &rgba, Rect { x: 1, y: 1, width: 5, height: 5 }, 100).unwrap();
        assert_eq!((b.width, b.height, b.bgra), (1, 1, vec![15, 14, 13, 16]));
        assert_eq!(to_bgra(1, 2, 1, &[9, 200], all, 100).unwrap().bgra, vec![9, 9, 9, 255, 200, 200, 200, 255]);
        assert_eq!(to_bgra(2, 1, 1, &[9, 50], all, 100).unwrap().bgra, vec![9, 9, 9, 50]);
        // Pixels still on the way, and parts outside the image, make nothing.
        assert_eq!(to_bgra(3, 2, 2, &[0; 11], all, 100), None);
        assert_eq!(to_bgra(3, 2, 2, &[0; 12], Rect { x: 2, y: 0, width: 1, height: 1 }, 100), None);
    }

    #[test]
    fn scales_down_what_a_texture_cant_hold() {
        let (w, h) = (10u32, 4u32);
        let data: Vec<u8> = (0..w * h).map(|i| i as u8).collect();
        let b = to_bgra(1, w, h, &data, Rect { x: 0, y: 0, width: w, height: h }, 5).unwrap();
        assert_eq!((b.width, b.height), (5, 2));
        // Every other pixel of every other row.
        let grays: Vec<u8> = b.bgra.chunks(4).map(|p| p[0]).collect();
        assert_eq!(grays, vec![0, 2, 4, 6, 8, 20, 22, 24, 26, 28]);
    }
}
