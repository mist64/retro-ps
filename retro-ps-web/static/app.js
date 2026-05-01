// retro-ps wasm frontend. Vanilla JS + ES modules, no build step.
//
// Architecture:
//   - Main thread owns the UI, the ROM byte buffer (fetched once at
//     startup), and the file-drop wiring.
//   - A dedicated Web Worker (`worker.js`) imports the wasm package
//     and runs `render_sync()` on each PS file. Pages stream back via
//     `postMessage`; the main thread paints them to <canvas> as they
//     arrive.
//   - The worker is necessary because retro-ps is synchronous and a
//     ~14-page render at 300 DPI takes ~minute. Doing that on the main
//     thread would freeze the page entirely.

const $ = (id) => document.getElementById(id);

// --- Zoom state (pinch-to-zoom inside the viewer) ---
let pageZoom = 1.0;
const ZOOM_MIN = 0.1, ZOOM_MAX = 8.0;

function applyPageZoom() {
  document.querySelectorAll('.page-card > img, .page-card > canvas').forEach(el => {
    const bw = parseFloat(el.dataset.baseW);
    const bh = parseFloat(el.dataset.baseH);
    if (!isFinite(bw) || !isFinite(bh)) return;
    el.style.width = (bw * pageZoom) + 'px';
    el.style.height = (bh * pageZoom) + 'px';
  });
}

// Zoom around an anchor point. (cx, cy) are the mouse / gesture
// coords in viewport pixels; the content under that point stays put.
// Pass null to zoom around the viewer center.
function setZoomAround(targetZoom, cx, cy) {
  const next = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, targetZoom));
  if (next === pageZoom) return;
  const r = viewer.getBoundingClientRect();
  if (cx == null || cy == null) {
    cx = r.left + r.width / 2;
    cy = r.top + r.height / 2;
  }
  // Mouse position in viewer-content coords (pre-zoom).
  const ax = viewer.scrollLeft + (cx - r.left);
  const ay = viewer.scrollTop  + (cy - r.top);
  const ratio = next / pageZoom;
  pageZoom = next;
  applyPageZoom();
  // After re-layout, scroll so the same content point lands under the
  // cursor. scrollWidth/Height has already grown linearly with zoom.
  viewer.scrollLeft = ax * ratio - (cx - r.left);
  viewer.scrollTop  = ay * ratio - (cy - r.top);
}

function nudgeZoom(factor, cx, cy) {
  setZoomAround(pageZoom * factor, cx, cy);
}

const dropzone   = $('dropzone');
const fileInput  = $('file-input');
const statusEl   = $('status');
const viewer     = $('viewer');
const emptyState = $('empty-state');
const pageIndex  = $('page-index');
const logEl      = $('log');
const dpiSel     = $('dpi');
const paperSel   = $('paper');
const customRow  = $('custom-paper');
const paperW     = $('paper-w');
const paperH     = $('paper-h');
const printBtn = $('print');
const tabDrop = $('tab-drop');
const tabPaste = $('tab-paste');
const tabSample = $('tab-sample');
const panelDrop = $('panel-drop');
const panelPaste = $('panel-paste');
const panelSample = $('panel-sample');
const psTextarea = $('ps-textarea');
const pasteRenderBtn = $('paste-render');

const TABS = [
  { btn: tabDrop,   panel: panelDrop,   key: 'drop'   },
  { btn: tabPaste,  panel: panelPaste,  key: 'paste'  },
  { btn: tabSample, panel: panelSample, key: 'sample' },
];
function switchTab(which) {
  for (const t of TABS) {
    const on = t.key === which;
    t.btn.classList.toggle('active', on);
    t.btn.setAttribute('aria-selected', String(on));
    t.panel.classList.toggle('hidden', !on);
  }
}
TABS.forEach(t => t.btn.addEventListener('click', () => switchTab(t.key)));

// Sample buttons: each one's `data-sample` is the filename under
// `samples/`. Click → fetch the PS bytes → feed submitFile() the same
// way as a dropped file.
panelSample.querySelectorAll('button[data-sample]').forEach(btn => {
  btn.addEventListener('click', async () => {
    const name = btn.dataset.sample;
    try {
      const r = await fetch(`samples/${name}`);
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const buf = await r.arrayBuffer();
      const file = new File([buf], name, { type: 'application/postscript' });
      submitFile(file);
    } catch (e) {
      showError(`couldn't load sample ${name}: ${e}`);
    }
  });
});

