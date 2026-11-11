use std::path::Path;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TextureFormat {
    Rgb565,
    Rgba5551,
    Rgba4444,
    Rgba8888,
    LuminanceAlpha88,
    A8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Rgba8>,
}

impl Texture {
    pub fn from_bytes(
        raw: &[u8],
        width: usize,
        height: usize,
        format: TextureFormat,
        a8_tint: Rgba8,
    ) -> Self {
        let pixels = decode_texture_pixels(raw, width, height, format, a8_tint);
        Self {
            width,
            height,
            pixels,
        }
    }

    /// Decode bytes from a live GL upload. The hardware-facing uploader uses
    /// nearest-integer expansion for packed 5/6-bit channels; the public
    /// offline helper retains its historical truncating behavior for replay
    /// fixtures and manifest-derived expectations.
    pub(crate) fn from_live_gl_bytes(
        raw: &[u8],
        width: usize,
        height: usize,
        format: TextureFormat,
        a8_tint: Rgba8,
    ) -> Self {
        let pixels = decode_texture_pixels_impl(raw, width, height, format, a8_tint, true);
        Self {
            width,
            height,
            pixels,
        }
    }
}

/// A texture decoded from the title's manifest rather than uploaded by the
/// guest. The direct PR runner exposes these as pre-existing GL texture names
/// before the eApp starts issuing ordinary upload calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedManifestTexture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Rgba8>,
    pub format: TextureFormat,
}

/// Pull `Files[].Path` entries from an XML `Manifest.plist`, preserving the
/// document order used by the direct HLE runner. A full plist parser is not
/// needed for this narrow resource contract.
pub(crate) fn manifest_texture_paths(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut paths = Vec::new();
    let mut rest = text.as_str();
    while let Some(index) = rest.find("<key>Path</key>") {
        rest = &rest[index + 15..];
        let start = rest.find("<string>")?;
        let end = rest[start + 8..].find("</string>")?;
        paths.push(rest[start + 8..start + 8 + end].to_string());
        rest = &rest[start + 8 + end..];
    }
    (!paths.is_empty()).then_some(paths)
}

fn expand16(value: u16, rgb565: bool) -> Rgba8 {
    let (r, g, b) = if rgb565 {
        (
            ((value >> 11) & 0x1f) as u8,
            ((value >> 5) & 0x3f) as u8,
            (value & 0x1f) as u8,
        )
    } else {
        (
            ((value >> 10) & 0x1f) as u8,
            ((value >> 5) & 0x1f) as u8,
            (value & 0x1f) as u8,
        )
    };
    let green = if rgb565 { (g << 2) | (g >> 4) } else { (g << 3) | (g >> 2) };
    // The iPod resource formats use magenta as their transparent colour key.
    let alpha = if value == 0xf83e { 0 } else { 255 };
    Rgba8::rgba(
        (r << 3) | (r >> 2),
        green,
        (b << 3) | (b >> 2),
        alpha,
    )
}

/// Decode an uncompressed 16-bit TGA. Preserve the PR runner's row handling;
/// presentation flips, when needed, happen after rasterization.
fn decode_manifest_tga(data: &[u8]) -> Option<DecodedManifestTexture> {
    if data.len() < 18 || data[2] != 2 || data[16] != 16 {
        return None;
    }
    let width = u16::from_le_bytes([data[12], data[13]]) as usize;
    let height = u16::from_le_bytes([data[14], data[15]]) as usize;
    let top = data[17] & 0x20 != 0;
    if width == 0 || height == 0 || data.len() < 18 + width * height * 2 {
        return None;
    }
    let mut pixels = vec![Rgba8::rgba(0, 0, 0, 0); width * height];
    for y in 0..height {
        let source_y = if top { height - 1 - y } else { y };
        for x in 0..width {
            let offset = 18 + (source_y * width + x) * 2;
            pixels[y * width + x] = expand16(
                u16::from_le_bytes([data[offset], data[offset + 1]]),
                false,
            );
        }
    }
    Some(DecodedManifestTexture {
        width,
        height,
        pixels,
        // TGA's colour-keyed 5:5:5 payload is represented as decoded RGBA.
        format: TextureFormat::Rgba8888,
    })
}

/// Decode `.ipd`: width, height, type and RGB format, followed by RGB565
/// pixels. This is the image container used by Vortex and Cubis 2.
fn decode_manifest_ipd(data: &[u8]) -> Option<DecodedManifestTexture> {
    if data.len() < 16 {
        return None;
    }
    let width = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let height = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    if width == 0
        || height == 0
        || width > 4096
        || height > 4096
        || data.len() < 16 + width * height * 2
    {
        return None;
    }
    let mut pixels = Vec::with_capacity(width * height);
    for index in 0..width * height {
        let offset = 16 + index * 2;
        pixels.push(expand16(
            u16::from_le_bytes([data[offset], data[offset + 1]]),
            true,
        ));
    }
    Some(DecodedManifestTexture {
        width,
        height,
        pixels,
        format: TextureFormat::Rgb565,
    })
}

/// Decode a `.pix` file. The extension is unusual, but the payload is a
/// Windows BMP. Tetris and Cubis 2 use 16/32-bit colour and 8-bit coverage
/// atlases in this form.
fn decode_manifest_bmp(data: &[u8]) -> Option<DecodedManifestTexture> {
    if data.len() < 54 || &data[0..2] != b"BM" {
        return None;
    }
    let read_u16 = |offset: usize| {
        data.get(offset..offset + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let read_u32 = |offset: usize| {
        data.get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let data_offset = read_u32(10)? as usize;
    let dib_size = read_u32(14)? as usize;
    if dib_size < 40 {
        return None;
    }
    let raw_width = read_u32(18)? as i32;
    let raw_height = read_u32(22)? as i32;
    let bits_per_pixel = read_u16(28)?;
    let compression = read_u32(30)?;
    if raw_width <= 0 || raw_height == 0 {
        return None;
    }
    let width = raw_width as usize;
    let height = raw_height.unsigned_abs() as usize;
    let top_down = raw_height < 0;
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }

    // BI_BITFIELDS masks follow the 40-byte base header. BI_RGB uses the
    // standard masks for the depths shipped by the games.
    let (red_mask, green_mask, blue_mask, alpha_mask) = match (compression, bits_per_pixel) {
        (3, _) => (
            read_u32(54)?,
            read_u32(58)?,
            read_u32(62)?,
            read_u32(66).unwrap_or(0),
        ),
        (0, 32) => (0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000),
        (0, 16) => (0x7c00, 0x03e0, 0x001f, 0x8000),
        (0, 24) | (0, 8) => (0, 0, 0, 0),
        _ => return None,
    };
    let channel = |value: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 255;
        }
        let shift = mask.trailing_zeros();
        let bits = mask.count_ones();
        let sample = (value & mask) >> shift;
        let max = if bits == 32 { u32::MAX } else { (1u32 << bits) - 1 };
        if bits == 8 {
            sample as u8
        } else {
            ((sample * 255 + max / 2) / max) as u8
        }
    };

    let palette_entries = if bits_per_pixel <= 8 {
        let used = read_u32(46).unwrap_or(0) as usize;
        if used == 0 {
            1usize << bits_per_pixel
        } else {
            used
        }
    } else {
        0
    };
    let palette_offset = 14 + dib_size;
    // `_a8` files carry a grayscale palette with zero alpha bytes. In that
    // case the byte index is coverage, and the draw colour supplies RGB.
    let alpha_ramp = bits_per_pixel == 8
        && palette_entries >= 2
        && (0..palette_entries).all(|index| {
            let offset = palette_offset + index * 4;
            data.get(offset..offset + 4).is_some_and(|entry| {
                entry[0] == entry[1]
                    && entry[1] == entry[2]
                    && entry[0] as usize == index
                    && entry[3] == 0
            })
        });

    let row_bytes = (width * bits_per_pixel as usize).div_ceil(8);
    let row_stride = row_bytes.div_ceil(4) * 4;
    if data_offset + row_stride * height > data.len() {
        return None;
    }
    let mut pixels = vec![Rgba8::rgba(0, 0, 0, 0); width * height];
    for y in 0..height {
        let source_y = if top_down { y } else { height - 1 - y };
        let row = data_offset + source_y * row_stride;
        for x in 0..width {
            pixels[y * width + x] = match bits_per_pixel {
                8 => {
                    let index = data[row + x] as usize;
                    if alpha_ramp {
                        Rgba8::rgba(255, 255, 255, index as u8)
                    } else {
                        let offset = palette_offset + index * 4;
                        match data.get(offset..offset + 4) {
                            Some(entry) => Rgba8::rgba(entry[2], entry[1], entry[0], 255),
                            None => Rgba8::rgba(0, 0, 0, 0),
                        }
                    }
                }
                16 => {
                    let offset = row + x * 2;
                    let value = u16::from_le_bytes([data[offset], data[offset + 1]]) as u32;
                    Rgba8::rgba(
                        channel(value, red_mask),
                        channel(value, green_mask),
                        channel(value, blue_mask),
                        channel(value, alpha_mask),
                    )
                }
                24 => {
                    let offset = row + x * 3;
                    Rgba8::rgba(data[offset + 2], data[offset + 1], data[offset], 255)
                }
                32 => {
                    let offset = row + x * 4;
                    let value = u32::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    ]);
                    Rgba8::rgba(
                        channel(value, red_mask),
                        channel(value, green_mask),
                        channel(value, blue_mask),
                        channel(value, alpha_mask),
                    )
                }
                _ => return None,
            };
        }
    }
    Some(DecodedManifestTexture {
        width,
        height,
        pixels,
        format: if alpha_ramp {
            TextureFormat::A8
        } else if bits_per_pixel == 16 {
            TextureFormat::Rgba5551
        } else {
            TextureFormat::Rgba8888
        },
    })
}

