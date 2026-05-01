//! Golden-checksum tests against the bundled sample PostScript files.
//!
//! Each sample is rendered through the cart at 72 DPI; the CRC32 of
//! every captured page's raw PBM body is compared against a committed
//! expected checksum. PBM is the cart's deterministic output (same
//! bits every run); we checksum that rather than the encoded PNG so
//! the test doesn't depend on the `png` crate's compression behaviour.
//!
//! Run with `cargo test --release` from the workspace. Release is
//! important — debug builds of the m68k interpreter are ~10× slower.
//! `font_chart` dominates the runtime; the others finish in seconds.

use retro_ps_c2089a::cfg::{Cfg, CfgInputs};
use retro_ps_c2089a::output::{NullLogger, VecSink};
use retro_ps_c2089a::render;

/// Cart ROM, embedded in the test binary so the test stands alone.
const ROM: &[u8] = include_bytes!("../c2089a.bin");

fn checksum_page(pbm: &[u8]) -> u32 {
    crc32fast::hash(pbm)
}

fn render_sample(ps: &[u8], dpi: i64, max_pages: u32) -> Vec<u32> {
    let cfg = Cfg::new(CfgInputs {
        paper_dpi: Some(dpi),
        exit_after: Some(max_pages),
        max_insns: Some(8_000_000_000),
        quiet: true,
        ..Default::default()
    })
    .expect("Cfg::new");

    let mut log = NullLogger;
    let mut sink = VecSink::default();
    render(ROM.to_vec(), ps.to_vec(), &cfg, &mut log, &mut sink)
        .expect("render");
    sink.pages.iter().map(|p| checksum_page(&p.pbm)).collect()
}

/// Format checksums as hex on assertion failure; the decimal default
/// is awful to read.
fn hex(sums: &[u32]) -> String {
    let inner: Vec<String> = sums.iter().map(|s| format!("0x{s:08x}")).collect();
    format!("[{}]", inner.join(", "))
}

#[test]
fn startup_page_72dpi() {
    let ps = include_bytes!("../../retro-ps-web/samples/startup_page.ps");
    let sums = render_sample(ps, 72, 1);
    assert_eq!(sums, vec![STARTUP_PAGE_72], "{}", hex(&sums));
}

#[test]
fn test_page_72dpi() {
    let ps = include_bytes!("../../retro-ps-web/samples/test_page.ps");
    let sums = render_sample(ps, 72, 1);
    assert_eq!(sums, vec![TEST_PAGE_72], "{}", hex(&sums));
}

#[test]
fn fontpage_72dpi() {
    let ps = include_bytes!("../../retro-ps-web/samples/fontpage.ps");
    let sums = render_sample(ps, 72, 1);
    assert_eq!(sums, vec![FONTPAGE_72], "{}", hex(&sums));
}

#[test]
fn font_chart_72dpi() {
    let ps = include_bytes!("../../retro-ps-web/samples/font_chart.ps");
    let sums = render_sample(ps, 72, 35);
    assert_eq!(sums, FONT_CHART_72.to_vec(), "{}", hex(&sums));
}

// ─── Expected checksums ──────────────────────────────────────────────
// Regenerate by deleting these and running the test once; the
// assertion failure prints the actual values.

const STARTUP_PAGE_72: u32 = 0x82555b73;
const TEST_PAGE_72:    u32 = 0x06518889;
const FONTPAGE_72:     u32 = 0x16cb2d43;
const FONT_CHART_72: &[u32] = &[
    0x81ca7169, 0x27220fdd, 0xbcd76b52, 0xa321c7d7, 0x0e11af79,
    0xc5ad0171, 0x2ec72738, 0xa0d22670, 0x6775b9f0, 0xd07f06f7,
    0xa16a7550, 0x60823b95, 0xca7dedd7, 0x70dffb77, 0x18742a37,
    0x2527dd12, 0x06db3bc3, 0xf5c79394, 0xb9550b42, 0x3cec2289,
    0xb9f4dc96, 0xc0b6a7a3, 0xac31a14e, 0x5d813366, 0x9b90488d,
    0x6f9d00ab, 0x0d24e25b, 0x2d18324a, 0x7e9c2eae, 0x8ddf0eb5,
    0x7f832655, 0x55606c21, 0x0ff9b88c, 0xd3a36088, 0x596e6343,
];
