use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;

use crate::bus::{Bus, RAM_BASE, RAM_SIZE};
use crate::cfg::Cfg;
use crate::host_rom::{self, HostRom};
use crate::output::{CapturedPage, Logger, PageSink};

// ─── PCs we hook in cart firmware ───────────────────────────────────
const FN_FATAL_ASSERT: u32 = 0x005F_2938;
const FN_PS_ALLOC: u32 = 0x005F_6D32;
const FN_PS_ALLOC_RTS: u32 = 0x005F_7042;
const FN_PS_OUT: u32 = 0x005E_D184;
const FN_SCAN_BUF_CTOR: u32 = 0x0059_8F08;
const FN_BITMAP_CFG: u32 = 0x0055_78B8;
const FN_SETPAGEDEV_ENTRY: u32 = 0x0054_5812;
const FN_SETPAGEDEV_POST_CORNER: u32 = 0x0054_58F2;
const FN_BAND_WRITE: u32 = 0x0053_2120;
const FN_SHOWPAGE_EMIT_RETURN: u32 = 0x0053_9B62;

// Caller PC after the JSR to FN_PS_ALLOC we want to intercept.
const PS_ALLOC_HIDPI_CALLER: u32 = 0x0055_7A24;

// ps_exec_loop's error-recovery branch (bne.w from setjmp #2 at $5cf898).
// We land here exactly when a PS error has longjmp'd back to the loop.
// vt_newpage's setjmp uses a different jmpbuf so its retry doesn't trip
// this anchor.
const PC_PS_ERROR_RECOVERY: u32 = 0x005d_06a0;
// Cart still needs ~60 k more insns after PS_ERROR_RECOVERY to print
// the error message; 5 M is plenty of grace.
const PS_ERROR_GRACE_INSNS: u64 = 5_000_000;

// Per-band scanline-buffer alloc: D0 = buf_ptr post-malloc.
const PC_BAND_BUF_ALLOC_DONE: u32 = 0x0054_37fe;
// Engine-done (synthetic): pull a slot from the in-use list onto the
// done slot at +0xc. Mirrors the real LJ III IRQ handler.
const PC_ENGINE_DONE_LATCH: u32 = 0x0050_57c6;
// Page-ring slot retire (consumer side): clear retired-slot fields and
// advance the consumer index. Backstops scheduler that's not running.
const PC_PAGE_RING_RETIRE: u32 = 0x0054_4e0a;

// ─── Cart struct field offsets (discovered via Ghidra) ──────────────
const SB_POOL_SIZE:   u32 = 0x2c;
const SB_BAND_H:      u32 = 0x30;
const SB_STRIDE:      u32 = 0x34;
const SB_HEIGHT:      u32 = 0x38;
const SB_NBANDS:      u32 = 0x40;

const BC_STRIDE:      u32 = 0x3c;
const BC_HEIGHT:      u32 = 0x40;

const DEV_HEIGHT_PX:   u32 = 0x20;
const DEV_MATRIX_A:    u32 = 0x24;
const DEV_MATRIX_D:    u32 = 0x30;
const DEV_MATRIX_TX:   u32 = 0x34;
const DEV_MATRIX_TY:   u32 = 0x38;
const DEV_SUB_STRUCT:  u32 = 0x5c;
const SUB_MATRIX_A:    u32 = 0x50;
const SUB_MATRIX_D:    u32 = 0x5c;
const SUB_MATRIX_TX:   u32 = 0x60;
const SUB_MATRIX_TY:   u32 = 0x64;

const SCAN_SLOT_PTR:   u32 = 0x24;
const PAGE_W_PX:       u32 = 0x18;
const PAGE_H_PX:       u32 = 0x20;

const HIDPI_BIGBUF: u32 = RAM_BASE + 0x0200_0000;

/// Per-page state observed by the page-capture hooks. Set as the cart
/// builds the page, read out at the showpage seam, then reset.
#[derive(Default)]
pub struct Capture {
    pub page_w: u32,
    pub page_h: u32,
    pub slot_ptr: u32,
    pub page_counter: i32,
    /// Last scan_buf shape we logged; stops the per-page scan_buf log
    /// from spamming when nothing actually changed across pages.
    pub last_scan_buf_key: Option<(u32, u32, u32, u32)>,
}