pasteRenderBtn.addEventListener('click', () => {
  const text = psTextarea.value;
  if (!text.trim()) return;
  // Synthesize a File so the same submitFile() path handles both modes.
  const file = new File([text], 'pasted.ps', { type: 'application/postscript' });
  submitFile(file);
});

// Log panel collapse toggle.
const logPanel = $('log-panel');
const logToggle = $('log-toggle');
logToggle.addEventListener('click', () => {
  const collapsed = logPanel.classList.toggle('collapsed');
  logToggle.setAttribute('aria-expanded', String(!collapsed));
});

// --- Status helpers ---
function setStatus(text, kind = '') {
  statusEl.textContent = text;
  statusEl.className = 'status' + (kind ? ' ' + kind : '');
}

// --- Paper size custom toggle ---
paperSel.addEventListener('change', () => {
  if (paperSel.value === 'custom') customRow.classList.remove('hidden');
  else customRow.classList.add('hidden');
});

// --- Drop zone wiring ---
['dragenter', 'dragover'].forEach(evt =>
  dropzone.addEventListener(evt, (e) => {
    e.preventDefault(); e.stopPropagation();
    dropzone.classList.add('dragover');
  }));
['dragleave', 'drop'].forEach(evt =>
  dropzone.addEventListener(evt, (e) => {
    e.preventDefault(); e.stopPropagation();
    dropzone.classList.remove('dragover');
  }));

dropzone.addEventListener('drop', (e) => {
  const file = e.dataTransfer.files && e.dataTransfer.files[0];
  if (file) submitFile(file);
});
dropzone.addEventListener('click', () => fileInput.click());
dropzone.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault();
    fileInput.click();
  }
});
fileInput.addEventListener('change', () => {
  if (fileInput.files[0]) submitFile(fileInput.files[0]);
});

// --- Print: one PNG per printed page ---
// The @media print rules in app.css hide everything except the page
// images and force a page-break between them, so window.print() —
// whether triggered by the button or Cmd/Ctrl+P — yields a clean
// print preview with one rendered page per sheet.
printBtn.addEventListener('click', () => window.print());

// --- Pinch / ctrl-wheel zoom inside the viewer ---
// Browsers translate trackpad pinches into wheel events with ctrlKey.
// Eat them inside the viewer so the OS-level page zoom doesn't fire,
// and apply the delta to our local `pageZoom` instead.
viewer.addEventListener('wheel', (e) => {
  if (!e.ctrlKey) return;
  e.preventDefault();
  // -deltaY → zoom in. Tame sensitivity so a flick doesn't blow past
  // the clamp; Math.exp keeps it geometrically smooth. Anchor at the
  // mouse position so the page point under the cursor stays put.
  nudgeZoom(Math.exp(-e.deltaY * 0.01), e.clientX, e.clientY);
}, { passive: false });

// Safari fires GestureEvents alongside ctrl-wheel for trackpad pinches.
// Track the cumulative scale relative to gesture start so we don't
// double-count, and anchor around the gesture's centroid.
let gestureStartZoom = 1.0;
let gestureAnchorX = 0, gestureAnchorY = 0;
viewer.addEventListener('gesturestart', (e) => {
  e.preventDefault();
  gestureStartZoom = pageZoom;
  gestureAnchorX = e.clientX;
  gestureAnchorY = e.clientY;
});
viewer.addEventListener('gesturechange', (e) => {
  e.preventDefault();
  setZoomAround(gestureStartZoom * e.scale, gestureAnchorX, gestureAnchorY);
});
viewer.addEventListener('gestureend', (e) => e.preventDefault());

// Cmd/Ctrl + / - / 0 keyboard shortcuts (also blocks browser zoom).
// No mouse anchor here — pivot around viewer center.
window.addEventListener('keydown', (e) => {
  if (!(e.metaKey || e.ctrlKey)) return;
  if (e.key === '=' || e.key === '+') { e.preventDefault(); nudgeZoom(1.1, null, null); }
  else if (e.key === '-') { e.preventDefault(); nudgeZoom(1 / 1.1, null, null); }
  else if (e.key === '0') { e.preventDefault(); setZoomAround(1.0, null, null); }
});