/// Decode headerless RGB565 resources whose dimensions are unambiguous from
/// the byte count (a square or a 2:1 rectangle).
fn decode_manifest_raw_rgb565(data: &[u8]) -> Option<DecodedManifestTexture> {
    let pixels_count = data.len() / 2;
    if data.len() % 2 != 0 || pixels_count == 0 {
        return None;
    }
    let side = (pixels_count as f64).sqrt() as usize;
    let (width, height) = if side * side == pixels_count {
        (side, side)
    } else if pixels_count / 2 > 0
        && (pixels_count / 2) * 2 == pixels_count
        && side > 0
        && (side * 2) * (side / 2) == pixels_count
    {
        (side * 2, side / 2)
    } else {
        return None;
    };
    let mut pixels = Vec::with_capacity(width * height);
    for index in 0..width * height {
        let offset = index * 2;
        pixels.push(expand16(
            u16::from_le_bytes([data[offset], data[offset + 1]]),
            true,
        ));
    }
    Some(DecodedManifestTexture {
        width,
        height,
        pixels,
        format: TextureFormat::Rgb565,
    })
}

/// Decode one manifest-listed image using the same extension contract as the
/// PR direct runner. Unknown files are intentionally ignored.
pub(crate) fn decode_manifest_texture(path: &Path, data: &[u8]) -> Option<DecodedManifestTexture> {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())?;
    match extension.as_str() {
        "tga" => decode_manifest_tga(data),
        "ipd" => decode_manifest_ipd(data),
        "bin" => decode_manifest_raw_rgb565(data),
        "pix" | "bmp" => decode_manifest_bmp(data),
        _ => None,
    }
}

pub fn decode_texture_pixels(
    raw: &[u8],
    width: usize,
    height: usize,
    format: TextureFormat,
    a8_tint: Rgba8,
) -> Vec<Rgba8> {
    decode_texture_pixels_impl(raw, width, height, format, a8_tint, false)
}

fn decode_texture_pixels_impl(
    raw: &[u8],
    width: usize,
    height: usize,
    format: TextureFormat,
    a8_tint: Rgba8,
    round_packed_channels: bool,
) -> Vec<Rgba8> {
    let expected = match format {
        TextureFormat::Rgb565 | TextureFormat::Rgba5551 | TextureFormat::Rgba4444 => {
            width * height * 2
        }
        TextureFormat::Rgba8888 => width * height * 4,
        TextureFormat::LuminanceAlpha88 => width * height * 2,
        TextureFormat::A8 => width * height,
    };
    assert_eq!(raw.len(), expected);
    let expand = |value: u16, max: u16| -> u8 {
        if round_packed_channels {
            ((value * 255 + max / 2) / max) as u8
        } else {
            (value * 255 / max) as u8
        }
    };
    match format {
        TextureFormat::Rgb565 => raw
            .chunks_exact(2)
            .map(|chunk| {
                let px = u16::from_le_bytes([chunk[0], chunk[1]]);
                let r = ((px >> 11) & 0x1f) as u8;
                let g = ((px >> 5) & 0x3f) as u8;
                let b = (px & 0x1f) as u8;
                Rgba8::rgba(
                    expand(r.into(), 31),
                    expand(g.into(), 63),
                    expand(b.into(), 31),
                    255,
                )
            })
            .collect(),
        TextureFormat::Rgba5551 => raw
            .chunks_exact(2)
            .map(|chunk| {
                let px = u16::from_le_bytes([chunk[0], chunk[1]]);
                let r = ((px >> 11) & 0x1f) as u8;
                let g = ((px >> 6) & 0x1f) as u8;
                let b = ((px >> 1) & 0x1f) as u8;
                let a = (px & 0x1) as u8;
                Rgba8::rgba(
                    expand(r.into(), 31),
                    expand(g.into(), 31),
                    expand(b.into(), 31),
                    if a != 0 { 255 } else { 0 },
                )
            })
            .collect(),
        TextureFormat::Rgba4444 => raw
            .chunks_exact(2)
            .map(|chunk| {
                let px = u16::from_le_bytes([chunk[0], chunk[1]]);
                let r = ((px >> 12) & 0x0f) as u8;
                let g = ((px >> 8) & 0x0f) as u8;
                let b = ((px >> 4) & 0x0f) as u8;
                let a = (px & 0x0f) as u8;
                Rgba8::rgba(
                    (r as u16 * 255 / 15) as u8,
                    (g as u16 * 255 / 15) as u8,
                    (b as u16 * 255 / 15) as u8,
                    (a as u16 * 255 / 15) as u8,
                )
            })
            .collect(),
        TextureFormat::Rgba8888 => raw
            .chunks_exact(4)
            .map(|chunk| Rgba8::rgba(chunk[0], chunk[1], chunk[2], chunk[3]))
            .collect(),
        TextureFormat::LuminanceAlpha88 => raw
            .chunks_exact(2)
            .map(|chunk| Rgba8::rgba(chunk[0], chunk[0], chunk[0], chunk[1]))
            .collect(),
        TextureFormat::A8 => raw
            .iter()
            .map(|&alpha| Rgba8::rgba(a8_tint.r, a8_tint.g, a8_tint.b, alpha))
            .collect(),
    }
}

#[inline(always)]
pub fn sample_nearest(texture: &Texture, u: f32, v: f32) -> Rgba8 {
    let x = if u <= 0.0 { 0 } else { (u as usize).min(texture.width.saturating_sub(1)) };
    let y = if v <= 0.0 { 0 } else { (v as usize).min(texture.height.saturating_sub(1)) };
    texture.pixels[y * texture.width + x]
}

/// `x.round().clamp(0.0, 255.0) as u8` without the libm call: neither ARMv7
/// nor baseline x86-64 has a rounding instruction, so `f32::round` is a
/// software routine there. Negative values and NaN give 0, as before.
#[inline(always)]
fn round_to_u8(x: f32) -> u8 {
    let whole = x as u32;
    let rounded = if x - whole as f32 >= 0.5 { whole.saturating_add(1) } else { whole };
    rounded.min(255) as u8
}