/// Cross-run signals: things the run loop in `lib.rs` polls each tick.
/// Hooks set these; nobody else clears them.
#[derive(Default)]
pub struct Watchdog {
    /// Set when fatal_assert fires. Run loop tears down N M insns later
    /// if no new page emits in the meantime.
    pub fatal_assert_at: Option<u64>,
    /// Set when the synthetic exit-after threshold trips, or by a
    /// PS-error grace-window arm. Run loop observes and breaks cleanly.
    pub stop_requested: bool,
    /// Hard deadline for the run. Used by the PS-error anchor to give
    /// the cart a few M insns of grace so its error-recovery print
    /// finishes before we kill the loop.
    pub stop_at_insn: Option<u64>,
    /// Full RAM snapshot for offline RE — populated by `band_write` when
    /// `cfg.ram_snapshot` is on. First-band-only by default; every-band
    /// when `cfg.ram_snapshot_force`.
    pub ram_snapshot: Option<Vec<u8>>,
}

fn short_circuit_rts(cpu: &mut CpuCore, bus: &mut Bus, d0: u32) {
    let sp = cpu.sp();
    let ret = bus.read_long(sp);
    cpu.set_d(0, d0);
    cpu.pc = ret;
    cpu.set_sp(sp + 4);
}

/// Emit an info line via the logger. Skipped when `cfg.quiet` is set.
/// Lazy: format args only evaluated when not quiet.
macro_rules! info_fmt {
    ($cfg:expr, $log:expr, $insns:expr, $($arg:tt)*) => {{
        if !$cfg.quiet {
            $log.info(&format!("[emu {:>10}] {}", $insns, format_args!($($arg)*)));
        }
    }};
}

/// Per-instruction PC dispatch for the PS-capture override layer.
#[allow(clippy::too_many_arguments)]
pub fn on_instr(
    cpu: &mut CpuCore,
    bus: &mut Bus,
    host: &mut HostRom,
    cfg: &Cfg,
    cap: &mut Capture,
    wd: &mut Watchdog,
    log: &mut dyn Logger,
    sink: &mut dyn PageSink,
    insns: u64,
    pc: u32,
) -> bool {
    match pc {
        FN_FATAL_ASSERT => {
            fatal_assert_log(cpu, bus, log, insns);
            if wd.fatal_assert_at.is_none() {
                wd.fatal_assert_at = Some(insns);
                let ret = bus.read_long(cpu.sp());
                if ret == 0x0053_0fba {
                    // $530fba = compositor's switch-default arm; assert
                    // here means a page-pool walk hit a corrupt entry.
                    band_compositor_assert_dump(cpu, bus, cfg, log, insns);
                }
            }
        }
        FN_PS_ALLOC => return ps_alloc_hook(cpu, bus, cfg, log, insns),
        FN_PS_ALLOC_RTS => ps_alloc_return_log(cpu, bus, cfg, log, insns),
        FN_PS_OUT => ps_out_trace(cpu, bus, cfg, log, insns),
        PC_PS_ERROR_RECOVERY => {
            if wd.stop_at_insn.is_none() {
                wd.stop_at_insn = Some(insns + PS_ERROR_GRACE_INSNS);
                info_fmt!(cfg, log, insns, "PS error raised (ps_exec_loop recovery)");
            }
        }
        FN_SCAN_BUF_CTOR => scan_buf_ctor_hook(cpu, bus, cfg, cap, log, insns),
        FN_BITMAP_CFG => bitmap_cfg_hook(cpu, bus, cfg, log, insns),
        FN_SETPAGEDEV_ENTRY => setpagedev_entry_hook(cpu, bus, cfg),
        FN_SETPAGEDEV_POST_CORNER => setpagedev_matrix_hook(cpu, bus, cfg),
        FN_BAND_WRITE => band_write_hook(cpu, bus, cfg, cap, wd, log, insns),
        FN_SHOWPAGE_EMIT_RETURN => page_capture_hook(bus, host, cfg, cap, wd, log, sink, insns),
        PC_BAND_BUF_ALLOC_DONE => {
            let buf_ptr = cpu.d(0);
            if buf_ptr != 0 {
                host.page_band_bufs.push(buf_ptr);
            }
        }
        PC_ENGINE_DONE_LATCH => engine_done_latch(bus),
        PC_PAGE_RING_RETIRE => page_ring_retire(bus),
        _ => {}
    }
    false
}

/// Synthetic engine-done: move one slot off the in-use list to the
/// done slot at engine[+0xc]. The real LJ III IRQ handler does this;
/// we synthesize it because we don't model the hardware interrupt.
fn engine_done_latch(bus: &mut Bus) {
    let eng = bus.read_long(host_rom::DAT_PS_DEVICE);
    if eng == 0 || bus.read_long(eng + 0xc) != 0 {
        return;
    }
    let head = bus.read_long(eng + 0x10);
    if head == 0 {
        return;
    }
    let next = bus.read_long(head);
    bus.write_long(eng + 0x10, next);
    bus.write_long(head, 0);
    bus.write_long(head + 4, 0);
    bus.write_long(eng + 0xc, head);
}

