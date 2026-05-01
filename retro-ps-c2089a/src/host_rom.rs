use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;

use crate::bus::Bus;
use crate::output::Logger;

// Lowmem soft-trap entry points.
const TRAP_PRINTER_PROBE:  u32 = 0x04EA;
const TRAP_HEAP_TOP:       u32 = 0x04F0;
const TRAP_CONFIG_BYTE:    u32 = 0x0406;
const TRAP_INSTALL_TRAP0:  u32 = 0x0550;
const TRAP_LCD_STRING:     u32 = 0x061C;
const TRAP_ENGINE_POLL:    u32 = 0x07BA;
const TRAP_ENGINE_CMD:     u32 = 0x07C6;
const TRAP_ENGINE_RESP:    u32 = 0x07C0;
const TRAP_PANIC:          u32 = 0x073A;
const TRAPS_CFG_ZERO:      &[u32] = &[0x0940, 0x0976, 0x097C];
const TRAPS_ENGINE_OK:     &[u32] = &[0x07A8, 0x07AE, 0x07B4];

// PC-anchored host patches.
const PC_HANDSHAKE: u32 = 0x0040_0662;
const PC_ENG_STATUS_POLL: u32 = 0x0052_ACC4;
const FN_IPC_RECV_8: u32 = 0x005F_7AE0;
const FN_IPC_RECV: u32 = 0x005F_9248;
const FN_IPC_IOCTL: u32 = 0x005F_8900;
const FN_TASK_WAIT_QUEUE: u32 = 0x005F_7784;

// Cart-RAM globals shared with cart_hooks.rs. Anything cart_hooks.rs
// also reads is `pub(crate)`; module-private addresses stay private.
const DAT_TASK_QUEUE_BASE: u32 = 0x00B1_9E1C;
const DAT_ENG_STRUCT: u32 = 0x00B1_9E44;
pub(crate) const DAT_PS_DEVICE: u32 = 0x00B1_7E78;
const DAT_PS_SERVER_STATE: u32 = 0x00B1_8DF4;
pub(crate) const DAT_RING_READER: u32 = 0x00B1_8538;
pub(crate) const DAT_RING_BASE: u32 = 0x00B1_8550;
const DAT_JOB_DESC_SCRATCH: u32 = 0x00B3_E000;
// Page-ring slot count + pool descriptor cursor/end. Used by both
// the host's page_done freelist walk and cart_hooks' diagnostic dumps.
pub(crate) const DAT_RING_COUNT: u32 = 0x00B1_8540;
pub(crate) const DAT_POOL_CUR:   u32 = 0x00B1_8C6C;
pub(crate) const DAT_POOL_END:   u32 = 0x00B1_8C68;

const ROM_STR_SERIAL: u32 = 0x004F_FAFB;
const PS_JOB_QUEUE: u32 = 6;
const PS_JOB_MSG_TYPE: u16 = 0x14;
const ENGINE_TYPE_LJ3: u8 = 7;

pub struct HostRom {
    pub stream: Vec<u8>,
    pub stream_pos: usize,
    pub handshake_hits: u32,
    pub engine_busy: i32,
    pub job_injected: bool,
    pub exit_after_env: Option<u32>,
    pub page_band_bufs: Vec<u32>,
    /// Set by the IPC ioctl/recv hooks once natural termination conditions
    /// are met. Main loop observes this and breaks the run cleanly (no
    /// `process::exit`).
    pub stop_requested: bool,
    /// Whether a TRAP_PANIC fired. Main loop translates this to a fatal
    /// run-result on the way out.
    pub panic_fired: bool,
}

const IPC_INJECT_WARMUP_INSNS: u64 = 3_000_000;

impl HostRom {
    pub fn new(mut ps_input: Vec<u8>, exit_after_env: Option<u32>, mut prolog: Vec<u8>) -> Self {
        prolog.append(&mut ps_input);
        HostRom {
            stream: prolog,
            stream_pos: 0,
            handshake_hits: 0,
            engine_busy: 0,
            job_injected: false,
            exit_after_env,
            page_band_bufs: Vec::new(),
            stop_requested: false,
            panic_fired: false,
        }
    }

