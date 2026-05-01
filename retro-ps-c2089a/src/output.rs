//! Output sink for rendered pages and a logger trait for stderr-style
//! diagnostics. The library never touches `std::fs` or `eprintln!`
//! directly — everything goes through these traits.

/// One captured page. `pbm` is the **packed bit body** (no P4 header) —
/// callers can write `format!("P4\n{w} {h}\n").as_bytes()` followed by
/// `pbm` to produce a valid PBM file.
pub struct CapturedPage {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub pbm: Vec<u8>,
}

/// Encode a packed PBM body (P4 raster, MSB-first) as a 1-bit grayscale
/// PNG. PBM packs `1 = black` and PNG bitdepth-1 grayscale packs
/// `0 = black` the same way; the conversion is just an inversion plus
/// a per-row tail-bit mask. Compression is set to `Fast` — the cart
/// data is sparse, so the size win from `Default`/`Best` is small and
/// not worth the extra CPU.
pub fn pbm_to_png(width: u32, height: u32, pbm: &[u8]) -> Result<Vec<u8>, String> {
    let stride = (width as usize + 7) / 8;
    let h = height as usize;
    if pbm.len() < stride * h {
        return Err(format!(
            "pbm body too short: have {} bytes, need {}", pbm.len(), stride * h
        ));
    }
    let mut inverted = Vec::with_capacity(stride * h);
    let tail_bits = (width as usize) & 7;
    let tail_mask: u8 = if tail_bits == 0 { 0xff } else { 0xff_u8 << (8 - tail_bits) };
    for y in 0..h {
        let row = &pbm[y * stride..y * stride + stride];
        for (i, &b) in row.iter().enumerate() {
            let inv = !b;
            inverted.push(if i + 1 == stride { inv & tail_mask } else { inv });
        }
    }
    let mut out = Vec::with_capacity(stride * h + 1024);
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::One);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
        writer.write_image_data(&inverted).map_err(|e| format!("png write: {e}"))?;
    }
    Ok(out)
}

/// Receiver for emitted pages. Called once per `showpage`.
pub trait PageSink {
    fn emit_page(&mut self, page: CapturedPage);
}

/// Receiver for log lines and structured signals from the emulator.
/// Each method tags the line with a category so callers can route
/// (e.g.) `ps_out` somewhere different from `info`.
pub trait Logger {
    /// Generic emulator-state info (boot, page emit, watchdog, etc.).
    fn info(&mut self, line: &str);
    /// LCD-trap output: a string the cart asked the host to display.
    fn lcd(&mut self, line: &str);
    /// PS interpreter `print`/`write`/`handleerror` byte stream snapshot.
    fn ps_out(&mut self, count: u32, line: &str);
    /// Hinting-RE diagnostic.
    fn hint(&mut self, line: &str);
    /// Cart's `fatal_assert` (recoverable warning, run continues).
    fn fatal_assert(&mut self, line: &str);
    /// Cart's lowmem panic trap. Native binary aborts; wasm records it.
    fn panic(&mut self, line: &str);
}

/// Logger that drops everything. Used in tests or when the host doesn't
/// care about diagnostics.
pub struct NullLogger;

impl Logger for NullLogger {
    fn info(&mut self, _line: &str) {}
    fn lcd(&mut self, _line: &str) {}
    fn ps_out(&mut self, _count: u32, _line: &str) {}
    fn hint(&mut self, _line: &str) {}
    fn fatal_assert(&mut self, _line: &str) {}
    fn panic(&mut self, _line: &str) {}
}

/// PageSink that buffers pages in memory.
#[derive(Default)]
pub struct VecSink {
    pub pages: Vec<CapturedPage>,
}

impl PageSink for VecSink {
    fn emit_page(&mut self, page: CapturedPage) {
        self.pages.push(page);
    }
}