/// Page-ring consumer-side retire: clear the retired slot's known
/// fields, advance the consumer index, and decrement the in-use count.
/// Drives the slot back onto the free list when the cart's normal
/// scheduler isn't running it for us.
fn page_ring_retire(bus: &mut Bus) {
    const RING_SLOT_BYTES: u32 = 0x44;
    const RING_HIWATER: i32 = 0x10;
    // Offsets within a ring slot that the cart leaves dirty for us to clear.
    const SLOT_DIRTY_FIELDS: &[u32] =
        &[0x00, 0x04, 0x10, 0x14, 0x18, 0x1c, 0x20, 0x30, 0x34, 0x38];

    let count = bus.read_long(host_rom::DAT_RING_COUNT) as i32;
    if count < RING_HIWATER {
        return;
    }
    let cons = bus.read_long(host_rom::DAT_RING_READER);
    let slot = host_rom::DAT_RING_BASE + (cons & 0xf) * RING_SLOT_BYTES;
    for &off in SLOT_DIRTY_FIELDS {
        bus.write_long(slot + off, 0);
    }
    bus.write_long(host_rom::DAT_RING_COUNT, (count - 1) as u32);
    bus.write_long(host_rom::DAT_RING_READER, (cons + 1) & 0xf);
}

fn fatal_assert_log(cpu: &mut CpuCore, bus: &mut Bus, log: &mut dyn Logger, insns: u64) {
    let ret = bus.read_long(cpu.sp());
    log.fatal_assert(&format!("[emu {:>10}] fatal_assert from {:06x}", insns, ret));
}

fn band_compositor_assert_dump(cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg, log: &mut dyn Logger, insns: u64) {
    let a6 = cpu.a(6);
    let p1 = bus.read_long(a6 + 8);
    let p2 = bus.read_long(a6 + 12);
    let p3 = bus.read_long(a6 + 16);
    let bad_byte = bus.read_byte(a6.wrapping_sub(0x8d));
    let pu_var20 = bus.read_long(p2);
    let elem_word: u32 = if pu_var20 != 0 {
        bus.read_long(pu_var20)
    } else {
        0
    };
    let pool_cur = bus.read_long(host_rom::DAT_POOL_CUR);
    let pool_end = bus.read_long(host_rom::DAT_POOL_END);
    let ring_cnt = bus.read_long(host_rom::DAT_RING_COUNT);
    info_fmt!(cfg, log, insns,
        "  band_compositor: A6=${:08x} p1=${:08x} p2=${:08x} p3=${:08x}",
        a6, p1, p2, p3);
    info_fmt!(cfg, log, insns,
        "  bad_type=0x{:02x} *p2=${:08x} *(*p2)[u32]=${:08x}",
        bad_byte, pu_var20, elem_word);
    info_fmt!(cfg, log, insns,
        "  pool_cur=${:x} / pool_end=${:x}  page_ring_count={}",
        pool_cur, pool_end, ring_cnt);
    let a4 = cpu.a(4);
    info_fmt!(cfg, log, insns, "  A4 (walk cursor) = ${:08x}", a4);
    if a4 != 0 && a4 < RAM_BASE + RAM_SIZE - 64 {
        let start = a4.saturating_sub(16);
        let mut bytes = [0u8; 64];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = bus.read_byte(start + i as u32);
        }
        info_fmt!(cfg, log, insns, "  A4[-16..+48]: {:02x?}", bytes);
    }
    if pu_var20 != 0 && pu_var20 < RAM_BASE + RAM_SIZE - 32 {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = bus.read_byte(pu_var20 + i as u32);
        }
        info_fmt!(cfg, log, insns, "  *p2[0..32]: {:02x?}", bytes);
    }
}

fn ps_alloc_hook(cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg, log: &mut dyn Logger, insns: u64) -> bool {
    let sp = cpu.sp();
    if bus.read_long(sp) != PS_ALLOC_HIDPI_CALLER {
        return false;
    }
    let sz = bus.read_long(sp + 4);
    info_fmt!(cfg, log, insns, "ps_alloc hijack size={} -> ${:x}", sz, HIDPI_BIGBUF);
    short_circuit_rts(cpu, bus, HIDPI_BIGBUF);
    true
}

