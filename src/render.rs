//! Building key images: background, icon, label → JPEG for the deck.

use crate::config::Style;
use crate::icons::{self, IconRef};
use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result, bail};
use image::{Rgb, RgbImage, imageops};
use std::path::Path;

const DEFAULT_BACKGROUND: [u8; 3] = [0x14, 0x16, 0x1c];
const DEFAULT_COLOR: [u8; 3] = [0xff, 0xff, 0xff];
const DEFAULT_FONT_SIZE: f32 = 13.0;
const DEFAULT_PADDING: u32 = 8;

pub struct Renderer {
    /// Edge length of the key image.
    pub size: u32,
    pub rotate180: bool,
    font: Option<FontArc>,
}

impl Renderer {
    pub fn new(size: u32, rotate180: bool, font_path: Option<&Path>) -> Self {
        let font = load_font(font_path);
        if font.is_none() {
            log::warn!("no font found — labels will not be drawn");
        }
        Self {
            size,
            rotate180,
            font,
        }
    }

    /// Render one key image as JPEG.
    pub fn render(
        &self,
        style: &Style,
        label: Option<&str>,
        icon: Option<&IconRef>,
        pressed: bool,
    ) -> Result<Vec<u8>> {
        let size = self.size;
        let bg = parse_color(style.background.as_deref()).unwrap_or(DEFAULT_BACKGROUND);
        let fg = parse_color(style.color.as_deref()).unwrap_or(DEFAULT_COLOR);
        let icon_fg = parse_color(style.icon_color.as_deref()).unwrap_or(fg);
        let font_size = style.font_size.unwrap_or(DEFAULT_FONT_SIZE);
        let padding = style.padding.unwrap_or(DEFAULT_PADDING);

        let mut canvas = RgbImage::from_pixel(size, size, Rgb(bg));

        // Height taken up by the label at the bottom.
        let lines = label
            .filter(|l| !l.trim().is_empty())
            .zip(self.font.as_ref())
            .map(|(text, font)| wrap_text(font, font_size, text, size as f32 - 4.0))
            .unwrap_or_default();
        let line_height = (font_size * 1.25).ceil();
        let text_block = if lines.is_empty() {
            0.0
        } else {
            line_height * lines.len() as f32 + 2.0
        };

        if let Some(icon) = icon {
            let avail_h = (size as f32 - text_block).round().max(1.0) as u32;
            match icon {
                IconRef::File(path) => self
                    .draw_image(&mut canvas, path, avail_h, padding)
                    .with_context(|| format!("could not load icon {}", path.display()))?,
                IconRef::Glyph { glyph, name } => self
                    .draw_glyph(&mut canvas, *glyph, avail_h, padding, icon_fg)
                    .with_context(|| format!("could not draw icon `mdi:{name}`"))?,
                IconRef::Unknown(name) => {
                    let hints = icons::suggestions(name);
                    if hints.is_empty() {
                        bail!("unknown icon `mdi:{name}`");
                    }
                    bail!(
                        "unknown icon `mdi:{name}` — did you mean {}?",
                        hints
                            .iter()
                            .map(|h| format!("`mdi:{h}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }

        if !lines.is_empty() {
            let font = self
                .font
                .as_ref()
                .expect("lines are only filled when a font exists");
            // Without an icon the text sits centred, otherwise at the bottom.
            let block_top = if icon.is_some() {
                size as f32 - text_block
            } else {
                (size as f32 - line_height * lines.len() as f32) / 2.0
            };
            for (i, line) in lines.iter().enumerate() {
                draw_line(
                    &mut canvas,
                    font,
                    font_size,
                    line,
                    block_top + i as f32 * line_height,
                    fg,
                );
            }
        }

        if pressed {
            brighten(&mut canvas, 1.45);
        }

        if self.rotate180 {
            canvas = imageops::rotate180(&canvas);
        }

        let mut jpeg = Vec::with_capacity(4096);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92).encode(
            &canvas,
            size,
            size,
            image::ExtendedColorType::Rgb8,
        )?;
        Ok(jpeg)
    }

    /// Blank image in the background colour, for keys with nothing on them.
    pub fn render_blank(&self, style: &Style) -> Result<Vec<u8>> {
        self.render(style, None, None, false)
    }

    /// Draw a glyph from the icon font, scaled to fill the available box.
    fn draw_glyph(
        &self,
        canvas: &mut RgbImage,
        glyph: char,
        avail_h: u32,
        padding: u32,
        color: [u8; 3],
    ) -> Result<()> {
        let font = icons::font();
        let id = font.glyph_id(glyph);
        let box_w = self.size.saturating_sub(padding * 2).max(1) as f32;
        let box_h = avail_h.saturating_sub(padding * 2).max(1) as f32;
        let target = box_w.min(box_h);

        // Measure once at a nominal size, then scale so the glyph's own bounds
        // fill the box — em boxes carry padding that varies per icon.
        let Some(probe) = font.outline_glyph(id.with_scale(target)) else {
            bail!("the icon font has no outline for U+{:04X}", glyph as u32);
        };
        let probe_bounds = probe.px_bounds();
        let extent = probe_bounds.width().max(probe_bounds.height()).max(1.0);
        let scale = target * (target / extent);

        let Some(outlined) = font.outline_glyph(id.with_scale(scale)) else {
            bail!("the icon font has no outline for U+{:04X}", glyph as u32);
        };
        let bounds = outlined.px_bounds();
        let off_x = (self.size as f32 - bounds.width()) / 2.0 - bounds.min.x;
        let off_y = (avail_h as f32 - bounds.height()) / 2.0 - bounds.min.y;

        outlined.draw(|gx, gy, coverage| {
            blend_px(
                canvas,
                bounds.min.x + gx as f32 + off_x,
                bounds.min.y + gy as f32 + off_y,
                color,
                coverage,
            );
        });
        Ok(())
    }

    fn draw_image(
        &self,
        canvas: &mut RgbImage,
        path: &Path,
        avail_h: u32,
        padding: u32,
    ) -> Result<()> {
        let icon = image::open(path)?.to_rgba8();
        let box_w = self.size.saturating_sub(padding * 2).max(1);
        let box_h = avail_h.saturating_sub(padding * 2).max(1);
        let scaled = imageops::resize(
            &icon,
            box_w.min(box_h),
            box_w.min(box_h),
            imageops::FilterType::Lanczos3,
        );

        let off_x = (self.size as i64 - scaled.width() as i64) / 2;
        let off_y = (avail_h as i64 - scaled.height() as i64) / 2;

        // Blend alpha over the background.
        for (x, y, px) in scaled.enumerate_pixels() {
            let (tx, ty) = (off_x + x as i64, off_y + y as i64);
            if tx < 0 || ty < 0 || tx >= self.size as i64 || ty >= self.size as i64 {
                continue;
            }
            let alpha = px[3] as f32 / 255.0;
            if alpha <= 0.0 {
                continue;
            }
            let under = canvas.get_pixel(tx as u32, ty as u32).0;
            let mut out = [0u8; 3];
            for c in 0..3 {
                out[c] = (px[c] as f32 * alpha + under[c] as f32 * (1.0 - alpha)).round() as u8;
            }
            canvas.put_pixel(tx as u32, ty as u32, Rgb(out));
        }
        Ok(())
    }
}

/// Wrap text to the key width, three lines at most.
fn wrap_text(font: &FontArc, font_size: f32, text: &str, max_width: f32) -> Vec<String> {
    let scaled = font.as_scaled(PxScale::from(font_size));
    let width_of = |s: &str| -> f32 {
        let mut w = 0.0;
        let mut prev = None;
        for c in s.chars() {
            let id = scaled.glyph_id(c);
            if let Some(p) = prev {
                w += scaled.kern(p, id);
            }
            w += scaled.h_advance(id);
            prev = Some(id);
        }
        w
    };

    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(last) if width_of(&format!("{last} {word}")) <= max_width => {
                last.push(' ');
                last.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }

    if lines.len() > 3 {
        lines.truncate(3);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines
}

/// Draw one line of text, horizontally centred, with a dark shadow so it stays
/// readable on top of bright icons.
fn draw_line(
    canvas: &mut RgbImage,
    font: &FontArc,
    font_size: f32,
    text: &str,
    top: f32,
    fg: [u8; 3],
) {
    let scaled = font.as_scaled(PxScale::from(font_size));
    let mut width = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            width += scaled.kern(p, id);
        }
        width += scaled.h_advance(id);
        prev = Some(id);
    }

    let mut pen_x = (canvas.width() as f32 - width) / 2.0;
    let base_y = top + scaled.ascent();
    let mut prev = None;

    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            pen_x += scaled.kern(p, id);
        }
        let glyph =
            id.with_scale_and_position(PxScale::from(font_size), ab_glyph::point(pen_x, base_y));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x + gx as f32;
                let py = bounds.min.y + gy as f32;
                // Shadow offset by one pixel, then the glyph itself.
                blend_px(canvas, px + 1.0, py + 1.0, [0, 0, 0], coverage * 0.7);
                blend_px(canvas, px, py, fg, coverage);
            });
        }
        pen_x += scaled.h_advance(id);
        prev = Some(id);
    }
}