// --- ROM fetched once at startup, then handed to the worker ---
let romLoaded = false;
// Kept on the main thread so a fresh worker (spawned after Stop) can be
// re-primed without a network round-trip. ~2 MB.
let cachedRomBytes = null;
async function loadRom() {
  const r = await fetch('c2089a.bin');
  if (!r.ok) throw new Error(`ROM fetch failed: HTTP ${r.status}`);
  const buf = await r.arrayBuffer();
  if (buf.byteLength !== 2 * 1024 * 1024) {
    throw new Error(`ROM size unexpected: ${buf.byteLength} bytes (expected 2 MB)`);
  }
  cachedRomBytes = new Uint8Array(buf);
  // Send a copy to the worker; keep our own for stop/restart.
  const copy = cachedRomBytes.slice(0).buffer;
  ensureWorker().postMessage({ type: 'set_rom', rom: new Uint8Array(copy) }, [copy]);
  romLoaded = true;
}

// --- Worker: spun up once, reused across renders ---
let worker = null;
let currentRenderId = 0;

function ensureWorker() {
  if (worker) return worker;
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
  worker.onmessage = onWorkerMessage;
  worker.onerror = (e) => {
    console.error('worker error', e);
    setStatus('worker error', 'error');
    showError(String(e.message || e));
  };
  return worker;
}

// --- Page-size inference (ported from web/src/main.rs `infer_page_size`).
// Peek the first ~16 KiB of the PS for DSC media hints. Recognises:
//   %%DocumentMedia: NAME W H ...        (W/H in pt)
//   %%BoundingBox: 0 0 W H               (fallback)
function inferPageSize(psBytes) {
  const head = psBytes.subarray(0, Math.min(psBytes.byteLength, 16 * 1024));
  const text = new TextDecoder('latin1').decode(head);
  let bbox = null;
  for (const raw of text.split('\n')) {
    const line = raw.trimStart();
    if (line.startsWith('%%DocumentMedia:')) {
      // %%DocumentMedia: name W H weight color type
      const parts = line.slice('%%DocumentMedia:'.length).trim().split(/\s+/);
      if (parts.length >= 3) {
        const w = parseFloat(parts[1]);
        const h = parseFloat(parts[2]);
        if (w > 0 && h > 0) return [Math.ceil(w), Math.ceil(h)];
      }
    } else if (line.startsWith('%%BoundingBox:')) {
      const parts = line.slice('%%BoundingBox:'.length).trim().split(/\s+/);
      if (parts.length === 4 && parts[0] !== '(atend)') {
        const w = parseFloat(parts[2]);
        const h = parseFloat(parts[3]);
        if (w > 0 && h > 0) bbox = [Math.ceil(w), Math.ceil(h)];
      }
    }
  }
  return bbox;
}

// --- Render submit ---
let elapsedTimer = null;
let renderedCount = 0;
let pageObserver = null;
let lastFile = null;
let renderStart = 0;

function rerenderIfReady() {
  if (lastFile) submitFile(lastFile);
}
dpiSel.addEventListener('change', rerenderIfReady);
paperSel.addEventListener('change', rerenderIfReady);
paperW.addEventListener('change', rerenderIfReady);
paperH.addEventListener('change', rerenderIfReady);

function ensureSpinner() {
  let sp = $('viewer-spinner');
  if (sp) return sp;
  sp = document.createElement('div');
  sp.id = 'viewer-spinner';
  sp.className = 'spinner';
  sp.innerHTML = '<div class="spinner-ring"></div><div class="spinner-text">rendering...</div>';
  viewer.appendChild(sp);
  return sp;
}
function removeSpinner() {
  const sp = $('viewer-spinner');
  if (sp) sp.remove();
}

