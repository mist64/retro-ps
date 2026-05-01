//! WebAssembly wrapper around the `retro-ps-c2089a` cart-emulator library.
//!
//! `render_sync` is the one entry point: synchronous, returns when the
//! cart finishes. Pages stream out via a JS callback as they emit so
//! the frontend can paint incrementally. Logs flow through a separate
//! callback. All buffers cross the boundary as `Uint8Array`.

use wasm_bindgen::prelude::*;

use retro_ps_c2089a::cfg::{Cfg, CfgInputs};
use retro_ps_c2089a::output::{CapturedPage, Logger, PageSink};

/// CLI subset the static frontend actually uses.
#[wasm_bindgen]
#[derive(Default)]
pub struct RenderOpts {
    paper_w: Option<i64>,
    paper_h: Option<i64>,
    paper_dpi: Option<i64>,
    paper_w_px: Option<u32>,
    paper_h_px: Option<u32>,
    lj3: bool,
    max_insns: Option<u64>,
    exit_after: Option<u32>,
}

#[wasm_bindgen]
impl RenderOpts {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self { Self::default() }

    pub fn set_paper_w(&mut self, v: f64) { self.paper_w = Some(v as i64); }
    pub fn set_paper_h(&mut self, v: f64) { self.paper_h = Some(v as i64); }
    pub fn set_paper_dpi(&mut self, v: f64) { self.paper_dpi = Some(v as i64); }
    pub fn set_paper_w_px(&mut self, v: u32) { self.paper_w_px = Some(v); }
    pub fn set_paper_h_px(&mut self, v: u32) { self.paper_h_px = Some(v); }
    pub fn set_lj3(&mut self, v: bool) { self.lj3 = v; }
    pub fn set_max_insns(&mut self, v: f64) { self.max_insns = Some(v as u64); }
    pub fn set_exit_after(&mut self, v: u32) { self.exit_after = Some(v); }
}

impl RenderOpts {
    fn to_cfg_inputs(&self) -> CfgInputs {
        CfgInputs {
            paper_w: self.paper_w,
            paper_h: self.paper_h,
            paper_dpi: self.paper_dpi,
            paper_w_px: self.paper_w_px,
            paper_h_px: self.paper_h_px,
            lj3: self.lj3,
            max_insns: self.max_insns,
            exit_after: self.exit_after,
            // wasm: keep info logs on; JS filters by `kind`.
            ..CfgInputs::default()
        }
    }
}

/// Logger that hands each line to a JS callback as `(kind, line)`. The
/// frontend filters by kind (e.g. only `"ps_out"` reaches the log panel).
struct JsLogger { cb: Option<js_sys::Function> }

impl JsLogger {
    fn emit(&self, kind: &str, line: &str) {
        if let Some(cb) = &self.cb {
            let _ = cb.call2(&JsValue::NULL, &JsValue::from_str(kind), &JsValue::from_str(line));
        }
    }
}

impl Logger for JsLogger {
    fn info(&mut self, line: &str)         { self.emit("info", line); }
    fn lcd(&mut self, line: &str)          { self.emit("lcd", line); }
    fn ps_out(&mut self, count: u32, line: &str) {
        self.emit("ps_out", &format!("[{}] {}", count, line));
    }
    fn hint(&mut self, line: &str)         { self.emit("hint", line); }
    fn fatal_assert(&mut self, line: &str) { self.emit("fatal_assert", line); }
    fn panic(&mut self, line: &str)        { self.emit("panic", line); }
}

/// Adapter wrapping a closure as a `PageSink`.
struct ClosureSink<F: FnMut(CapturedPage)> { cb: F }

impl<F: FnMut(CapturedPage)> PageSink for ClosureSink<F> {
    fn emit_page(&mut self, page: CapturedPage) { (self.cb)(page); }
}

/// Sink that calls `page_cb(index, width, height, pbm)` for each page.
/// The PBM body crosses as a fresh `Uint8Array` (zero-copy isn't worth
/// the lifetime gymnastics for tens of KB per page).
fn js_page_sink(page_cb: Option<js_sys::Function>) -> ClosureSink<impl FnMut(CapturedPage)> {
    ClosureSink {
        cb: move |page: CapturedPage| {
            if let Some(cb) = &page_cb {
                let arr = js_sys::Uint8Array::from(page.pbm.as_slice());
                let _ = cb.call4(
                    &JsValue::NULL,
                    &JsValue::from_f64(page.index as f64),
                    &JsValue::from_f64(page.width as f64),
                    &JsValue::from_f64(page.height as f64),
                    &arr,
                );
            }
        },
    }
}

/// Run summary. Pages have already been delivered via `page_cb`.
#[wasm_bindgen]
pub struct RenderResult {
    pages: u32,
    wedged: bool,
    panicked: bool,
    error: Option<String>,
}

impl RenderResult {
    fn err(msg: String) -> Self {
        RenderResult { pages: 0, wedged: false, panicked: false, error: Some(msg) }
    }
}

#[wasm_bindgen]
impl RenderResult {
    #[wasm_bindgen(getter)] pub fn pages(&self) -> u32 { self.pages }
    #[wasm_bindgen(getter)] pub fn wedged(&self) -> bool { self.wedged }
    #[wasm_bindgen(getter)] pub fn panicked(&self) -> bool { self.panicked }
    #[wasm_bindgen(getter)] pub fn error(&self) -> Option<String> { self.error.clone() }
}

/// Run the cart firmware synchronously over `ps_input`.
///
/// `rom` is the 2 MB c2089a.bin image. `page_cb(idx, w, h, pbm)` fires
/// once per captured page; `pbm` is the packed P4 raster body (call
/// [`pbm_to_png`] to encode for canvas display). `log_cb(kind, line)`
/// fires per diagnostic line. Both callbacks are optional.
#[wasm_bindgen]
pub fn render_sync(
    rom: Box<[u8]>,
    ps_input: Box<[u8]>,
    opts: &RenderOpts,
    page_cb: Option<js_sys::Function>,
    log_cb: Option<js_sys::Function>,
) -> RenderResult {
    let cfg = match Cfg::new(opts.to_cfg_inputs()) {
        Ok(c) => c,
        Err(e) => return RenderResult::err(format!("config: {}", e)),
    };
    let mut log = JsLogger { cb: log_cb };
    let mut sink = js_page_sink(page_cb);
    match retro_ps_c2089a::render(rom.into_vec(), ps_input.into_vec(), &cfg, &mut log, &mut sink) {
        Ok(r) => RenderResult {
            pages: r.pages,
            wedged: r.wedged,
            panicked: r.panicked,
            error: None,
        },
        Err(e) => RenderResult::err(e),
    }
}

/// JS-callable PBM → bitdepth-1 PNG encoder. Lets the worker hand
/// PNG bytes straight to a `Blob` URL, skipping a width*height*4 RGBA
/// working canvas on the main thread (at 1200 DPI letter that's 538 MB
/// per page and crashes the tab).
#[wasm_bindgen]
pub fn pbm_to_png(width: u32, height: u32, pbm: &[u8]) -> Result<Box<[u8]>, JsValue> {
    retro_ps_c2089a::output::pbm_to_png(width, height, pbm)
        .map(|v| v.into_boxed_slice())
        .map_err(|e| JsValue::from_str(&e))
}
