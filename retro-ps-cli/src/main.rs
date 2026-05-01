//! CLI shim around the `retro-ps-c2089a` library. Handles arg parsing,
//! file I/O, and stderr-style logging. The library stays free of
//! `std::fs`, `clap`, and `std::process::exit`.

use std::fs;
use std::process::ExitCode;

use clap::Parser;

use retro_ps_c2089a::cfg::{Cfg, CfgInputs};
use retro_ps_c2089a::output::{CapturedPage, Logger, PageSink};
use retro_ps_c2089a::render;

/// HP C2089A PostScript cartridge emulator.
///
/// Renders a PS file through the cart firmware and writes captured pages
/// to disk as 1-bit grayscale PNGs.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// PostScript file to render.
    input: String,

    /// Path to a cart ROM. Defaults to the 2 MB c2089a.bin baked into
    /// the binary at build time.
    #[arg(long)]
    rom: Option<String>,

    /// Output filename prefix. Files land at `<prefix>_NN.png`.
    #[arg(short = 'o', long, default_value = "page")]
    output: String,

    /// Paper width in PostScript points. Default 612 (US Letter).
    #[arg(long)]
    paper_w: Option<i64>,

    /// Paper height in PostScript points. Default 792 (US Letter).
    #[arg(long)]
    paper_h: Option<i64>,

    /// Render DPI. Default 300. Practical max is bounded by the cart's
    /// ~16000-device-pixels-per-axis clip cap (≈ 1450 DPI on Letter).
    #[arg(long)]
    paper_dpi: Option<i64>,

    /// Raw pixel width override (wins over derived w-pt × dpi).
    #[arg(long)]
    paper_w_px: Option<u32>,

    /// Raw pixel height override.
    #[arg(long)]
    paper_h_px: Option<u32>,

    /// LaserJet III emulation mode.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    lj3: bool,

    /// Exit after N captured pages.
    #[arg(long)]
    exit_after: Option<u32>,

    /// Hard cap on cart insns executed. Default unlimited — the
    /// no-page-progress and fatal_assert watchdogs handle wedges.
    #[arg(long, default_value_t = u64::MAX)]
    max_insns: u64,

    /// Print host-side chatter alongside the cart's output: emulator
    /// boot/teardown info, LCD status text, per-page write notices,
    /// internal diagnostic lines.
    #[arg(short = 'v', long, action = clap::ArgAction::SetTrue)]
    verbose: bool,

    /// Force halftone frequency by injecting a setscreen prolog.
    #[arg(long)]
    lpi: Option<u32>,

    /// Screen angle for --lpi, in degrees.
    #[arg(long, default_value_t = 0)]
    screen_angle: i32,

    /// Dump all RAM to this file on first band_write.
    #[arg(long)]
    ram_snapshot: Option<String>,

    /// With RAM_SNAPSHOT, dump every band_write.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    ram_snapshot_force: bool,
}

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {}", path, e))
}

/// Default mode emits exactly what the cart firmware prints — its PS
/// interpreter `print` / `==` / `handleerror` byte stream — and
/// nothing else. `--verbose` opens up the host-side chatter
/// (boot/teardown info, LCD status text, internal diagnostic lines).
struct StderrLogger { verbose: bool }

impl Logger for StderrLogger {
    fn info(&mut self, line: &str) {
        if self.verbose { eprintln!("{}", line); }
    }
    fn lcd(&mut self, line: &str) {
        if self.verbose { eprintln!("[lcd] {}", line); }
    }
    fn ps_out(&mut self, _count: u32, line: &str) {
        // The cart already includes its own newlines and brackets;
        // pass it through verbatim so error tracebacks read like
        // a real PostScript printer's stderr.
        eprintln!("{}", line);
    }
    fn hint(&mut self, line: &str) {
        if self.verbose { eprintln!("{}", line); }
    }
    fn fatal_assert(&mut self, line: &str) {
        if self.verbose { eprintln!("{}", line); }
    }
    fn panic(&mut self, line: &str) { eprintln!("{}", line); }
}

/// Writes each captured page to disk as a 1-bit grayscale PNG.
struct FileSink {
    output_prefix: String,
    verbose: bool,
}

fn write_png(path: &str, page: &CapturedPage) -> std::io::Result<()> {
    let bytes = retro_ps_c2089a::output::pbm_to_png(page.width, page.height, &page.pbm)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(path, bytes)
}

impl PageSink for FileSink {
    fn emit_page(&mut self, page: CapturedPage) {
        let path = format!("{}_{:02}.png", self.output_prefix, page.index);
        match write_png(&path, &page) {
            Ok(()) if self.verbose => eprintln!(
                "wrote {} ({}x{})",
                path, page.width, page.height
            ),
            Ok(()) => {}
            Err(e) => eprintln!("error writing {}: {}", path, e),
        }
    }
}

/// Cart ROM baked into the binary so the CLI runs out of the box. The
/// file lives at the workspace root (`retro-ps/c2089a.bin`).
const EMBEDDED_ROM: &[u8] = include_bytes!("../../retro-ps-c2089a/c2089a.bin");

fn run(cli: Cli) -> Result<i32, String> {
    let rom: Vec<u8> = match &cli.rom {
        Some(path) => read_file(path)?,
        None => EMBEDDED_ROM.to_vec(),
    };
    let ps_input = read_file(&cli.input)?;
    let inputs = CfgInputs {
        paper_w: cli.paper_w,
        paper_h: cli.paper_h,
        paper_dpi: cli.paper_dpi,
        paper_w_px: cli.paper_w_px,
        paper_h_px: cli.paper_h_px,
        lj3: cli.lj3,
        max_insns: Some(cli.max_insns),
        // The logger filters host-side chatter at the sink; ask the
        // library to skip emitting it at the source when we know it'd
        // be filtered out anyway.
        quiet: !cli.verbose,
        ram_snapshot: cli.ram_snapshot.is_some(),
        ram_snapshot_force: cli.ram_snapshot_force,
        exit_after: cli.exit_after,
        lpi: cli.lpi,
        screen_angle: Some(cli.screen_angle),
    };
    let cfg = Cfg::new(inputs)?;
    let mut log = StderrLogger { verbose: cli.verbose };
    let mut sink = FileSink {
        output_prefix: cli.output,
        verbose: cli.verbose,
    };
    let result = render(rom, ps_input, &cfg, &mut log, &mut sink)?;

    if let (Some(snap_path), Some(snap_bytes)) = (&cli.ram_snapshot, &result.ram_snapshot) {
        match fs::write(snap_path, snap_bytes) {
            Ok(()) if cli.verbose => {
                eprintln!("wrote {} ({} bytes)", snap_path, snap_bytes.len())
            }
            Ok(()) => {}
            Err(e) => eprintln!("error writing {}: {}", snap_path, e),
        }
    }

    if result.panicked {
        return Ok(2);
    }
    Ok(if result.pages > 0 { 0 } else { 1 })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::from(2)
        }
    }
}