/// Sample at texel centres using the filtering behavior observed in the PR
/// direct runner. RGB is interpolated in premultiplied-alpha space so keyed
/// transparent texels do not bleed their hidden colour into scaled edges.
pub fn sample_bilinear_premultiplied(texture: &Texture, u: f32, v: f32) -> Rgba8 {
    let (width, height) = (texture.width, texture.height);
    if width == 0 || height == 0 {
        return Rgba8::rgba(0, 0, 0, 0);
    }
    let sx = (u - 0.5).clamp(0.0, (width - 1) as f32);
    let sy = (v - 0.5).clamp(0.0, (height - 1) as f32);
    if !(sx >= 0.0 && sy >= 0.0) {
        // NaN coordinates: every weight is NaN, which rounds to nothing.
        return Rgba8::rgba(0, 0, 0, 0);
    }
    // Both are non-negative, so truncation is `floor`.
    let x0 = sx as usize;
    let y0 = sy as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let row0 = y0 * width;
    let row1 = y1 * width;
    let p00 = texture.pixels[row0 + x0];
    let p10 = texture.pixels[row0 + x1];
    let p01 = texture.pixels[row1 + x0];
    let p11 = texture.pixels[row1 + x1];
    if p00 == p10 && p00 == p01 && p00 == p11 {
        // Flat area: the weights sum to one, so the filter returns the texel
        // (or nothing when it is transparent).
        return if p00.a == 0 { Rgba8::rgba(0, 0, 0, 0) } else { p00 };
    }
    let dx = sx - x0 as f32;
    let dy = sy - y0 as f32;
    let weights = [
        ((1.0 - dx) * (1.0 - dy), p00),
        (dx * (1.0 - dy), p10),
        ((1.0 - dx) * dy, p01),
        (dx * dy, p11),
    ];
    let mut alpha_sum = 0.0f32;
    let mut channel_sum = [0.0f32; 3];
    for (weight, pixel) in weights {
        let alpha = pixel.a as f32;
        alpha_sum += weight * alpha;
        channel_sum[0] += weight * alpha * pixel.r as f32;
        channel_sum[1] += weight * alpha * pixel.g as f32;
        channel_sum[2] += weight * alpha * pixel.b as f32;
    }
    let alpha = round_to_u8(alpha_sum);
    if alpha_sum <= 0.0 {
        return Rgba8::rgba(0, 0, 0, alpha);
    }
    Rgba8::rgba(
        round_to_u8(channel_sum[0] / alpha_sum),
        round_to_u8(channel_sum[1] / alpha_sum),
        round_to_u8(channel_sum[2] / alpha_sum),
        alpha,
    )
}

#[inline(always)]
fn one_to_one_mapping(verts: &[(f32, f32, f32, f32); 3]) -> bool {
    let (mut min_x, mut max_x) = (verts[0].0, verts[0].0);
    let (mut min_y, mut max_y) = (verts[0].1, verts[0].1);
    let (mut min_u, mut max_u) = (verts[0].2, verts[0].2);
    let (mut min_v, mut max_v) = (verts[0].3, verts[0].3);
    for v in &verts[1..] {
        if v.0 < min_x { min_x = v.0; } else if v.0 > max_x { max_x = v.0; }
        if v.1 < min_y { min_y = v.1; } else if v.1 > max_y { max_y = v.1; }
        if v.2 < min_u { min_u = v.2; } else if v.2 > max_u { max_u = v.2; }
        if v.3 < min_v { min_v = v.3; } else if v.3 > max_v { max_v = v.3; }
    }
    let (position_w, position_h) = (max_x - min_x, max_y - min_y);
    let (uv_w, uv_h) = (max_u - min_u, max_v - min_v);
    position_w > 0.5
        && position_h > 0.5
        && (uv_w / position_w - 1.0).abs() < 0.02
        && (uv_h / position_h - 1.0).abs() < 0.02
}

#[inline(always)]
pub fn blend_src_over(dst: Rgba8, src: Rgba8) -> Rgba8 {
    if src.a == 0 {
        return dst;
    }
    if src.a == 255 {
        return src;
    }
    let sa = src.a as u32;
    let da = dst.a as u32;
    let inv_sa = 255 - sa;
    if da == 255 {
        // Opaque destination, the usual case once a background is drawn:
        // the result stays opaque and the divisor is the constant 255.
        let blend = |src_c: u8, dst_c: u8| -> u8 {
            ((src_c as u32 * sa + dst_c as u32 * inv_sa + 127) / 255) as u8
        };
        return Rgba8::rgba(
            blend(src.r, dst.r),
            blend(src.g, dst.g),
            blend(src.b, dst.b),
            255,
        );
    }
    let out_a = sa + (da * inv_sa + 127) / 255;
    if out_a == 0 {
        return Rgba8::rgba(0, 0, 0, 0);
    }
    let blend = |src_c: u8, dst_c: u8| -> u8 {
        let src_p = src_c as u32 * sa;
        let dst_p = dst_c as u32 * da * inv_sa / 255;
        ((src_p + dst_p + out_a / 2) / out_a) as u8
    };
    Rgba8::rgba(
        blend(src.r, dst.r),
        blend(src.g, dst.g),
        blend(src.b, dst.b),
        out_a as u8,
    )
}

#[inline(always)]
fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (px - ax) * (by - ay) - (py - ay) * (bx - ax)
}

#[inline(always)]
fn is_top_left(ax: f32, ay: f32, bx: f32, by: f32) -> bool {
    let dy = by - ay;
    let dx = bx - ax;
    dy > 0.0 || (dy == 0.0 && dx < 0.0)
}

#[inline(always)]
fn modulate(src: Rgba8, tint: Rgba8) -> Rgba8 {
    if tint.r == 255 && tint.g == 255 && tint.b == 255 && tint.a == 255 {
        return src;
    }
    let mul = |a: u8, b: u8| -> u8 { ((a as u16 * b as u16 + 127) / 255) as u8 };
    Rgba8::rgba(
        mul(src.r, tint.r),
        mul(src.g, tint.g),
        mul(src.b, tint.b),
        mul(src.a, tint.a),
    )
}

/// The primary colour of a triangle whose three vertex colours are equal,
/// with the tolerance that keeps it exact. Interpolating equal colours gives
/// `c * (w0 + w1 + w2)`, and the weights sum to one only up to float noise.
/// While `|w0 + w1 + w2 - 1|` stays within the tolerance, that noise cannot
/// move any channel across a rounding boundary, so the result is this colour.
/// Sprites, text and tinted tiles are all drawn this way.
fn flat_primary(colors: &[[f32; 4]; 3]) -> Option<(Rgba8, f32)> {
    if colors[0] != colors[1] || colors[0] != colors[2] {
        return None;
    }
    let mut out = [0u8; 4];
    let mut tolerance = 0.25f32;
    for (component, &c) in colors[0].iter().enumerate() {
        if c <= 0.0 {
            // Stays at or below zero whatever the weights are.
            continue;
        }
        let scaled = c.min(1.0) * 255.0;
        let rounded = round_to_u8(scaled);
        out[component] = rounded;
        // Distance to the nearest value that would round differently.
        let below = scaled - (rounded as f32 - 0.5);
        let above = if c >= 1.0 { f32::INFINITY } else { rounded as f32 + 0.5 - scaled };
        let margin = below.min(above) / scaled - 4.0e-6;
        if !(margin > 0.0) {
            return None;
        }
        tolerance = tolerance.min(margin);
    }
    Some((Rgba8::rgba(out[0], out[1], out[2], out[3]), tolerance))
}

/// What `rasterize_triangle_spans` writes for a covered pixel.
enum SpanSource {
    /// The texel under the pixel: column `tx0 + step_x * (x - min_x)` and row
    /// `ty0 + step_y * (y - min_y)`, clamped to the texture.
    Texels { tx0: i64, ty0: i64, step_x: i64, step_y: i64 },
    /// One texture sample for the whole triangle.
    Constant(Rgba8),
}

/// How far a nearest-neighbour coordinate must stay from a texel boundary for
/// the direct copy: the float noise of the per-pixel interpolation it
/// replaces is below 0.002 texels for the coordinate range accepted here.
const TEXEL_BOUNDARY_MARGIN: f64 = 1.0 / 64.0;
/// Largest vertex coordinate for which the edge functions of a triangle with
/// whole-pixel vertices are exact in `f32`.
const EXACT_EDGE_LIMIT: f32 = 1024.0;
const EXACT_UV_LIMIT: f32 = 2048.0;