fn blend_px(canvas: &mut RgbImage, x: f32, y: f32, color: [u8; 3], alpha: f32) {
    if x < 0.0 || y < 0.0 || alpha <= 0.0 {
        return;
    }
    let (x, y) = (x as u32, y as u32);
    if x >= canvas.width() || y >= canvas.height() {
        return;
    }
    let a = alpha.clamp(0.0, 1.0);
    let under = canvas.get_pixel(x, y).0;
    let mut out = [0u8; 3];
    for c in 0..3 {
        out[c] = (color[c] as f32 * a + under[c] as f32 * (1.0 - a)).round() as u8;
    }
    canvas.put_pixel(x, y, Rgb(out));
}

fn brighten(canvas: &mut RgbImage, factor: f32) {
    for px in canvas.pixels_mut() {
        for c in 0..3 {
            // The constant makes dark keys react visibly too.
            px[c] = ((px[c] as f32 * factor) + 26.0).min(255.0) as u8;
        }
    }
}

/// `#rgb`, `#rrggbb`, or one of a few common colour names.
pub fn parse_color(raw: Option<&str>) -> Option<[u8; 3]> {
    let raw = raw?.trim();
    let named = match raw.to_ascii_lowercase().as_str() {
        "black" => Some([0, 0, 0]),
        "white" => Some([255, 255, 255]),
        "red" => Some([0xd7, 0x2f, 0x2f]),
        "green" => Some([0x2f, 0xa8, 0x4f]),
        "blue" => Some([0x2f, 0x6f, 0xd7]),
        "yellow" => Some([0xe0, 0xb0, 0x20]),
        "orange" => Some([0xe0, 0x7b, 0x20]),
        "purple" => Some([0x8a, 0x4f, 0xd7]),
        "grey" | "gray" => Some([0x60, 0x60, 0x60]),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    let hex = raw.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let v: Vec<u8> = hex
                .chars()
                .map(|c| u8::from_str_radix(&c.to_string(), 16).unwrap_or(0) * 17)
                .collect();
            Some([v[0], v[1], v[2]])
        }
        6 | 8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some([r, g, b])
        }
        _ => None,
    }
}

/// Load a font: the configured path first, then the usual system locations.
fn load_font(explicit: Option<&Path>) -> Option<FontArc> {
    if let Some(path) = explicit {
        match std::fs::read(path)
            .map_err(anyhow::Error::from)
            .and_then(|d| Ok(FontArc::try_from_vec(d)?))
        {
            Ok(font) => return Some(font),
            Err(e) => log::warn!("font {} is unusable: {e}", path.display()),
        }
    }

    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/liberation-sans-fonts/LiberationSans-Bold.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/open-sans/OpenSans-Bold.ttf",
        "/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf",
        "/usr/share/fonts/google-noto-vf/NotoSans[wght].ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(data) = std::fs::read(path)
            && let Ok(font) = FontArc::try_from_vec(data)
        {
            log::debug!("font: {path}");
            return Some(font);
        }
    }
    None
}
