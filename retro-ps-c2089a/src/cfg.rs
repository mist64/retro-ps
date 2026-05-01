//! Runtime config for the cart emulator. The library consumes a
//! resolved `Cfg`; building one from user-facing inputs is the
//! caller's job — pass a `CfgInputs` to `Cfg::new`.

/// Resolved config the rest of the emulator consumes. All fields
/// pre-validated by `Cfg::new`.
pub struct Cfg {
    pub max_insns: u64,
    pub quiet: bool,
    pub paper_w_px: u32,
    pub paper_h_px: u32,
    pub paper_dpi: i32,
    pub lj3: bool,
    /// When `Some`, the emulator captures full RAM into the result on
    /// the first `band_write` (or every band_write when force is set).
    pub ram_snapshot: bool,
    pub ram_snapshot_force: bool,
    pub exit_after: Option<u32>,
    pub lpi: Option<u32>,
    pub screen_angle: i32,
}

impl Cfg {
    /// Build a `Cfg` from raw user inputs (paper dims in points or pixels,
    /// DPI, LJ III mode flag). Validates ranges and returns a human-
    /// readable error on bad input.
    ///
    /// `paper_w_px` / `paper_h_px` win when supplied; otherwise the
    /// dims derive from `paper_w` / `paper_h` (in PS points) at `dpi`.
    pub fn new(opts: CfgInputs) -> Result<Self, String> {
        // --lj3 forces a fixed paper/dpi combo.
        if opts.lj3
            && (opts.paper_w.is_some() || opts.paper_h.is_some() || opts.paper_dpi.is_some())
        {
            return Err("--lj3 forces paper=612x792 and dpi=300; \
                        don't combine with --paper-w/-h/--paper-dpi".into());
        }
        let paper_w = opts.paper_w.unwrap_or(612);
        let paper_h = opts.paper_h.unwrap_or(792);
        let dpi = opts.paper_dpi.unwrap_or(300);

        let paper_w_px = opts
            .paper_w_px
            .unwrap_or_else(|| ((paper_w * dpi + 71) / 72) as u32);
        let paper_h_px = opts
            .paper_h_px
            .unwrap_or_else(|| ((paper_h * dpi + 71) / 72) as u32);
        if paper_w_px > 31999 || paper_h_px > 31999 {
            return Err(format!(
                "page exceeds cart 31999 px limit (W_px={} H_px={})",
                paper_w_px, paper_h_px
            ));
        }
        if paper_w_px > 16000 || paper_h_px > 16000 {
            return Err(format!(
                "page {}x{} px exceeds the 16000 px per-axis limit. \
                 Lower paper_dpi or paper_w/-h, or render smaller and \
                 post-upscale.",
                paper_w_px, paper_h_px
            ));
        }

        Ok(Cfg {
            max_insns: opts.max_insns.unwrap_or(u64::MAX),
            quiet: opts.quiet,
            paper_w_px,
            paper_h_px,
            paper_dpi: dpi as i32,
            lj3: opts.lj3,
            ram_snapshot: opts.ram_snapshot,
            ram_snapshot_force: opts.ram_snapshot_force,
            exit_after: opts.exit_after,
            lpi: opts.lpi,
            screen_angle: opts.screen_angle.unwrap_or(0),
        })
    }
}

/// Human-friendly inputs to `Cfg::new`. Paper, DPI, mode flags — no
/// file paths or output prefixes; those are the caller's concern.
#[derive(Default)]
pub struct CfgInputs {
    pub paper_w: Option<i64>,
    pub paper_h: Option<i64>,
    pub paper_dpi: Option<i64>,
    pub paper_w_px: Option<u32>,
    pub paper_h_px: Option<u32>,
    pub lj3: bool,
    pub max_insns: Option<u64>,
    pub quiet: bool,
    pub ram_snapshot: bool,
    pub ram_snapshot_force: bool,
    pub exit_after: Option<u32>,
    pub lpi: Option<u32>,
    pub screen_angle: Option<i32>,
}
