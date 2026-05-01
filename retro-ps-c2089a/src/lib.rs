//! retro-ps-c2089a — HP C2089A PostScript cartridge emulator. m68020
//! interpreter plus cart-firmware hooks. Library crate.
//!
//! The library never touches `std::fs`, `std::process::exit`, or
//! `eprintln!` directly — diagnostics flow through the [`Logger`] trait
//! and rendered pages flow through [`PageSink`]. Callers are responsible
//! for whatever I/O makes sense for them.

mod bus;
mod cart_hooks;
pub mod cfg;
mod host_rom;
pub mod output;

use m68k::core::cpu::CpuCore;
use m68k::core::memory::AddressBus;
use m68k::core::types::{CpuType, StepResult};

use bus::{Bus, RAM_BASE, RAM_SIZE, ROM_SIZE};
use cart_hooks::{Capture, Watchdog};
use cfg::Cfg;
use host_rom::HostRom;
use output::{Logger, PageSink};

/// Result of a single render.
pub struct RenderResult {
    /// Number of pages emitted via the [`PageSink`].
    pub pages: u32,
    /// Whether the renderer wedged (no progress watchdog tripped). The
    /// already-emitted pages are still valid; the wedge usually means
    /// later pages won't materialise at this DPI.
    pub wedged: bool,
    /// Whether the cart fired its lowmem panic trap (firmware fatal,
    /// not the recoverable `fatal_assert`).
    pub panicked: bool,
    /// Number of cart insns executed.
    pub insns: u64,
    /// RAM snapshot if `cfg.ram_snapshot` was set. Captured at the first
    /// `band_write` (or every band_write if force was set — last wins).
    pub ram_snapshot: Option<Vec<u8>>,
}

/// Build the PS prolog the cart's stdin sees before user PS bytes.
/// Always installs a `/clippath` that returns a paper-sized rectangle.
/// In `--lj3` mode also redefines `setpagedevice` / `initclip` to clip
/// to the printable area (i.e. mimic the real LJ III's 0.25" margin).
/// Optionally prepends a `setscreen` to force a halftone frequency.
fn build_prolog(cfg: &Cfg, log: &mut dyn Logger) -> Vec<u8> {
    let mut prolog = Vec::<u8>::new();
    if let Some(lpi) = cfg.lpi {
        if !cfg.quiet {
            log.info(&format!(
                "[emu] lpi {} @ angle {}: injecting setscreen prolog",
                lpi, cfg.screen_angle
            ));
        }
        prolog.extend(format!(
            "{} {} {{dup mul exch dup mul add 1 exch sub}} setscreen ",
            lpi, cfg.screen_angle
        ).into_bytes());
    }
    let pw = cfg.paper_w_px as i64 * 72 / cfg.paper_dpi as i64;
    let ph = cfg.paper_h_px as i64 * 72 / cfg.paper_dpi as i64;
    let (l, b, r, t) = if cfg.lj3 {
        (18i64, 18i64, pw - 18, ph - 18)
    } else {
        (0i64, 0i64, pw, ph)
    };
    let bbox = format!(
        "newpath {l} {b} moveto {r} {b} lineto {r} {t} lineto {l} {t} lineto closepath "
    );
    prolog.extend(format!("/clippath {{ {bbox} }} bind def ").into_bytes());
    if cfg.lj3 {
        let clip = format!("{bbox} clip newpath ");
        prolog.extend(format!(
            "/setpagedevice {{ systemdict /setpagedevice get exec {clip} }} bind def \
             /initclip {{ systemdict /initclip get exec {clip} }} bind def {clip}"
        ).into_bytes());
    }
    prolog
}

/// Boot stack frame the cart's reset vector at $4000A0 expects:
/// [sp0]=0, [sp0+4]=RAM top, [sp0+8]=engine struct, [sp0+12]=0.
const BOOT_SP: u32 = 0x00FF_8000;
/// Cart maps a per-engine struct at +0x200000 into RAM.
const ENGINE_STRUCT: u32 = RAM_BASE + 0x0020_0000;
const BOOT_PC: u32 = 0x0040_00A0;

fn boot_cart(bus: &mut Bus) -> CpuCore {
    let ram_top = RAM_BASE + RAM_SIZE - 1;
    bus.write_long(BOOT_SP,      0);
    bus.write_long(BOOT_SP + 4,  ram_top);
    bus.write_long(BOOT_SP + 8,  ENGINE_STRUCT);
    bus.write_long(BOOT_SP + 12, 0);

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68020);
    cpu.reset(bus);
    cpu.set_sr(0x2000);
    cpu.pc = BOOT_PC;
    cpu.set_sp(BOOT_SP);
    cpu.set_a(6, 0);
    cpu
}