/// Sprites, tiles, text and backgrounds are triangles with whole-pixel
/// vertices that either copy texels one to one or stretch a single texel. For
/// those the covered pixels and the texel under each are known without
/// interpolating per pixel, so each row becomes a plain copy.
///
/// Returns `None` whenever that cannot be shown to give exactly the pixels of
/// the general loop; the caller then runs the general loop. `v` is already
/// wound so that the area is positive.
#[allow(clippy::too_many_arguments)]
fn rasterize_triangle_spans(
    fb: &mut [Rgba8],
    fb_width: usize,
    tex: &Texture,
    v: &[(f32, f32, f32, f32); 3],
    colors: Option<&[[f32; 4]; 3]>,
    tint: Rgba8,
    is_one_to_one: bool,
    (min_x, max_x, min_y, max_y): (i32, i32, i32, i32),
) -> Option<u64> {
    if tex.width == 0 || tex.height == 0 || tex.pixels.len() < tex.width * tex.height {
        return None;
    }
    for p in v {
        let whole = p.0 == p.0.trunc() && p.1 == p.1.trunc();
        if !whole || p.0.abs() > EXACT_EDGE_LIMIT || p.1.abs() > EXACT_EDGE_LIMIT {
            return None;
        }
        if !(p.2.abs() <= EXACT_UV_LIMIT && p.3.abs() <= EXACT_UV_LIMIT) {
            return None;
        }
    }
    // With whole-pixel vertices the weights sum to one within 1e-6, so a flat
    // primary colour needs no per-pixel check.
    let primary = match colors {
        None => None,
        Some(colors) => match flat_primary(colors) {
            Some((primary, tolerance)) if tolerance >= 1.0e-5 => Some(primary),
            _ => return None,
        },
    };

    let source = if is_one_to_one {
        // Solve the affine texture mapping; it must be an unrotated 1:1 copy.
        let [a, b, c] = v.map(|p| (p.0 as f64, p.1 as f64, p.2 as f64, p.3 as f64));
        let area = (b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1);
        let du_dx = ((b.2 - a.2) * (c.1 - a.1) - (c.2 - a.2) * (b.1 - a.1)) / area;
        let du_dy = ((c.2 - a.2) * (b.0 - a.0) - (b.2 - a.2) * (c.0 - a.0)) / area;
        let dv_dx = ((b.3 - a.3) * (c.1 - a.1) - (c.3 - a.3) * (b.1 - a.1)) / area;
        let dv_dy = ((c.3 - a.3) * (b.0 - a.0) - (b.3 - a.3) * (c.0 - a.0)) / area;
        const SLOP: f64 = 1.0e-6;
        if !((du_dx.abs() - 1.0).abs() < SLOP
            && (dv_dy.abs() - 1.0).abs() < SLOP
            && du_dy.abs() < SLOP
            && dv_dx.abs() < SLOP)
        {
            return None;
        }
        // Texture coordinate at the centre of the first pixel of the bounds.
        let (px, py) = (min_x as f64 + 0.5 - a.0, min_y as f64 + 0.5 - a.1);
        let u = a.2 + du_dx * px + du_dy * py;
        let w = a.3 + dv_dx * px + dv_dy * py;
        let clear_of_boundary = |value: f64| {
            let fraction = value - value.floor();
            fraction >= TEXEL_BOUNDARY_MARGIN && fraction <= 1.0 - TEXEL_BOUNDARY_MARGIN
        };
        if !clear_of_boundary(u) || !clear_of_boundary(w) {
            // The texel is decided by float noise (half-texel offsets do
            // this); only the general loop reproduces that.
            return None;
        }
        SpanSource::Texels {
            tx0: u.floor() as i64,
            ty0: w.floor() as i64,
            step_x: if du_dx > 0.0 { 1 } else { -1 },
            step_y: if dv_dy > 0.0 { 1 } else { -1 },
        }
    } else {
        // One texture coordinate for the whole triangle: a solid fill taken
        // from a texel. The filter may land one cell either side of it, so
        // every texel it could touch must be the same.
        if v[1].2 != v[0].2 || v[2].2 != v[0].2 || v[1].3 != v[0].3 || v[2].3 != v[0].3 {
            return None;
        }
        let cell = |coordinate: f32, size: usize| -> (usize, usize) {
            let centre = (coordinate - 0.5).clamp(0.0, (size - 1) as f32) as usize;
            (centre.saturating_sub(1), (centre + 2).min(size - 1))
        };
        let (x_lo, x_hi) = cell(v[0].2, tex.width);
        let (y_lo, y_hi) = cell(v[0].3, tex.height);
        let texel = tex.pixels[y_lo * tex.width + x_lo];
        for y in y_lo..=y_hi {
            let row = &tex.pixels[y * tex.width + x_lo..=y * tex.width + x_hi];
            if row.iter().any(|&other| other != texel) {
                return None;
            }
        }
        SpanSource::Constant(if texel.a == 0 { Rgba8::rgba(0, 0, 0, 0) } else { texel })
    };

    let shade = |sample: Rgba8| -> Rgba8 {
        match primary {
            Some(primary) => modulate(modulate(sample, primary), tint),
            None => modulate(sample, tint),
        }
    };

    // The edge functions of the general loop, times four so that they are
    // integers at pixel centres. `edges[i]` is opposite vertex `i`.
    let doubled = v.map(|p| (2 * p.0 as i64, 2 * p.1 as i64));
    let edges = [(1usize, 2usize), (2, 0), (0, 1)].map(|(from, to)| {
        let (a, b) = (doubled[from], doubled[to]);
        let top_left = is_top_left(v[from].0, v[from].1, v[to].0, v[to].1);
        (a, b.0 - a.0, b.1 - a.1, top_left)
    });
    let px0 = 2 * min_x as i64 + 1;
    let width = (max_x - min_x) as i64;

    let mut coverage = 0u64;
    for y in min_y..=max_y {
        let py = 2 * y as i64 + 1;
        // Columns `first..=last` (relative to `min_x`) inside all three edges.
        let (mut first, mut last) = (0i64, width);
        for (a, dx, dy, top_left) in edges {
            let value = (px0 - a.0) * dy - (py - a.1) * dx;
            let step = 2 * dy;
            if step > 0 {
                // A left edge, which is always top-left: `value >= 0` counts,
                // from column `ceil(-value / step)` on.
                first = first.max(-value.div_euclid(step));
            } else if step < 0 {
                // A right edge, never top-left: `value > 0` is needed, up to
                // column `ceil(value / -step) - 1`.
                last = last.min(-(-value).div_euclid(-step) - 1);
            } else if value < 0 || (value == 0 && !top_left) {
                last = -1;
            }
        }
        if first > last {
            continue;
        }
        coverage += (last - first + 1) as u64;
        let row_start = y as usize * fb_width + min_x as usize;
        let row = &mut fb[row_start + first as usize..=row_start + last as usize];
        match source {
            SpanSource::Constant(sample) => {
                let src = shade(sample);
                if src.a == 255 {
                    row.fill(src);
                } else if src.a > 0 {
                    for dst in row.iter_mut() {
                        *dst = blend_src_over(*dst, src);
                    }
                }
            }
            SpanSource::Texels { tx0, ty0, step_x, step_y } => {
                let ty = (ty0 + step_y * (y - min_y) as i64).clamp(0, tex.height as i64 - 1) as usize;
                let texels = &tex.pixels[ty * tex.width..(ty + 1) * tex.width];
                let tx_first = tx0 + step_x * first;
                let tx_last = tx0 + step_x * last;
                let plain = primary.map_or(true, |c| c == Rgba8::rgba(255, 255, 255, 255))
                    && tint == Rgba8::rgba(255, 255, 255, 255);
                if plain && step_x == 1 && tx_first >= 0 && tx_last < tex.width as i64 {
                    // The common case: an unclipped, untinted run of texels.
                    let run = &texels[tx_first as usize..=tx_last as usize];
                    for (dst, &src) in row.iter_mut().zip(run) {
                        if src.a == 255 {
                            *dst = src;
                        } else if src.a > 0 {
                            *dst = blend_src_over(*dst, src);
                        }
                    }
                } else {
                    let mut tx = tx_first;
                    for dst in row.iter_mut() {
                        let sample = texels[tx.clamp(0, tex.width as i64 - 1) as usize];
                        tx += step_x;
                        if sample.a == 0 {
                            continue;
                        }
                        let src = shade(sample);
                        if src.a > 0 {
                            *dst = blend_src_over(*dst, src);
                        }
                    }
                }
            }
        }
    }
    Some(coverage)
}

pub fn rasterize_triangle_tinted(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    tex: &Texture,
    verts: &[(f32, f32, f32, f32); 3],
    tint: Rgba8,
) -> u64 {
    rasterize_triangle_tinted_with_vertex_colors(fb, fb_width, fb_height, tex, verts, tint, None)
}