    /// Mark the printer-engine ready bit at $D00025. Real LJ III host
    /// ROM does this; we run before any cart code so the firmware sees
    /// a ready engine on its first probe.
    pub fn init(&mut self, bus: &mut Bus) {
        bus.write_byte(0x00D0_0025, 0x01);
    }

    pub fn maybe_grow_pool_end(&self, bus: &mut Bus, insns: u64) {
        const TARGET: u32 = 0x0040_0000;
        if insns > 6_000_000 {
            let cur = bus.read_long(DAT_POOL_END);
            if cur > 0 && cur < TARGET {
                bus.write_long(DAT_POOL_END, TARGET);
            }
        }
    }
}

fn short_circuit_rts(cpu: &mut CpuCore, bus: &mut Bus, d0: u32) {
    let sp = cpu.sp();
    let ret = bus.read_long(sp);
    cpu.set_d(0, d0);
    cpu.pc = ret;
    cpu.set_sp(sp + 4);
}

pub fn lowmem_trap(cpu: &mut CpuCore, bus: &mut Bus, host: &mut HostRom, log: &mut dyn Logger, pc: u32) {
    let sp = cpu.sp();
    let a1 = bus.read_long(sp + 4);
    let a2 = bus.read_long(sp + 8);

    let d0 = match pc {
        TRAP_PRINTER_PROBE => 0x00B0_0000,
        TRAP_HEAP_TOP      => 0x00E0_0000,
        p if p == TRAP_CONFIG_BYTE || TRAPS_CFG_ZERO.contains(&p) => 0,
        TRAP_ENGINE_POLL   => if a1 == 3 || host.engine_busy > 0 { 1 } else { 0 },
        TRAP_ENGINE_CMD    => {
            host.engine_busy = 3;
            1
        }
        TRAP_ENGINE_RESP   => {
            bus.write_byte(a1, ENGINE_TYPE_LJ3);
            host.engine_busy = 0;
            1
        }
        TRAP_LCD_STRING => { print_lcd_string(bus, log, a1); 0 }
        TRAP_INSTALL_TRAP0 => {
            if a1 == 0 { bus.write_long(0x80, a2); }
            0
        }
        TRAP_PANIC => {
            log.panic("[host] cartridge PANIC");
            host.panic_fired = true;
            host.stop_requested = true;
            0
        }
        p if TRAPS_ENGINE_OK.contains(&p) => 1,
        _ => 0,
    };
    cpu.set_d(0, d0);
}

fn print_lcd_string(bus: &mut Bus, log: &mut dyn Logger, addr: u32) {
    let mut buf = [0u8; 80];
    for (i, slot) in buf.iter_mut().enumerate().take(79) {
        let c = bus.read_byte(addr.wrapping_add(i as u32));
        if c == 0 { break; }
        *slot = if (32..127).contains(&c) { c } else { b'?' };
    }
    let s = core::str::from_utf8(&buf).unwrap_or("<non-utf8>").trim_end_matches('\0');
    log.lcd(s);
}

pub fn on_instr(
    cpu: &mut CpuCore,
    bus: &mut Bus,
    host: &mut HostRom,
    log: &mut dyn Logger,
    insns: u64,
    pc: u32,
    pages_emitted: i32,
) -> bool {
    match pc {
        PC_HANDSHAKE => {
            host.handshake_hits += 1;
            if host.handshake_hits == 3 {
                let d3 = cpu.d(3);
                let v = bus.read_word(d3);
                bus.write_word(d3, v & !0x4000);
            }
            true
        }
        PC_ENG_STATUS_POLL => {
            let eng = bus.read_long(DAT_ENG_STRUCT);
            let b = bus.read_byte(eng + 0x2d);
            if b & 2 != 0 {
                bus.write_byte(eng + 0x2d, b & !2);
            }
            true
        }
        FN_TASK_WAIT_QUEUE => task_wait_queue_hook(cpu, bus),
        FN_IPC_RECV_8 => ipc_recv_8_hook(cpu, bus, host, insns),
        FN_IPC_RECV => ipc_recv_hook(cpu, bus, host, log, pages_emitted),
        FN_IPC_IOCTL => ipc_ioctl_hook(cpu, bus, host, log, pages_emitted),
        _ => false,
    }
}