async function submitFile(file) {
  if (!romLoaded) {
    showError('ROM not loaded yet');
    return;
  }
  if (!file.name.toLowerCase().endsWith('.ps') &&
      file.type !== 'application/postscript') {
    console.warn('not .ps?', file.name, file.type);
  }
  lastFile = file;

  // Reset UI. Revoke any object URLs we held so the GC reclaims the
  // previous render's PNG blobs.
  viewer.querySelectorAll('.page-card img').forEach(img => {
    if (img.src && img.src.startsWith('blob:')) URL.revokeObjectURL(img.src);
  });
  viewer.querySelectorAll('.page-card, .page-label, .error-banner, .spinner').forEach(n => n.remove());
  emptyState.classList.add('hidden');
  pageIndex.innerHTML = '';
  logEl.textContent = '';
  // Re-collapse the log panel so it only re-opens if THIS render
  // produces ps_out (i.e. an error or PS-side print).
  logPanel.classList.add('collapsed');
  logToggle.setAttribute('aria-expanded', 'false');
  printBtn.disabled = true;
  renderedCount = 0;
  if (pageObserver) { pageObserver.disconnect(); pageObserver = null; }

  const psBuf = await file.arrayBuffer();
  const psBytes = new Uint8Array(psBuf);

  const opts = {
    paper_dpi: parseInt(dpiSel.value, 10),
  };
  if (paperSel.value === 'auto') {
    const inferred = inferPageSize(psBytes);
    if (inferred) {
      opts.paper_w = inferred[0];
      opts.paper_h = inferred[1];
    }
  } else if (paperSel.value === 'custom') {
    opts.paper_w = parseInt(paperW.value, 10);
    opts.paper_h = parseInt(paperH.value, 10);
  } else {
    const [w, h] = paperSel.value.split('x').map((s) => parseInt(s, 10));
    opts.paper_w = w;
    opts.paper_h = h;
  }
  // Bigger insn cap so multi-page jobs (font_chart 35 pp) actually
  // finish. Same as cart_web's previous default.
  opts.max_insns = 8_000_000_000;

  renderStart = Date.now();
  setStatus('rendering... 0.0 s', 'busy');
  ensureSpinner();
  if (elapsedTimer) clearInterval(elapsedTimer);
  elapsedTimer = setInterval(() => {
    const s = ((Date.now() - renderStart) / 1000).toFixed(1);
    setStatus(`rendering... ${s} s`, 'busy');
  }, 100);

  currentRenderId++;
  const renderId = currentRenderId;
  // Worker has the ROM cached from boot; just send the PS bytes.
  ensureWorker().postMessage(
    { type: 'render', renderId, ps: psBytes, opts },
    [psBuf]
  );
}

function onWorkerMessage(ev) {
  const m = ev.data;
  if (!m) return;
  // Stale renders (user dropped a new file before the previous finished)
  // — silently drop.
  if (m.renderId !== currentRenderId) return;

  switch (m.type) {
    case 'page':
      paintPage(m.index, m.width, m.height, m.dpi || 300, m.png);
      renderedCount++;
      break;
    case 'log':
      appendLog(m.kind, m.line);
      break;
    case 'done':
      clearInterval(elapsedTimer); elapsedTimer = null;
      removeSpinner();
      if (m.error) {
        setStatus('error', 'error');
        showError(m.error);
        if (renderedCount === 0) emptyState.classList.remove('hidden');
      } else if (m.wedged) {
        if (m.pages === 0) {
          // No pages emitted at all → almost always bad PS source.
          setStatus('no pages', 'error');
          showError('renderer made no progress. The input is likely not valid PostScript or uses operators the cart does not implement. Check the Log panel for an "Error: …" line.');
          emptyState.classList.remove('hidden');
        } else {
          setStatus(`${m.pages} page${m.pages === 1 ? '' : 's'} (wedged)`, 'error');
          showError(`renderer wedged after page ${m.pages}. Got the first ${m.pages} page(s); subsequent pages may need a lower DPI.`);
        }
      } else if (m.panicked) {
        setStatus('panic', 'error');
        showError('cart firmware panicked');
      } else if (m.pages === 0) {
        setStatus('no pages', 'error');
        showError('no pages produced. Input may be empty or invalid.');
        emptyState.classList.remove('hidden');
      } else {
        const elapsedS = ((Date.now() - renderStart) / 1000).toFixed(1);
        setStatus(`${m.pages} page${m.pages === 1 ? '' : 's'} in ${elapsedS} s`, 'done');
      }
      // Enable print button if at least one page made it through —
      // works for clean done AND wedged (we still want partial output).
      if (renderedCount > 0) printBtn.disabled = false;
      break;
    case 'ready':
      setStatus('idle');
      break;
    default:
      console.warn('unknown worker msg', m);
  }
}

function appendLog(kind, line) {
  // The Log panel is for *user-relevant* output only — i.e. what the
  // PostScript program itself wrote (errors, `print` operator output).
  // Everything else (info chatter, LCD updates, hint traces, panic
  // notices) is debug for us and stays out of the UI.
  if (kind !== 'ps_out') return;
  // First user-visible line of this render → auto-expand the panel
  // (default state is collapsed).
  if (logPanel && logPanel.classList.contains('collapsed')) {
    logPanel.classList.remove('collapsed');
    logToggle.setAttribute('aria-expanded', 'true');
  }
  // Cap at ~200 lines so memory doesn't grow unbounded.
  const cur = logEl.textContent;
  const text = cur + (cur ? '\n' : '') + line;
  const lines = text.split('\n');
  logEl.textContent = lines.length > 200 ? lines.slice(-200).join('\n') : text;
}