/// Rasterize a textured triangle while optionally interpolating a primary
/// colour supplied by the guest's GL colour array. The direct PR runner keeps
/// this colour separate from the constant colour register; Vortex uses it to
/// turn one greyscale ring atlas into the coloured attract-mode tiles.
pub fn rasterize_triangle_tinted_with_vertex_colors(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    tex: &Texture,
    verts: &[(f32, f32, f32, f32); 3],
    tint: Rgba8,
    vertex_colors: Option<&[[f32; 4]; 3]>,
) -> u64 {
    let mut v = *verts;
    let mut colors = vertex_colors.copied();
    if edge(v[0].0, v[0].1, v[1].0, v[1].1, v[2].0, v[2].1) < 0.0 {
        v.swap(1, 2);
        if let Some(colors) = colors.as_mut() {
            colors.swap(1, 2);
        }
    }

    let (mut min_x_f, mut max_x_f) = (v[0].0, v[0].0);
    let (mut min_y_f, mut max_y_f) = (v[0].1, v[0].1);
    for p in &v[1..] {
        if p.0 < min_x_f { min_x_f = p.0; } else if p.0 > max_x_f { max_x_f = p.0; }
        if p.1 < min_y_f { min_y_f = p.1; } else if p.1 > max_y_f { max_y_f = p.1; }
    }
    let min_x = min_x_f.floor().max(0.0) as i32;
    let min_y = min_y_f.floor().max(0.0) as i32;
    let max_x = max_x_f.ceil().min((fb_width - 1) as f32) as i32;
    let max_y = max_y_f.ceil().min((fb_height - 1) as f32) as i32;
    if min_x > max_x || min_y > max_y {
        return 0;
    }

    let area = edge(v[0].0, v[0].1, v[1].0, v[1].1, v[2].0, v[2].1);
    if area == 0.0 {
        return 0;
    }

    let tl01 = is_top_left(v[0].0, v[0].1, v[1].0, v[1].1);
    let tl12 = is_top_left(v[1].0, v[1].1, v[2].0, v[2].1);
    let tl20 = is_top_left(v[2].0, v[2].1, v[0].0, v[0].1);

    let is_one_to_one = one_to_one_mapping(&v);
    if let Some(coverage) = rasterize_triangle_spans(
        fb,
        fb_width,
        tex,
        &v,
        colors.as_ref(),
        tint,
        is_one_to_one,
        (min_x, max_x, min_y, max_y),
    ) {
        return coverage;
    }
    let inv_area = 1.0 / area;
    let dy12 = v[2].1 - v[1].1;
    let dy20 = v[0].1 - v[2].1;
    let dy01 = v[1].1 - v[0].1;
    let start_x = min_x as f32 + 0.5;
    let (min_x, max_x) = (min_x as usize, max_x as usize);
    let flat = colors.as_ref().and_then(flat_primary);

    let mut coverage = 0u64;
    for y in min_y..=max_y {
        let py = y as f32 + 0.5;
        let mut e0 = edge(v[1].0, v[1].1, v[2].0, v[2].1, start_x, py);
        let mut e1 = edge(v[2].0, v[2].1, v[0].0, v[0].1, start_x, py);
        let mut e2 = edge(v[0].0, v[0].1, v[1].0, v[1].1, start_x, py);

        let row_start = y as usize * fb_width;
        let row = &mut fb[row_start + min_x..=row_start + max_x];
        let mut entered = false;
        for dst in row.iter_mut() {
            let cur_e0 = e0;
            let cur_e1 = e1;
            let cur_e2 = e2;
            e0 += dy12;
            e1 += dy20;
            e2 += dy01;

            let inside = (cur_e0 > 0.0 || (cur_e0 == 0.0 && tl12))
                && (cur_e1 > 0.0 || (cur_e1 == 0.0 && tl20))
                && (cur_e2 > 0.0 || (cur_e2 == 0.0 && tl01));
            if !inside {
                // Each edge value moves one way along the row, so the covered
                // pixels are one run: nothing follows once it has ended.
                if entered {
                    break;
                }
                continue;
            }
            entered = true;
            let w0 = cur_e0 * inv_area;
            let w1 = cur_e1 * inv_area;
            let w2 = cur_e2 * inv_area;
            let u = v[0].2 * w0 + v[1].2 * w1 + v[2].2 * w2;
            let vv = v[0].3 * w0 + v[1].3 * w1 + v[2].3 * w2;
            let sample = if is_one_to_one {
                sample_nearest(tex, u, vv)
            } else {
                sample_bilinear_premultiplied(tex, u, vv)
            };
            coverage += 1;
            if sample.a == 0 {
                // Modulating never raises alpha, so nothing is drawn.
                continue;
            }
            let src = match &colors {
                Some(colors) => {
                    // GL_MODULATE multiplies texture alpha by the primary
                    // colour alpha. Vortex fades its ball trail and the
                    // TWA engine its text this way.
                    let primary = match flat {
                        Some((primary, tolerance)) if (w0 + w1 + w2 - 1.0).abs() <= tolerance => {
                            primary
                        }
                        _ => {
                            let channel = |component: usize| -> u8 {
                                let value = (colors[0][component] * w0
                                    + colors[1][component] * w1
                                    + colors[2][component] * w2)
                                    .clamp(0.0, 1.0);
                                round_to_u8(value * 255.0)
                            };
                            Rgba8::rgba(channel(0), channel(1), channel(2), channel(3))
                        }
                    };
                    modulate(modulate(sample, primary), tint)
                }
                None => modulate(sample, tint),
            };
            if src.a > 0 {
                *dst = blend_src_over(*dst, src);
            }
        }
    }

    coverage
}

pub fn rasterize_solid_quad(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    color: Rgba8,
    positions: &[(f32, f32); 4],
) -> u64 {
    if color.a == 0 {
        return 0;
    }
    let min_x = positions
        .iter()
        .map(|p| p.0)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as i32;
    let min_y = positions
        .iter()
        .map(|p| p.1)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as i32;
    let max_x = positions
        .iter()
        .map(|p| p.0)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min((fb_width - 1) as f32) as i32;
    let max_y = positions
        .iter()
        .map(|p| p.1)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min((fb_height - 1) as f32) as i32;
    if min_x > max_x || min_y > max_y {
        return 0;
    }

    let mut coverage = 0u64;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let idx = y as usize * fb_width + x as usize;
            fb[idx] = blend_src_over(fb[idx], color);
            coverage += 1;
        }
    }
    coverage
}

pub fn rasterize_quad_tinted(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    tex: &Texture,
    positions: &[(f32, f32); 4],
    uvs: &[(f32, f32); 4],
    tint: Rgba8,
) -> u64 {
    rasterize_quad_tinted_with_vertex_colors(fb, fb_width, fb_height, tex, positions, uvs, tint, None)
}

/// Rasterize a textured quad with an optional four-vertex primary colour
/// array. The two triangles share the same colour values so interpolation is
/// continuous across the quad diagonal.
pub fn rasterize_quad_tinted_with_vertex_colors(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    tex: &Texture,
    positions: &[(f32, f32); 4],
    uvs: &[(f32, f32); 4],
    tint: Rgba8,
    vertex_colors: Option<&[[f32; 4]; 4]>,
) -> u64 {
    let tri0 = [
        (positions[0].0, positions[0].1, uvs[0].0, uvs[0].1),
        (positions[1].0, positions[1].1, uvs[1].0, uvs[1].1),
        (positions[2].0, positions[2].1, uvs[2].0, uvs[2].1),
    ];
    let tri1 = [
        (positions[0].0, positions[0].1, uvs[0].0, uvs[0].1),
        (positions[2].0, positions[2].1, uvs[2].0, uvs[2].1),
        (positions[3].0, positions[3].1, uvs[3].0, uvs[3].1),
    ];
    let tri0_colors = vertex_colors.map(|colors| [colors[0], colors[1], colors[2]]);
    let tri1_colors = vertex_colors.map(|colors| [colors[0], colors[2], colors[3]]);
    let coverage0 = rasterize_triangle_tinted_with_vertex_colors(
        fb,
        fb_width,
        fb_height,
        tex,
        &tri0,
        tint,
        tri0_colors.as_ref(),
    );
    let coverage1 = rasterize_triangle_tinted_with_vertex_colors(
        fb,
        fb_width,
        fb_height,
        tex,
        &tri1,
        tint,
        tri1_colors.as_ref(),
    );
    coverage0 + coverage1
}

pub fn rasterize_triangle(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    tex: &Texture,
    verts: &[(f32, f32, f32, f32); 3],
) -> u64 {
    rasterize_triangle_tinted(
        fb,
        fb_width,
        fb_height,
        tex,
        verts,
        Rgba8::rgba(255, 255, 255, 255),
    )
}