fn task_wait_queue_hook(cpu: &mut CpuCore, bus: &mut Bus) -> bool {
    let sp = cpu.sp();
    let qid = bus.read_word(sp + 6) as u32;
    let qbase = bus.read_long(DAT_TASK_QUEUE_BASE);
    let slot = bus.read_long(qbase + qid * 8);
    if slot == 0 {
        return false;
    }
    bus.write_long(qbase + qid * 8, 0);
    short_circuit_rts(cpu, bus, 0);
    true
}

fn ipc_recv_8_hook(cpu: &mut CpuCore, bus: &mut Bus, host: &mut HostRom, insns: u64) -> bool {
    if host.job_injected {
        return false;
    }
    let sp = cpu.sp();
    let h = bus.read_long(sp + 4) & 0xFFFF;
    let bufp = bus.read_long(sp + 8);
    if h != PS_JOB_QUEUE || host.stream.is_empty() {
        return false;
    }
    if insns < IPC_INJECT_WARMUP_INSNS {
        return false;
    }
    // Job descriptor scratch buffer: 64 bytes the cart will read field-
    // wise as `(name, len)*3, …`. We only fill the first 24 bytes; zero
    // the rest so stale data from a prior job doesn't leak in.
    let desc = DAT_JOB_DESC_SCRATCH;
    for off in 0..64 {
        bus.write_byte(desc + off, 0);
    }
    bus.write_long(desc,      ROM_STR_SERIAL);
    bus.write_long(desc + 4,  1);
    bus.write_long(desc + 8,  ROM_STR_SERIAL);
    bus.write_long(desc + 12, 1);
    bus.write_long(desc + 16, ROM_STR_SERIAL);
    bus.write_long(desc + 20, 7);
    bus.write_word(bufp,     0);
    bus.write_word(bufp + 2, PS_JOB_MSG_TYPE);
    bus.write_long(bufp + 4, desc);
    short_circuit_rts(cpu, bus, 1);
    host.job_injected = true;
    true
}

fn ipc_recv_hook(
    cpu: &mut CpuCore, bus: &mut Bus, host: &mut HostRom,
    log: &mut dyn Logger, pages_emitted: i32,
) -> bool {
    if host.stream.is_empty() {
        return false;
    }
    let sp = cpu.sp();
    let handle = bus.read_long(sp + 4);
    let buf = bus.read_long(sp + 8);
    let count = bus.read_long(sp + 12);
    let state = bus.read_long(DAT_PS_SERVER_STATE);
    let stdin_h = if state != 0 { bus.read_long(state + 0x2C) } else { 0 };
    if stdin_h == 0 || (handle & 0xFFFF) != (stdin_h & 0xFFFF) {
        return false;
    }
    let tail = &host.stream[host.stream_pos..];
    if tail.is_empty() && pages_emitted > 0 && host.exit_after_env.is_none() {
        log.info(&format!(
            "[host] job done: input drained + {} page(s) captured",
            pages_emitted
        ));
        host.stop_requested = true;
        return false;
    }
    let n = (count as usize).min(tail.len());
    for (i, &b) in tail[..n].iter().enumerate() {
        bus.write_byte(buf + i as u32, b);
    }
    host.stream_pos += n;
    short_circuit_rts(cpu, bus, n as u32);
    true
}

fn ipc_ioctl_hook(
    cpu: &mut CpuCore, bus: &mut Bus, host: &mut HostRom,
    log: &mut dyn Logger, pages_emitted: i32,
) -> bool {
    if host.stream.is_empty() {
        return false;
    }
    let sp = cpu.sp();
    let handle = bus.read_long(sp + 4);
    let subop = bus.read_long(sp + 8);
    let state = bus.read_long(DAT_PS_SERVER_STATE);
    let stdin_h = if state != 0 {
        bus.read_long(state + 0x2C)
    } else {
        0
    };
    if stdin_h == 0 || (handle & 0xFFFF) != (stdin_h & 0xFFFF) {
        return false;
    }
    if subop != 0x13 {
        return false;
    }
    if host.stream_pos < host.stream.len() {
        short_circuit_rts(cpu, bus, 1);
        return true;
    }
    if pages_emitted > 0 && host.exit_after_env.is_none() {
        log.info(&format!(
            "[host] job done: scanner idle-poll + {} page(s) captured",
            pages_emitted
        ));
        host.stop_requested = true;
    }
    false
}

