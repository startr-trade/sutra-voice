//! A baseline JPEG encoder for I420 frames.
//!
//! An I420 frame is already what a baseline JFIF image with 4:2:0 chroma subsampling holds —
//! one full-resolution luma plane and two half-resolution chroma planes — so the frame goes
//! straight into the encoder with no colour conversion beyond expanding the video range
//! (luma 16-235, chroma 16-240) to the full range JFIF assumes. Sequential DCT, 8-bit
//! samples, the example quantisation and Huffman tables of ITU-T T.81 Annex K. Written here
//! rather than taken as a dependency to keep the worker's licences to MIT / Apache / BSD.

/// The zigzag order: `ZIGZAG[i]` is the natural (row-major) index of the i-th coefficient.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// T.81 Table K.1 (luminance), natural order.
const LUMA_QUANT: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// T.81 Table K.2 (chrominance), natural order.
const CHROMA_QUANT: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// One I420 frame, borrowed: three planes and their strides.
#[derive(Debug, Clone, Copy)]
pub struct I420<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub stride_y: u32,
    pub u: &'a [u8],
    pub stride_u: u32,
    pub v: &'a [u8],
    pub stride_v: u32,
}

/// Why a frame could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpegError {
    /// Zero or more than 65535 pixels in a dimension.
    Dimensions,
    /// A plane is shorter than its stride and the frame's size need.
    PlaneTooShort,
}

impl std::fmt::Display for JpegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JpegError::Dimensions => f.write_str("frame dimensions out of range"),
            JpegError::PlaneTooShort => f.write_str("a plane is shorter than the frame needs"),
        }
    }
}

impl std::error::Error for JpegError {}

/// The quantisation table for `quality` (1-100), in zigzag order, scaled the usual way
/// (50 = the Annex K table; lower is coarser).
fn scaled_table(base: &[u16; 64], quality: u8) -> [u8; 64] {
    let quality = u32::from(quality.clamp(1, 100));
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    let mut out = [0u8; 64];
    for (i, slot) in out.iter_mut().enumerate() {
        let value = (u32::from(base[ZIGZAG[i]]) * scale + 50) / 100;
        *slot = value.clamp(1, 255) as u8;
    }
    out
}

/// A canonical Huffman code table: `(code, length)` per symbol.
struct Huffman {
    codes: [(u16, u8); 256],
}

