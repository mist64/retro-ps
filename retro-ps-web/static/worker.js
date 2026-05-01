// Web worker: hosts the retro-ps wasm engine and runs renders off the
// main thread. The worker imports the wasm-pack output module via ESM,
// initialises it once, then handles `render` requests synchronously
// (the wasm engine itself is a tight m68k step loop — there's no async
// path inside it). Pages stream back via `postMessage` as `page` events.

import init, { RenderOpts, render_sync, pbm_to_png } from '../pkg/retro_ps_wasm.js';

let ready = false;
const queue = [];
// Cached ROM bytes. The main thread posts them once via `set_rom` so
// we don't re-clone 2 MB on every render.
let cachedRom = null;

async function boot() {
  await init({ module_or_path: '../pkg/retro_ps_wasm_bg.wasm' });
  ready = true;
  self.postMessage({ type: 'ready' });
  // Drain anything that arrived during boot.
  while (queue.length) handle(queue.shift());
}

self.onmessage = (ev) => {
  if (!ready) {
    queue.push(ev.data);
    return;
  }
  handle(ev.data);
};

function handle(msg) {
  if (!msg) return;
  if (msg.type === 'set_rom') {
    cachedRom = msg.rom;
    return;
  }
  if (msg.type !== 'render') return;
  const { renderId, ps, opts } = msg;
  const rom = msg.rom || cachedRom;
  if (!rom) {
    self.postMessage({
      type: 'done', renderId,
      pages: 0, wedged: false, panicked: false,
      error: 'no ROM available; call set_rom first',
    });
    return;
  }

  // Build the wasm RenderOpts from the plain JS object.
  const ro = new RenderOpts();
  if (opts.lj3) ro.set_lj3(true);
  if (opts.paper_w !== undefined) ro.set_paper_w(opts.paper_w);
  if (opts.paper_h !== undefined) ro.set_paper_h(opts.paper_h);
  if (opts.paper_dpi !== undefined) ro.set_paper_dpi(opts.paper_dpi);
  if (opts.paper_w_px !== undefined) ro.set_paper_w_px(opts.paper_w_px);
  if (opts.paper_h_px !== undefined) ro.set_paper_h_px(opts.paper_h_px);
  if (opts.max_insns !== undefined) ro.set_max_insns(opts.max_insns);
  if (opts.exit_after !== undefined) ro.set_exit_after(opts.exit_after);

  // Page callback: convert the PBM body to RGBA right here in the
  // worker so the main thread only has to blit pre-formatted pixels.
  const dpi = opts.paper_dpi || 300;
  const pageCb = (index, width, height, pbm) => {
    // Encode bitdepth-1 grayscale PNG with fast (low) zlib here in the
    // worker so the main thread never allocates a width*height*4
    // canvas. At 1200 DPI letter that canvas would be 538 MB per page
    // and the browser kills the tab. PNG bytes are tens of KB to a
    // few MB, depending on content density and zlib level.
    const png = pbm_to_png(width, height, pbm);
    self.postMessage(
      { type: 'page', renderId, index, width, height, dpi, png },
      [png.buffer]
    );
  };
  const logCb = (kind, line) => {
    self.postMessage({ type: 'log', renderId, kind, line });
  };

  let result;
  try {
    result = render_sync(rom, ps, ro, pageCb, logCb);
  } catch (e) {
    self.postMessage({
      type: 'done', renderId,
      pages: 0, wedged: false, panicked: false,
      error: String(e && e.message || e),
    });
    ro.free();
    return;
  }
  self.postMessage({
    type: 'done', renderId,
    pages: result.pages,
    wedged: result.wedged,
    panicked: result.panicked,
    error: result.error || null,
  });
  result.free();
  ro.free();
}

boot().catch((e) => {
  self.postMessage({ type: 'done', renderId: -1, error: String(e && e.message || e) });
});