fn ps_alloc_return_log(cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg, log: &mut dyn Logger, insns: u64) {
    let d0 = cpu.d(0);
    if d0 == 0 {
        let ret = bus.read_long(cpu.sp());
        info_fmt!(cfg, log, insns, "FUN_005f6d32 FAILED caller={:06x}", ret);
    }
}

fn ps_out_trace(cpu: &mut CpuCore, bus: &mut Bus, _cfg: &Cfg, log: &mut dyn Logger, _insns: u64) {
    let sp = cpu.sp();
    let buf = bus.read_long(sp + 4);
    let cnt = bus.read_long(sp + 12);
    if cnt == 0 || cnt >= 8192 {
        return;
    }
    let mut line = String::new();
    for i in 0..cnt.min(1023) {
        let b = bus.read_byte(buf + i);
        line.push(match b {
            b'\n' => '\\',
            0x20..=0x7E => b as char,
            _ => '.',
        });
    }
    log.ps_out(cnt, &line);
}

fn scan_buf_ctor_hook(
    cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg, cap: &mut Capture, log: &mut dyn Logger, insns: u64,
) {
    if cfg.paper_w_px == 0 && cfg.paper_h_px == 0 {
        return;
    }
    let p = bus.read_long(cpu.sp() + 4);
    if p == 0 {
        return;
    }
    let stride = if cfg.paper_w_px != 0 {
        cfg.paper_w_px.div_ceil(8)
    } else {
        bus.read_long(p + SB_STRIDE)
    };
    let h = if cfg.paper_h_px != 0 {
        cfg.paper_h_px
    } else {
        bus.read_long(p + SB_HEIGHT)
    };
    let band_h = bus.read_long(p + SB_BAND_H);
    let nbands = if band_h != 0 { h.div_ceil(band_h) } else { 7 };
    let pool_size = stride * h;

    bus.write_long(p + SB_BAND_H, band_h);
    bus.write_long(p + SB_STRIDE, stride);
    bus.write_long(p + SB_HEIGHT, h);
    bus.write_long(p + SB_NBANDS, nbands);
    let cur_pool = bus.read_long(p + SB_POOL_SIZE);
    if cur_pool < pool_size {
        bus.write_long(p + SB_POOL_SIZE, pool_size);
    }
    let key = (stride, h, nbands, pool_size);
    if cap.last_scan_buf_key != Some(key) {
        cap.last_scan_buf_key = Some(key);
        info_fmt!(cfg, log, insns,
            "scan_buf: stride={} H={} nbands={} poolsize={}",
            stride, h, nbands, pool_size);
    }
}

fn bitmap_cfg_hook(cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg, log: &mut dyn Logger, insns: u64) {
    let p = bus.read_long(cpu.sp() + 4);
    if p == 0 {
        return;
    }
    let old_stride = bus.read_long(p + BC_STRIDE);
    let old_h = bus.read_long(p + BC_HEIGHT);
    if cfg.paper_w_px != 0 {
        bus.write_long(p + BC_STRIDE, cfg.paper_w_px.div_ceil(8));
    }
    if cfg.paper_h_px != 0 {
        bus.write_long(p + BC_HEIGHT, cfg.paper_h_px);
    }
    info_fmt!(cfg, log, insns,
        "bitmap_cfg: stride {}->{} H {}->{}",
        old_stride, cfg.paper_w_px.div_ceil(8), old_h, cfg.paper_h_px);
}

fn setpagedev_entry_hook(cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg) {
    if cfg.paper_w_px == 0 && cfg.paper_h_px == 0 {
        return;
    }
    let sp = cpu.sp();
    if cfg.paper_w_px != 0 {
        bus.write_long(sp + 12, cfg.paper_w_px);
    }
    if cfg.paper_h_px != 0 {
        bus.write_long(sp + 16, cfg.paper_h_px);
    }
}

fn setpagedev_matrix_hook(cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg) {
    let dev = cpu.d(0);
    if dev == 0 {
        return;
    }
    let h_f32 = bus.read_long(dev + DEV_HEIGHT_PX) as f32;
    let sub = bus.read_long(dev + DEV_SUB_STRUCT);

    let scale = cfg.paper_dpi as f32 / 72.0;

    bus.write_long(dev + DEV_MATRIX_A,  scale.to_bits());
    bus.write_long(dev + DEV_MATRIX_D,  (-scale).to_bits());
    bus.write_long(dev + DEV_MATRIX_TX, 0u32);
    bus.write_long(dev + DEV_MATRIX_TY, h_f32.to_bits());
    if sub != 0 {
        bus.write_long(sub + SUB_MATRIX_A,  scale.to_bits());
        bus.write_long(sub + SUB_MATRIX_D,  (-scale).to_bits());
        bus.write_long(sub + SUB_MATRIX_TX, 0u32);
        bus.write_long(sub + SUB_MATRIX_TY, 0u32);
    }
}