fn find_drv24(bus: &mut Bus) -> u32 {
    let reader = bus.read_long(DAT_RING_READER) & 0xf;
    let ring_entry = DAT_RING_BASE + reader * 0x44;
    let driver = bus.read_long(ring_entry + 0x8);
    if driver == 0 {
        return 0;
    }
    bus.read_long(driver + 0x24)
}

/// Synthesize the engine-done ISR side-effects after we've captured a
/// page raster. Frees the band buffers we tracked, clears the
/// per-driver page cache slots, zeroes the captured frame, and clears
/// `pdev+0x14` so the cart will accept the next showpage.
pub fn page_done(host: &mut HostRom, bus: &mut Bus, frame_addr: u32, frame_bytes: u32) {
    const POOL_DESCRIPTOR: u32 = 0x00B1_852C;
    const POOL_FREELIST_HEAD: u32 = 0x34;
    const BAND_BUF_SIZE: u32 = 0x3fc; // bytes the cart reserves per band
    const PDEV_BUSY: u32 = 0x14;
    const DRV24_CACHE: u32 = 0x78;
    const DRV24_NSLOTS: u32 = 0x3c;
    const DRV24_NBANDS: u32 = 0x40;
    const CACHE_SLOT_BYTES: u32 = 0x14;
    const CACHE_SLOT_BANDARR: u32 = 0x10;

    // Push every tracked band-buffer back onto the free list at
    // pool_descriptor[+0x34], decrementing pool_cur by the cart's per-
    // band slot size for each one.
    let pool = bus.read_long(POOL_DESCRIPTOR);
    if pool != 0 {
        for &buf in &host.page_band_bufs {
            let head = bus.read_long(pool + POOL_FREELIST_HEAD);
            bus.write_long(buf, head);
            bus.write_long(pool + POOL_FREELIST_HEAD, buf);
            let cur = bus.read_long(DAT_POOL_CUR);
            if cur >= BAND_BUF_SIZE {
                bus.write_long(DAT_POOL_CUR, cur - BAND_BUF_SIZE);
            }
        }
    }
    host.page_band_bufs.clear();

    // Clear `pdev+0x14`: mirrors the engine IRQ handler that signals
    // "engine free" so the cart will accept the next showpage.
    let pdev = bus.read_long(DAT_PS_DEVICE);
    if pdev != 0 {
        bus.write_long(pdev + PDEV_BUSY, 0);
    }

    // Walk the driver's per-page cache, clearing each slot's slot[0]/[+4]
    // and its band array (allowing the cart to reuse the slot next page).
    let drv24 = find_drv24(bus);
    if drv24 != 0 {
        let cache = bus.read_long(drv24 + DRV24_CACHE);
        let nslots = bus.read_long(drv24 + DRV24_NSLOTS);
        let nbands = bus.read_long(drv24 + DRV24_NBANDS);
        if cache != 0 && nslots <= 64 {
            for i in 0..nslots {
                let cslot = cache + i * CACHE_SLOT_BYTES;
                bus.write_long(cslot, 0);
                bus.write_long(cslot + 4, 0);
                let barr = bus.read_long(cslot + CACHE_SLOT_BANDARR);
                if barr != 0 && nbands > 0 && nbands < 1024 {
                    for j in 0..nbands {
                        bus.write_byte(barr + j, 0);
                    }
                }
            }
        }
    }

    // Wipe the just-captured framebuffer so the next page renders
    // against a clean slate.
    if frame_addr != 0 && frame_bytes != 0 {
        for off in 0..frame_bytes {
            bus.write_byte(frame_addr + off, 0);
        }
    }
}