impl Huffman {
    fn new(bits: &[u8; 16], values: &[u8]) -> Huffman {
        let mut codes = [(0u16, 0u8); 256];
        let mut code: u16 = 0;
        let mut k = 0;
        for (length_index, &count) in bits.iter().enumerate() {
            for _ in 0..count {
                codes[usize::from(values[k])] = (code, length_index as u8 + 1);
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Huffman { codes }
    }
}

struct BitWriter {
    out: Vec<u8>,
    buffer: u32,
    count: u32,
}

impl BitWriter {
    fn write(&mut self, bits: u16, length: u8) {
        if length == 0 {
            return;
        }
        let length = u32::from(length);
        self.buffer = (self.buffer << length) | (u32::from(bits) & ((1 << length) - 1));
        self.count += length;
        while self.count >= 8 {
            let byte = (self.buffer >> (self.count - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
            self.count -= 8;
        }
        self.buffer &= (1 << self.count) - 1;
    }

    /// Pad the last byte with one-bits.
    fn flush(&mut self) {
        if self.count > 0 {
            let pad = 8 - self.count;
            self.write((1 << pad) - 1, pad as u8);
        }
    }
}

/// The bit category of `value` and its bits as the entropy coder writes them.
fn category(value: i32) -> (u8, u16) {
    let magnitude = value.unsigned_abs();
    let size = (32 - magnitude.leading_zeros()) as u8;
    let bits = if value < 0 {
        (value - 1) as u32 & ((1u32 << size) - 1)
    } else {
        value as u32
    };
    (size, bits as u16)
}

/// The cosine table of the forward DCT: `COS[x][u] = c(u) * cos((2x+1)uπ/16) / 2`.
fn cos_table() -> [[f32; 8]; 8] {
    let mut table = [[0f32; 8]; 8];
    for (x, row) in table.iter_mut().enumerate() {
        for (u, slot) in row.iter_mut().enumerate() {
            let c = if u == 0 {
                std::f32::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            *slot =
                c * ((2.0 * x as f32 + 1.0) * u as f32 * std::f32::consts::PI / 16.0).cos() / 2.0;
        }
    }
    table
}

struct Encoder {
    cos: [[f32; 8]; 8],
    writer: BitWriter,
}

impl Encoder {
    /// Transform, quantise and entropy-code one 8x8 block (samples level-shifted).
    fn block(
        &mut self,
        samples: &[f32; 64],
        quant: &[u8; 64],
        previous_dc: &mut i32,
        dc: &Huffman,
        ac: &Huffman,
    ) {
        let mut rows = [0f32; 64];
        for y in 0..8 {
            for u in 0..8 {
                let mut sum = 0.0;
                for x in 0..8 {
                    sum += samples[y * 8 + x] * self.cos[x][u];
                }
                rows[y * 8 + u] = sum;
            }
        }
        let mut coefficients = [0i32; 64];
        for (i, coefficient) in coefficients.iter_mut().enumerate() {
            let natural = ZIGZAG[i];
            let (v, u) = (natural / 8, natural % 8);
            let mut sum = 0.0;
            for y in 0..8 {
                sum += rows[y * 8 + u] * self.cos[y][v];
            }
            *coefficient = (sum / f32::from(quant[i])).round() as i32;
        }
        let diff = coefficients[0] - *previous_dc;
        *previous_dc = coefficients[0];
        let (size, bits) = category(diff);
        let (code, length) = dc.codes[usize::from(size)];
        self.writer.write(code, length);
        self.writer.write(bits, size);
        let mut run = 0;
        for &coefficient in &coefficients[1..] {
            if coefficient == 0 {
                run += 1;
                continue;
            }
            while run >= 16 {
                let (code, length) = ac.codes[0xF0];
                self.writer.write(code, length);
                run -= 16;
            }
            let (size, bits) = category(coefficient);
            let (code, length) = ac.codes[usize::from((run << 4) as u8 | size)];
            self.writer.write(code, length);
            self.writer.write(bits, size);
            run = 0;
        }
        if run > 0 {
            let (code, length) = ac.codes[0x00];
            self.writer.write(code, length);
        }
    }
}

fn plane_ok(plane: &[u8], stride: u32, width: u32, height: u32) -> bool {
    stride >= width
        && (plane.len() as u64)
            >= u64::from(stride) * u64::from(height.saturating_sub(1)) + u64::from(width)
}

/// Fetch a sample with edge clamping, expanding video range to full range.
fn sample(plane: &[u8], stride: u32, width: u32, height: u32, x: u32, y: u32, luma: bool) -> f32 {
    let x = x.min(width - 1);
    let y = y.min(height - 1);
    let raw = f32::from(plane[(y * stride + x) as usize]);
    let full = if luma {
        (raw - 16.0) * 255.0 / 219.0
    } else {
        (raw - 128.0) * 255.0 / 224.0 + 128.0
    };
    full.clamp(0.0, 255.0) - 128.0
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// Encode `frame` as a baseline JFIF image at `quality` (1-100).
pub fn encode_i420(frame: &I420<'_>, quality: u8) -> Result<Vec<u8>, JpegError> {
    let (width, height) = (frame.width, frame.height);
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return Err(JpegError::Dimensions);
    }
    let (chroma_w, chroma_h) = (width.div_ceil(2), height.div_ceil(2));
    if !plane_ok(frame.y, frame.stride_y, width, height)
        || !plane_ok(frame.u, frame.stride_u, chroma_w, chroma_h)
        || !plane_ok(frame.v, frame.stride_v, chroma_w, chroma_h)
    {
        return Err(JpegError::PlaneTooShort);
    }
    let luma_q = scaled_table(&LUMA_QUANT, quality);
    let chroma_q = scaled_table(&CHROMA_QUANT, quality);
    let mut out = Vec::with_capacity((width * height / 4) as usize + 1024);
    out.extend_from_slice(&[0xFF, 0xD8]);
    segment(
        &mut out,
        0xE0,
        &[b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0],
    );
    let mut dqt = vec![0x00];
    dqt.extend_from_slice(&luma_q);
    dqt.push(0x01);
    dqt.extend_from_slice(&chroma_q);
    segment(&mut out, 0xDB, &dqt);
    let (h, w) = ((height as u16).to_be_bytes(), (width as u16).to_be_bytes());
    segment(
        &mut out,
        0xC0,
        &[
            8, h[0], h[1], w[0], w[1], 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1,
        ],
    );
    let mut dht = Vec::new();
    for (class_id, bits, values) in [
        (0x00u8, &DC_LUMA_BITS, &DC_VALUES[..]),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALUES[..]),
        (0x01, &DC_CHROMA_BITS, &DC_VALUES[..]),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALUES[..]),
    ] {
        dht.push(class_id);
        dht.extend_from_slice(bits);
        dht.extend_from_slice(values);
    }
    segment(&mut out, 0xC4, &dht);
    segment(&mut out, 0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);

    let dc_luma = Huffman::new(&DC_LUMA_BITS, &DC_VALUES);
    let ac_luma = Huffman::new(&AC_LUMA_BITS, &AC_LUMA_VALUES);
    let dc_chroma = Huffman::new(&DC_CHROMA_BITS, &DC_VALUES);
    let ac_chroma = Huffman::new(&AC_CHROMA_BITS, &AC_CHROMA_VALUES);
    let mut encoder = Encoder {
        cos: cos_table(),
        writer: BitWriter {
            out,
            buffer: 0,
            count: 0,
        },
    };
    let (mut dc_y, mut dc_u, mut dc_v) = (0, 0, 0);
    let mut block = [0f32; 64];
    for mcu_y in (0..height).step_by(16) {
        for mcu_x in (0..width).step_by(16) {
            for (bx, by) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
                for y in 0..8 {
                    for x in 0..8 {
                        block[(y * 8 + x) as usize] = sample(
                            frame.y,
                            frame.stride_y,
                            width,
                            height,
                            mcu_x + bx + x,
                            mcu_y + by + y,
                            true,
                        );
                    }
                }
                encoder.block(&block, &luma_q, &mut dc_y, &dc_luma, &ac_luma);
            }
            for (plane, stride, previous) in [
                (frame.u, frame.stride_u, &mut dc_u),
                (frame.v, frame.stride_v, &mut dc_v),
            ] {
                for y in 0..8 {
                    for x in 0..8 {
                        block[(y * 8 + x) as usize] = sample(
                            plane,
                            stride,
                            chroma_w,
                            chroma_h,
                            mcu_x / 2 + x,
                            mcu_y / 2 + y,
                            false,
                        );
                    }
                }
                encoder.block(&block, &chroma_q, previous, &dc_chroma, &ac_chroma);
            }
        }
    }
    encoder.writer.flush();
    let mut out = encoder.writer.out;
    out.extend_from_slice(&[0xFF, 0xD9]);
    Ok(out)
}

/// Whether `bytes` looks like a JPEG file (SOI marker at the start, EOI at the end).
pub fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 4 && bytes[..3] == [0xFF, 0xD8, 0xFF] && bytes[bytes.len() - 2..] == [0xFF, 0xD9]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{fill_i420, Rgb};

    #[test]
    fn categories_follow_the_standard() {
        assert_eq!(category(0), (0, 0));
        assert_eq!(category(1), (1, 1));
        assert_eq!(category(-1), (1, 0));
        assert_eq!(category(5), (3, 5));
        assert_eq!(category(-5), (3, 2));
        assert_eq!(category(1023), (10, 1023));
    }

    #[test]
    fn a_synthetic_i420_frame_encodes_to_a_decodable_jpeg() {
        let (width, height) = (100u32, 70u32);
        let (mut y, mut u, mut v) = (
            vec![0u8; (width * height) as usize],
            vec![0u8; (width.div_ceil(2) * height.div_ceil(2)) as usize],
            vec![0u8; (width.div_ceil(2) * height.div_ceil(2)) as usize],
        );
        fill_i420(
            width,
            height,
            (&mut y, width),
            (&mut u, width.div_ceil(2)),
            (&mut v, width.div_ceil(2)),
            Rgb(200, 40, 40),
            0.5,
        );
        let frame = I420 {
            width,
            height,
            y: &y,
            stride_y: width,
            u: &u,
            stride_u: width.div_ceil(2),
            v: &v,
            stride_v: width.div_ceil(2),
        };
        let jpeg = encode_i420(&frame, 70).expect("encodes");
        assert!(is_jpeg(&jpeg));
        let mut decoder = jpeg_decoder::Decoder::new(&jpeg[..]);
        let pixels = decoder.decode().expect("a decodable JPEG");
        let info = decoder.info().expect("info");
        assert_eq!(
            (u32::from(info.width), u32::from(info.height)),
            (width, height)
        );
        assert_eq!(pixels.len(), (width * height * 3) as usize);
        // A pixel well inside the background (away from the bar) is close to the fill colour.
        let at = ((5 * width + 5) * 3) as usize;
        let (r, g, b) = (pixels[at], pixels[at + 1], pixels[at + 2]);
        assert!(
            r.abs_diff(200) < 30 && g.abs_diff(40) < 30 && b.abs_diff(40) < 30,
            "decoded {r},{g},{b}"
        );
    }

    #[test]
    fn a_short_plane_or_empty_frame_is_refused() {
        let y = [0u8; 10];
        let frame = I420 {
            width: 16,
            height: 16,
            y: &y,
            stride_y: 16,
            u: &y,
            stride_u: 8,
            v: &y,
            stride_v: 8,
        };
        assert_eq!(encode_i420(&frame, 70), Err(JpegError::PlaneTooShort));
        let frame = I420 { width: 0, ..frame };
        assert_eq!(encode_i420(&frame, 70), Err(JpegError::Dimensions));
    }

    #[test]
    fn quality_scales_the_tables() {
        assert_eq!(scaled_table(&LUMA_QUANT, 50)[0], 16);
        assert!(scaled_table(&LUMA_QUANT, 90)[0] < 16);
        assert!(scaled_table(&LUMA_QUANT, 10)[0] > 16);
        assert!(scaled_table(&LUMA_QUANT, 100).iter().all(|&q| q >= 1));
    }
}