function showError(msg) {
  const div = document.createElement('div');
  div.className = 'error-banner';
  div.textContent = msg;
  viewer.prepend(div);
}

// Paint a single page to a fresh <canvas> in the viewer. `rgba` is the
// raw 4-bytes-per-pixel buffer the wasm side pre-converted from PBM.
// Set @page size to match the rendered page's natural pt dimensions.
// Default browser print fits to the host's paper size and adds its own
// margins, which boxes the image into ~75 % of the sheet. With an
// explicit @page size + zero margin, the printer sheet IS the page,
// and the IMG at width:100%/height:auto fills it edge-to-edge.
let printPageStyle = null;
function setPrintPageSize(ptsW, ptsH) {
  if (!printPageStyle) {
    printPageStyle = document.createElement('style');
    document.head.appendChild(printPageStyle);
  }
  printPageStyle.textContent = `@media print { @page { size: ${ptsW.toFixed(1)}pt ${ptsH.toFixed(1)}pt; margin: 0; } }`;
}

function paintPage(index, width, height, dpi, png) {
  if (!pageObserver) {
    // Track every card's current intersection ratio. A naive
    // threshold-fires-first observer would jitter the active
    // highlight when two cards are both ≥20 % visible during
    // scroll; pick the one with the highest ratio across all
    // currently-observed cards instead.
    const visibility = new WeakMap();
    const cards = new Set();
    pageObserver = new IntersectionObserver((entries) => {
      entries.forEach(e => {
        visibility.set(e.target, e.intersectionRatio);
        cards.add(e.target);
      });
      let activeIdx = -1, bestRatio = 0;
      for (const card of cards) {
        const r = visibility.get(card) || 0;
        if (r > bestRatio) {
          bestRatio = r;
          activeIdx = parseInt(card.dataset.idx, 10);
        }
      }
      if (activeIdx < 0) return;
      const lis = pageIndex.querySelectorAll('li');
      lis.forEach((li, j) => li.classList.toggle('active', j === activeIdx));
      // If the newly-active item is scrolled out of the page-list
      // pane, nudge it into view. `nearest` is a no-op when already
      // visible — no jitter while scrolling within the list.
      const active = lis[activeIdx];
      if (active) active.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    }, { root: viewer, threshold: [0, 0.25, 0.5, 0.75, 1.0] });
  }
  const sp = $('viewer-spinner');

  // PNG bytes already came in pre-encoded by the wasm worker (bitdepth-1
  // grayscale, zlib level=Fast). Wrap as a Blob and hand the object
  // URL straight to an <img>.
  const blob = new Blob([png], { type: 'image/png' });
  const url = URL.createObjectURL(blob);

  const card = document.createElement('div');
  card.className = 'page-card';
  card.id = `page-${index}`;
  card.dataset.idx = String(index);
  card.title = `Page ${index + 1} (${width} x ${height} px) - click to zoom`;

  const ptsW = width * 72 / dpi;
  const ptsH = height * 72 / dpi;
  // First page sets the print sheet size; assume the rest match.
  if (index === 0) setPrintPageSize(ptsW, ptsH);

  const img = document.createElement('img');
  img.src = url;
  img.alt = `page ${index + 1}`;
  img.dataset.baseW = String(ptsW);
  img.dataset.baseH = String(ptsH);
  img.style.width = (ptsW * pageZoom) + 'px';
  img.style.height = (ptsH * pageZoom) + 'px';
  card.appendChild(img);

  if (sp) viewer.insertBefore(card, sp); else viewer.appendChild(card);

  const lbl = document.createElement('div');
  lbl.className = 'page-label';
  lbl.textContent = `page ${index + 1}  -  ${width} x ${height} px`;
  if (sp) viewer.insertBefore(lbl, sp); else viewer.appendChild(lbl);

  const li = document.createElement('li');
  li.textContent = String(index + 1);
  li.addEventListener('click', () => {
    card.scrollIntoView({ behavior: 'smooth', block: 'start' });
  });
  pageIndex.appendChild(li);
  pageObserver.observe(card);
}

// --- Boot ---
(async function boot() {
  try {
    setStatus('loading wasm...', 'busy');
    ensureWorker();
    setStatus('loading ROM...', 'busy');
    await loadRom();
    setStatus('idle');
  } catch (e) {
    console.error(e);
    setStatus('boot failed', 'error');
    showError(String(e.message || e));
  }
})();