/// Run the cart firmware over `ps_input` and emit captured pages via
/// `sink`. `rom` must be the 2 MB c2089a.bin image. `cfg` is fully
/// validated (use [`Cfg::new`] / [`CfgInputs`] to build it).
///
/// Returns a [`RenderResult`] with the page count + termination flags.
/// Errors when the ROM is the wrong size or the m68k core hits a
/// genuine illegal-instruction / unhandled step.
pub fn render(
    rom: Vec<u8>,
    ps_input: Vec<u8>,
    cfg: &Cfg,
    log: &mut dyn Logger,
    sink: &mut dyn PageSink,
) -> Result<RenderResult, String> {
    if rom.len() != ROM_SIZE as usize {
        return Err(format!("ROM short read ({} bytes, expected {})", rom.len(), ROM_SIZE));
    }

    // `[emu] msg` for setup/teardown, `[emu <insns>] msg` for per-tick
    // lines inside the loop. Both elide on --quiet.
    let info = |log: &mut dyn Logger, msg: &str| {
        if !cfg.quiet { log.info(&format!("[emu] {}", msg)); }
    };
    let info_at = |log: &mut dyn Logger, insns: u64, msg: &str| {
        if !cfg.quiet { log.info(&format!("[emu {:>10}] {}", insns, msg)); }
    };

    info(log, &format!("loaded {} bytes of PS input", ps_input.len()));
    let prolog = build_prolog(cfg, log);

    let mut bus = Bus::new(rom);
    let mut host = HostRom::new(ps_input, cfg.exit_after, prolog);
    let mut cap = Capture::default();
    let mut wd = Watchdog::default();

    let mut cpu = boot_cart(&mut bus);
    host.init(&mut bus);

    info(log, &format!(
        "booting: PC=${:06x} SP=${:06x} engine=${:06x} max_insns={}",
        BOOT_PC, BOOT_SP, ENGINE_STRUCT, cfg.max_insns
    ));

    // Watchdog budgets, in cart insns.
    const NO_PAGE_PROGRESS_BUDGET: u64 = 500_000_000;
    const POST_FATAL_ASSERT_BUDGET: u64 = 30_000_000;

    let mut insn_count: u64 = 0;
    let mut wedged_after: Option<u64> = None;
    let mut last_page_at: u64 = 0;
    let mut last_page_count: i32 = 0;
    while insn_count < cfg.max_insns {
        if wd.stop_requested || host.stop_requested {
            break;
        }
        if let Some(at) = wd.stop_at_insn {
            if insn_count >= at {
                info_at(log, insn_count, "aborting after PS-error grace window");
                break;
            }
        }
        if let Some(at) = wd.fatal_assert_at {
            if insn_count > at + POST_FATAL_ASSERT_BUDGET {
                info_at(log, insn_count, &format!(
                    "no progress {} M insns after fatal_assert; aborting at page {}",
                    (insn_count - at) / 1_000_000, cap.page_counter
                ));
                wedged_after = Some(at);
                break;
            }
        }
        if cap.page_counter != last_page_count {
            last_page_count = cap.page_counter;
            last_page_at = insn_count;
        }
        if insn_count > last_page_at + NO_PAGE_PROGRESS_BUDGET {
            // Catches both flavors of wedge:
            //   - page_counter > 0: cart spinning between pages
            //     (e.g. 200-DPI hash.usenix p12).
            //   - page_counter == 0: cart spinning before any page
            //     emits, usually invalid PS it can't recover from. Boot
            //     itself takes ~10 M insns; a 500 M budget gives any
            //     plausible single page time to render.
            info_at(log, insn_count, &format!(
                "no new page in {} M insns since page {}; aborting",
                (insn_count - last_page_at) / 1_000_000, cap.page_counter
            ));
            wedged_after = Some(last_page_at);
            break;
        }
        let pc = cpu.pc;
        host.maybe_grow_pool_end(&mut bus, insn_count);
        if (0x100..0x1000).contains(&pc) {
            host_rom::lowmem_trap(&mut cpu, &mut bus, &mut host, log, pc);
        } else if !host_rom::on_instr(
            &mut cpu, &mut bus, &mut host, log, insn_count, pc, cap.page_counter,
        ) {
            cart_hooks::on_instr(
                &mut cpu, &mut bus, &mut host, cfg, &mut cap, &mut wd, log, sink, insn_count, pc,
            );
        }

        match cpu.step(&mut bus) {
            StepResult::Ok { .. } => insn_count += 1,
            StepResult::Stopped => {
                return Err(format!("cpu stopped at pc=${:06x}", cpu.pc));
            }
            StepResult::TrapInstruction { trap_num } => {
                cpu.take_trap_exception(&mut bus, trap_num);
                insn_count += 1;
            }
            StepResult::AlineTrap { .. } => {
                cpu.take_aline_exception(&mut bus);
                insn_count += 1;
            }
            StepResult::FlineTrap { .. } => {
                cpu.take_fline_exception(&mut bus);
                insn_count += 1;
            }
            StepResult::IllegalInstruction { opcode } => {
                return Err(format!(
                    "illegal op=${:04x} pc=${:06x}",
                    opcode, cpu.pc
                ));
            }
            other => {
                return Err(format!("unhandled step: {:?} pc=${:06x}", other, cpu.pc));
            }
        }
    }
    if let Some(at) = wedged_after {
        info(log, &format!(
            "wedged at insn={} after fatal_assert at insn={}; pages={}",
            insn_count, at, cap.page_counter
        ));
    } else {
        info(log, &format!(
            "done. insns={} pages={}",
            insn_count, cap.page_counter
        ));
    }
    Ok(RenderResult {
        pages: cap.page_counter as u32,
        wedged: wedged_after.is_some(),
        panicked: host.panic_fired,
        insns: insn_count,
        ram_snapshot: wd.ram_snapshot,
    })
}