pub fn rasterize_quad(
    fb: &mut [Rgba8],
    fb_width: usize,
    fb_height: usize,
    tex: &Texture,
    positions: &[(f32, f32); 4],
    uvs: &[(f32, f32); 4],
) -> u64 {
    rasterize_quad_tinted(
        fb,
        fb_width,
        fb_height,
        tex,
        positions,
        uvs,
        Rgba8::rgba(255, 255, 255, 255),
    )
}

pub fn framebuffer_hash(fb: &[Rgba8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for px in fb {
        for b in [px.r, px.g, px.b, px.a] {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

pub fn framebuffer_to_ppm(path: &std::path::Path, fb: &[Rgba8], width: usize, height: usize) {
    let mut out = Vec::with_capacity(width * height * 3 + 64);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", width, height).as_bytes());
    for px in fb {
        out.push(px.r);
        out.push(px.g);
        out.push(px.b);
    }
    std::fs::write(path, out).expect("write ppm");
}

/// The rasterizer as it was before the inner loops were optimized. The fast
/// versions must match it pixel for pixel (see `fast_paths_match_reference`).
#[cfg(test)]
mod reference {
    use super::{edge, is_top_left, modulate, one_to_one_mapping, sample_nearest, Rgba8, Texture};

    /// Sample at texel centres using the filtering behavior observed in the PR
    /// direct runner. RGB is interpolated in premultiplied-alpha space so keyed
    /// transparent texels do not bleed their hidden colour into scaled edges.
    pub fn sample_bilinear_premultiplied(texture: &Texture, u: f32, v: f32) -> Rgba8 {
        if texture.width == 0 || texture.height == 0 {
            return Rgba8::rgba(0, 0, 0, 0);
        }
        let sx = (u - 0.5).clamp(0.0, (texture.width - 1) as f32);
        let sy = (v - 0.5).clamp(0.0, (texture.height - 1) as f32);
        let x0 = sx.floor() as usize;
        let y0 = sy.floor() as usize;
        let x1 = (x0 + 1).min(texture.width - 1);
        let y1 = (y0 + 1).min(texture.height - 1);
        let dx = sx - x0 as f32;
        let dy = sy - y0 as f32;
        let weights = [
            ((1.0 - dx) * (1.0 - dy), x0, y0),
            (dx * (1.0 - dy), x1, y0),
            ((1.0 - dx) * dy, x0, y1),
            (dx * dy, x1, y1),
        ];
        let mut alpha_sum = 0.0;
        let mut channel_sum = [0.0; 3];
        for (weight, x, y) in weights {
            let pixel = texture.pixels[y * texture.width + x];
            let alpha = pixel.a as f32;
            alpha_sum += weight * alpha;
            for (channel, sum) in channel_sum.iter_mut().enumerate() {
                let value = [pixel.r, pixel.g, pixel.b][channel] as f32;
                *sum += weight * alpha * value;
            }
        }
        let alpha = alpha_sum.round().clamp(0.0, 255.0) as u8;
        if alpha_sum <= 0.0 {
            return Rgba8::rgba(0, 0, 0, alpha);
        }
        let channel = |sum: f32| (sum / alpha_sum).round().clamp(0.0, 255.0) as u8;
        Rgba8::rgba(
            channel(channel_sum[0]),
            channel(channel_sum[1]),
            channel(channel_sum[2]),
            alpha,
        )
    }

    pub fn blend_src_over(dst: Rgba8, src: Rgba8) -> Rgba8 {
        if src.a == 0 {
            return dst;
        }
        if src.a == 255 {
            return src;
        }
        let sa = src.a as u32;
        let da = dst.a as u32;
        let inv_sa = 255 - sa;
        let out_a = sa + (da * inv_sa + 127) / 255;
        if out_a == 0 {
            return Rgba8::rgba(0, 0, 0, 0);
        }
        let blend = |src_c: u8, dst_c: u8| -> u8 {
            let src_p = src_c as u32 * sa;
            let dst_p = dst_c as u32 * da * inv_sa / 255;
            ((src_p + dst_p + out_a / 2) / out_a) as u8
        };
        Rgba8::rgba(
            blend(src.r, dst.r),
            blend(src.g, dst.g),
            blend(src.b, dst.b),
            out_a as u8,
        )
    }

    /// Rasterize a textured triangle while optionally interpolating a primary
    /// colour supplied by the guest's GL colour array. The direct PR runner keeps
    /// this colour separate from the constant colour register; Vortex uses it to
    /// turn one greyscale ring atlas into the coloured attract-mode tiles.
    pub fn rasterize_triangle_tinted_with_vertex_colors(
        fb: &mut [Rgba8],
        fb_width: usize,
        fb_height: usize,
        tex: &Texture,
        verts: &[(f32, f32, f32, f32); 3],
        tint: Rgba8,
        vertex_colors: Option<&[[f32; 4]; 3]>,
    ) -> u64 {
        let mut v = *verts;
        let mut colors = vertex_colors.copied();
        if edge(v[0].0, v[0].1, v[1].0, v[1].1, v[2].0, v[2].1) < 0.0 {
            v.swap(1, 2);
            if let Some(colors) = colors.as_mut() {
                colors.swap(1, 2);
            }
        }

        let (mut min_x_f, mut max_x_f) = (v[0].0, v[0].0);
        let (mut min_y_f, mut max_y_f) = (v[0].1, v[0].1);
        for p in &v[1..] {
            if p.0 < min_x_f { min_x_f = p.0; } else if p.0 > max_x_f { max_x_f = p.0; }
            if p.1 < min_y_f { min_y_f = p.1; } else if p.1 > max_y_f { max_y_f = p.1; }
        }
        let min_x = min_x_f.floor().max(0.0) as i32;
        let min_y = min_y_f.floor().max(0.0) as i32;
        let max_x = max_x_f.ceil().min((fb_width - 1) as f32) as i32;
        let max_y = max_y_f.ceil().min((fb_height - 1) as f32) as i32;
        if min_x > max_x || min_y > max_y {
            return 0;
        }

        let area = edge(v[0].0, v[0].1, v[1].0, v[1].1, v[2].0, v[2].1);
        if area == 0.0 {
            return 0;
        }

        let tl01 = is_top_left(v[0].0, v[0].1, v[1].0, v[1].1);
        let tl12 = is_top_left(v[1].0, v[1].1, v[2].0, v[2].1);
        let tl20 = is_top_left(v[2].0, v[2].1, v[0].0, v[0].1);

        let is_one_to_one = one_to_one_mapping(&v);
        let inv_area = 1.0 / area;
        let dy12 = v[2].1 - v[1].1;
        let dy20 = v[0].1 - v[2].1;
        let dy01 = v[1].1 - v[0].1;

        let mut coverage = 0u64;
        for y in min_y..=max_y {
            let py = y as f32 + 0.5;
            let mut e0 = edge(v[1].0, v[1].1, v[2].0, v[2].1, min_x as f32 + 0.5, py);
            let mut e1 = edge(v[2].0, v[2].1, v[0].0, v[0].1, min_x as f32 + 0.5, py);
            let mut e2 = edge(v[0].0, v[0].1, v[1].0, v[1].1, min_x as f32 + 0.5, py);

            for x in min_x..=max_x {
                let cur_e0 = e0;
                let cur_e1 = e1;
                let cur_e2 = e2;
                e0 += dy12;
                e1 += dy20;
                e2 += dy01;

                let inside = (cur_e0 > 0.0 || (cur_e0 == 0.0 && tl12))
                    && (cur_e1 > 0.0 || (cur_e1 == 0.0 && tl20))
                    && (cur_e2 > 0.0 || (cur_e2 == 0.0 && tl01));
                if !inside {
                    continue;
                }
                let w0 = cur_e0 * inv_area;
                let w1 = cur_e1 * inv_area;
                let w2 = cur_e2 * inv_area;
                let u = v[0].2 * w0 + v[1].2 * w1 + v[2].2 * w2;
                let vv = v[0].3 * w0 + v[1].3 * w1 + v[2].3 * w2;
                let sample = if is_one_to_one {
                    sample_nearest(tex, u, vv)
                } else {
                    sample_bilinear_premultiplied(tex, u, vv)
                };
                let src = match colors {
                    Some(colors) => {
                        let w0 = cur_e0 * inv_area;
                        let w1 = cur_e1 * inv_area;
                        let w2 = cur_e2 * inv_area;
                        let mut out = [0.0; 4];
                        for component in 0..4 {
                            out[component] = (colors[0][component] * w0
                                + colors[1][component] * w1
                                + colors[2][component] * w2)
                                .clamp(0.0, 1.0);
                        }
                        let primary = Rgba8::rgba(
                            (out[0] * 255.0).round() as u8,
                            (out[1] * 255.0).round() as u8,
                            (out[2] * 255.0).round() as u8,
                            // GL_MODULATE multiplies texture alpha by the primary
                            // colour alpha. Vortex fades its ball trail and the
                            // TWA engine its text this way.
                            (out[3] * 255.0).round() as u8,
                        );
                        modulate(modulate(sample, primary), tint)
                    }
                    None => modulate(sample, tint),
                };
                let idx = y as usize * fb_width + x as usize;
                if src.a > 0 {
                    fb[idx] = blend_src_over(fb[idx], src);
                }
                coverage += 1;
            }
        }

        coverage
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            (self.next() >> 11) % n
        }

        fn unit(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1u64 << 24) as f32
        }

        fn byte(&mut self) -> u8 {
            // Favour the values with special paths: 0 and 255.
            match self.below(4) {
                0 => 0,
                1 => 255,
                _ => self.below(256) as u8,
            }
        }

        fn color(&mut self) -> Rgba8 {
            Rgba8::rgba(self.byte(), self.byte(), self.byte(), self.byte())
        }

        /// A coordinate in `-margin..size + margin`, on whole pixels, half
        /// pixels or anywhere.
        fn coord(&mut self, size: f32, margin: f32) -> f32 {
            let value = self.unit() * (size + 2.0 * margin) - margin;
            match self.below(3) {
                0 => value.round(),
                1 => (value * 2.0).round() / 2.0,
                _ => value,
            }
        }
    }

    fn random_texture(rng: &mut XorShift) -> Texture {
        let width = 1 + rng.below(24) as usize;
        let height = 1 + rng.below(24) as usize;
        // Flat, blocky and noisy textures: the flat-area filter path needs
        // neighbouring texels that are equal.
        let block = 1 + rng.below(6) as usize;
        let palette: Vec<Rgba8> = (0..1 + rng.below(5)).map(|_| rng.color()).collect();
        let cells_x = width.div_ceil(block);
        let cells: Vec<Rgba8> = (0..cells_x * height.div_ceil(block))
            .map(|_| palette[rng.below(palette.len() as u64) as usize])
            .collect();
        let pixels = (0..width * height)
            .map(|index| cells[(index / width / block) * cells_x + (index % width) / block])
            .collect();
        Texture { width, height, pixels }
    }

    #[test]
    fn round_to_u8_matches_round_and_clamp() {
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        let check = |x: f32| {
            assert_eq!(round_to_u8(x), x.round().clamp(0.0, 255.0) as u8, "x={x}");
        };
        for x in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.0,
            0.49999997,
            0.5,
            254.5,
            254.49998,
            255.0,
            255.5,
            1.0e9,
            -1.0e9,
            f32::MAX,
        ] {
            check(x);
        }
        for step in 0..=(256 * 64) {
            let x = step as f32 / 64.0;
            check(x);
            check(f32::from_bits(x.to_bits().wrapping_sub(1)));
            check(f32::from_bits(x.to_bits() + 1));
        }
        for _ in 0..200_000 {
            check(rng.unit() * 300.0 - 20.0);
        }
    }

    #[test]
    fn blend_matches_reference() {
        let mut rng = XorShift(0x2545_f491_4f6c_dd1d);
        for _ in 0..400_000 {
            let (dst, src) = (rng.color(), rng.color());
            assert_eq!(
                blend_src_over(dst, src),
                reference::blend_src_over(dst, src),
                "dst={dst:?} src={src:?}"
            );
        }
    }

    #[test]
    fn bilinear_matches_reference() {
        let mut rng = XorShift(0x1234_5678_9abc_def1);
        for _ in 0..400 {
            let tex = random_texture(&mut rng);
            for _ in 0..500 {
                let u = rng.coord(tex.width as f32, 2.0);
                let v = rng.coord(tex.height as f32, 2.0);
                assert_eq!(
                    sample_bilinear_premultiplied(&tex, u, v),
                    reference::sample_bilinear_premultiplied(&tex, u, v),
                    "u={u} v={v} tex={}x{}",
                    tex.width,
                    tex.height
                );
            }
            for (u, v) in [(f32::NAN, 0.5), (0.5, f32::NAN), (f32::INFINITY, f32::NEG_INFINITY)] {
                assert_eq!(
                    sample_bilinear_premultiplied(&tex, u, v),
                    reference::sample_bilinear_premultiplied(&tex, u, v)
                );
            }
        }
    }

    #[test]
    fn fast_paths_match_reference() {
        const W: usize = 48;
        const H: usize = 40;
        let mut rng = XorShift(0xdead_beef_cafe_f00d);
        for case in 0..120000 {
            let tex = random_texture(&mut rng);
            let background = if rng.below(2) == 0 {
                (0..W * H).map(|_| rng.color()).collect::<Vec<_>>()
            } else {
                let alpha = if rng.below(3) == 0 { rng.byte() } else { 255 };
                (0..W * H)
                    .map(|_| Rgba8::rgba(rng.byte(), rng.byte(), rng.byte(), alpha))
                    .collect()
            };
            let tint = if rng.below(2) == 0 { Rgba8::rgba(255, 255, 255, 255) } else { rng.color() };
            let colors = match rng.below(5) {
                0 => None,
                1 => Some([[1.0f32; 4]; 3]),
                // One colour for the whole triangle: byte values, as the
                // games pass them, values on a rounding boundary and any.
                2 | 3 => Some(
                    [[(); 4].map(|_| match rng.below(6) {
                        0 => 0.0,
                        1 => 1.0,
                        2 => rng.below(256) as f32 / 255.0,
                        3 => (rng.below(255) as f32 + 0.5) / 255.0,
                        4 => rng.below(65537) as f32 / 65536.0,
                        _ => rng.unit() * 1.2 - 0.1,
                    }); 3],
                ),
                // Slightly out of range, so the clamp is exercised.
                _ => Some([(); 3].map(|_| [(); 4].map(|_| rng.unit() * 1.2 - 0.1))),
            };
            let shape = rng.below(8);
            let verts = if shape < 3 {
                // Half of an axis-aligned rectangle mapped one to one, as
                // sprites are drawn: plain, mirrored, and with the texture
                // offset by part of a texel (on or next to a texel boundary
                // the general loop decides).
                let (x0, y0) = (rng.coord(W as f32, 4.0).round(), rng.coord(H as f32, 4.0).round());
                let (w, h) = (1.0 + rng.below(30) as f32, 1.0 + rng.below(30) as f32);
                let offset = |rng: &mut XorShift| match rng.below(8) {
                    0 => 0.5,
                    1 => 0.25,
                    2 => 1.0 / 128.0,
                    3 => 1.0 - 1.0 / 128.0,
                    4 => rng.unit(),
                    _ => 0.0,
                };
                let u0 = rng.below(8) as f32 - 2.0 + offset(&mut rng);
                let v0 = rng.below(8) as f32 - 2.0 + offset(&mut rng);
                let (su, sv) = (
                    if rng.below(4) == 0 { -1.0 } else { 1.0 },
                    if rng.below(4) == 0 { -1.0 } else { 1.0 },
                );
                let corner = |cx: f32, cy: f32| (x0 + cx * w, y0 + cy * h, u0 + su * cx * w, v0 + sv * cy * h);
                match rng.below(4) {
                    0 => [corner(0.0, 0.0), corner(1.0, 0.0), corner(1.0, 1.0)],
                    1 => [corner(0.0, 0.0), corner(1.0, 1.0), corner(0.0, 1.0)],
                    2 => [corner(0.0, 0.0), corner(1.0, 0.0), corner(0.0, 1.0)],
                    _ => [corner(1.0, 0.0), corner(0.0, 1.0), corner(1.0, 1.0)],
                }
            } else if shape == 3 {
                // Any triangle with whole-pixel vertices copying texels 1:1.
                let (u0, v0) = (rng.below(6) as f32, rng.below(6) as f32);
                [(); 3].map(|_| {
                    let (x, y) = (rng.coord(W as f32, 6.0).round(), rng.coord(H as f32, 6.0).round());
                    (x, y, x + u0, y + v0)
                })
            } else if shape == 4 {
                // A solid fill: every vertex has the same texture coordinate.
                let (u, v) = (rng.coord(tex.width as f32, 1.0), rng.coord(tex.height as f32, 1.0));
                [(); 3].map(|_| (rng.coord(W as f32, 6.0).round(), rng.coord(H as f32, 6.0).round(), u, v))
            } else if rng.below(4) == 0 {
                // Small and sliver triangles, where the weights are noisiest.
                let (cx, cy) = (rng.coord(W as f32, 0.0), rng.coord(H as f32, 0.0));
                let (sx, sy) = (rng.unit() * 6.0, rng.unit() * 0.5 + 0.01);
                [(); 3].map(|_| {
                    (
                        cx + (rng.unit() - 0.5) * sx * 40.0,
                        cy + (rng.unit() - 0.5) * sy * 40.0,
                        rng.coord(tex.width as f32, 1.0),
                        rng.coord(tex.height as f32, 1.0),
                    )
                })
            } else {
                [(); 3].map(|_| {
                    (
                        rng.coord(W as f32, 6.0),
                        rng.coord(H as f32, 6.0),
                        rng.coord(tex.width as f32, 1.0),
                        rng.coord(tex.height as f32, 1.0),
                    )
                })
            };

            let mut fast = background.clone();
            let mut slow = background;
            let fast_coverage = rasterize_triangle_tinted_with_vertex_colors(
                &mut fast, W, H, &tex, &verts, tint, colors.as_ref(),
            );
            let slow_coverage = reference::rasterize_triangle_tinted_with_vertex_colors(
                &mut slow, W, H, &tex, &verts, tint, colors.as_ref(),
            );
            assert_eq!(fast_coverage, slow_coverage, "case {case} verts={verts:?}");
            if let Some(index) = (0..W * H).find(|&index| fast[index] != slow[index]) {
                panic!(
                    "case {case}: pixel ({}, {}) is {:?}, reference {:?}; verts={verts:?} tint={tint:?} colors={colors:?}",
                    index % W,
                    index / W,
                    fast[index],
                    slow[index]
                );
            }
        }
    }

    #[test]
    fn decode_rgba8888_preserves_channel_order() {
        let tex = Texture::from_bytes(
            &[0x11, 0x22, 0x33, 0x44],
            1,
            1,
            TextureFormat::Rgba8888,
            Rgba8::rgba(255, 255, 255, 255),
        );
        assert_eq!(tex.pixels[0], Rgba8::rgba(0x11, 0x22, 0x33, 0x44));
    }

    #[test]
    fn rasterize_quad_tinted_modulates_rgb_and_alpha() {
        let tex = Texture::from_bytes(
            &[0xff],
            1,
            1,
            TextureFormat::A8,
            Rgba8::rgba(255, 255, 255, 255),
        );
        let mut fb = vec![Rgba8::rgba(0, 0, 0, 0)];
        let cov = rasterize_quad_tinted(
            &mut fb,
            1,
            1,
            &tex,
            &[(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)],
            &[(0.0, 0.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)],
            Rgba8::rgba(64, 128, 255, 128),
        );
        assert_eq!(cov, 1);
        assert_eq!(fb[0], Rgba8::rgba(64, 128, 255, 128));
    }

    #[test]
    fn textured_vertex_alpha_modulates_texture_alpha() {
        let tex = Texture::from_bytes(
            &[0x00, 0xff, 0x00, 0xff],
            1,
            1,
            TextureFormat::Rgba8888,
            Rgba8::rgba(255, 255, 255, 255),
        );
        let mut fb = vec![Rgba8::rgba(255, 0, 0, 255)];
        let colors = [[1.0, 1.0, 1.0, 0.25]; 4];
        let cov = rasterize_quad_tinted_with_vertex_colors(
            &mut fb,
            1,
            1,
            &tex,
            &[(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)],
            &[(0.0, 0.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)],
            Rgba8::rgba(255, 255, 255, 255),
            Some(&colors),
        );
        assert_eq!(cov, 1);
        // A quarter of the green texel over the opaque red background.
        assert_eq!(fb[0], Rgba8::rgba(191, 64, 0, 255));
    }

    #[test]
    fn bilinear_sampling_weights_rgb_by_alpha() {
        let tex = Texture::from_bytes(
            &[255, 0, 0, 255, 0, 255, 0, 0],
            2,
            1,
            TextureFormat::Rgba8888,
            Rgba8::rgba(255, 255, 255, 255),
        );
        assert_eq!(
            sample_bilinear_premultiplied(&tex, 0.5, 0.5),
            Rgba8::rgba(255, 0, 0, 255)
        );
        assert_eq!(
            sample_bilinear_premultiplied(&tex, 1.25, 0.5),
            Rgba8::rgba(255, 0, 0, 64)
        );
    }

    #[test]
    fn scaled_quad_uses_bilinear_but_one_to_one_stays_nearest() {
        let tex = Texture::from_bytes(
            &[255, 0, 0, 255, 0, 0, 255, 255],
            2,
            1,
            TextureFormat::Rgba8888,
            Rgba8::rgba(255, 255, 255, 255),
        );
        let scaled = [
            (0.0, 0.0, 0.0, 0.0),
            (4.0, 0.0, 2.0, 0.0),
            (4.0, 1.0, 2.0, 1.0),
        ];
        let one_to_one = [
            (0.0, 0.0, 0.0, 0.0),
            (2.0, 0.0, 2.0, 0.0),
            (2.0, 1.0, 2.0, 1.0),
        ];
        assert!(!one_to_one_mapping(&scaled));
        assert!(one_to_one_mapping(&one_to_one));
        assert_eq!(sample_nearest(&tex, 0.75, 0.5), Rgba8::rgba(255, 0, 0, 255));
        assert_eq!(
            sample_bilinear_premultiplied(&tex, 0.75, 0.5),
            Rgba8::rgba(191, 0, 64, 255)
        );
    }

    #[test]
    fn manifest_ipd_decodes_rgb565_pixels() {
        let mut data = vec![0u8; 18];
        data[0..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..8].copy_from_slice(&1u32.to_le_bytes());
        data[16..18].copy_from_slice(&0xf800u16.to_le_bytes());
        let decoded = decode_manifest_ipd(&data).expect("valid ipd");
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(decoded.format, TextureFormat::Rgb565);
        assert_eq!(decoded.pixels[0], Rgba8::rgba(255, 0, 0, 255));
    }

    #[test]
    fn live_packed_colour_expansion_rounds_like_gl_uploads() {
        // Red=3 and green=11 are the first packed values where truncation
        // differs from the PR runner's nearest-integer expansion.
        let raw = 0x19_6bu16.to_le_bytes();
        let pixels = Texture::from_live_gl_bytes(
            &raw,
            1,
            1,
            TextureFormat::Rgb565,
            Rgba8::rgba(255, 255, 255, 255),
        )
        .pixels;
        assert_eq!(pixels[0], Rgba8::rgba(25, 45, 90, 255));
    }

    #[test]
    fn manifest_tga_applies_magenta_colour_key() {
        let mut data = vec![0u8; 20];
        data[2] = 2;
        data[12..14].copy_from_slice(&1u16.to_le_bytes());
        data[14..16].copy_from_slice(&1u16.to_le_bytes());
        data[16] = 16;
        data[18..20].copy_from_slice(&0xf83eu16.to_le_bytes());
        let decoded = decode_manifest_tga(&data).expect("valid tga");
        assert_eq!(decoded.pixels[0].a, 0);
    }

    #[test]
    fn manifest_bmp_recognizes_a8_coverage_palette() {
        let mut data = vec![0u8; 66];
        data[0..2].copy_from_slice(b"BM");
        data[10..14].copy_from_slice(&62u32.to_le_bytes());
        data[14..18].copy_from_slice(&40u32.to_le_bytes());
        data[18..22].copy_from_slice(&1i32.to_le_bytes());
        data[22..26].copy_from_slice(&1i32.to_le_bytes());
        data[26..28].copy_from_slice(&1u16.to_le_bytes());
        data[28..30].copy_from_slice(&8u16.to_le_bytes());
        data[46..50].copy_from_slice(&2u32.to_le_bytes());
        data[54..58].copy_from_slice(&[0, 0, 0, 0]);
        data[58..62].copy_from_slice(&[1, 1, 1, 0]);
        data[62] = 1;
        let decoded = decode_manifest_bmp(&data).expect("valid bmp");
        assert_eq!(decoded.format, TextureFormat::A8);
        assert_eq!(decoded.pixels[0], Rgba8::rgba(255, 255, 255, 1));
    }
}