fn band_write_hook(
    cpu: &mut CpuCore, bus: &mut Bus, cfg: &Cfg,
    cap: &mut Capture, wd: &mut Watchdog,
    log: &mut dyn Logger, insns: u64,
) {
    let sp = cpu.sp();
    let band_y   = bus.read_long(sp + 8);
    let scan_buf = bus.read_long(sp + 12);
    let dev      = bus.read_long(sp + 20);

    cap.slot_ptr = bus.read_long(scan_buf + SCAN_SLOT_PTR);
    cap.page_w = bus.read_long(dev + PAGE_W_PX);
    cap.page_h = bus.read_long(dev + PAGE_H_PX);

    let first_band = band_y == 0 && cap.page_counter == 0;
    if (first_band || cfg.ram_snapshot_force) && cfg.ram_snapshot {
        wd.ram_snapshot = Some(bus.ram.clone());
        info_fmt!(cfg, log, insns, "RAM_SNAPSHOT captured ({} bytes)", RAM_SIZE);
    }
}

/// Drop the LJ III's 0.25" hardware-margin border off all four sides
/// of a P4 PBM byte buffer. Returns the new (w, h, packed bytes).
fn crop_to_imageable(
    buf: &[u8], w: u32, h: u32, stride: u32, dpi: i32,
) -> (u32, u32, Vec<u8>) {
    let margin = (dpi as u32 * 75 / 300).min(w / 2).min(h / 2);
    if margin == 0 {
        return (w, h, buf.to_vec());
    }
    let new_w = w - 2 * margin;
    let new_h = h - 2 * margin;
    let new_stride = new_w.div_ceil(8);
    let mut out = vec![0u8; (new_stride * new_h) as usize];
    for y in 0..new_h {
        let src_row = ((y + margin) * stride) as usize;
        let dst_row = (y * new_stride) as usize;
        for x in 0..new_w {
            let src_x = x + margin;
            let bit = (buf[src_row + (src_x / 8) as usize] >> (7 - src_x % 8)) & 1;
            if bit != 0 {
                out[dst_row + (x / 8) as usize] |= 1 << (7 - x % 8);
            }
        }
    }
    (new_w, new_h, out)
}

#[allow(clippy::too_many_arguments)]
fn page_capture_hook(
    bus: &mut Bus, host: &mut HostRom, cfg: &Cfg,
    cap: &mut Capture, wd: &mut Watchdog,
    log: &mut dyn Logger, sink: &mut dyn PageSink, insns: u64,
) {
    if cap.slot_ptr == 0 || cap.page_w == 0 || cap.page_h == 0 {
        return;
    }
    let stride = cap.page_w.div_ceil(8);
    let total = stride * cap.page_h;
    let page_idx = cap.page_counter as u32;
    cap.page_counter += 1;

    let mut buf = Vec::with_capacity(total as usize);
    for i in 0..total {
        buf.push(bus.read_byte(cap.slot_ptr + i));
    }
    let (out_w, out_h, out_buf) = if cfg.lj3 {
        crop_to_imageable(&buf, cap.page_w, cap.page_h, stride, cfg.paper_dpi)
    } else {
        (cap.page_w, cap.page_h, buf)
    };
    let pbm_len = out_buf.len();
    sink.emit_page(CapturedPage {
        index: page_idx,
        width: out_w,
        height: out_h,
        pbm: out_buf,
    });
    info_fmt!(cfg, log, insns,
        "wrote page {:02} ({}x{}, {} bytes from ${:06x})",
        page_idx, out_w, out_h, pbm_len, cap.slot_ptr);
    let captured = cap.slot_ptr;
    cap.slot_ptr = 0;
    host_rom::page_done(host, bus, captured, total);
    // Page emitted: clear the fatal_assert deadline so the watchdog
    // doesn't trip mid-job after a recoverable warning earlier on.
    wd.fatal_assert_at = None;
    if let Some(n) = cfg.exit_after {
        if cap.page_counter as u32 >= n {
            info_fmt!(cfg, log, insns, "captured {} page(s), stop requested", cap.page_counter);
            wd.stop_requested = true;
        }
    }
}

