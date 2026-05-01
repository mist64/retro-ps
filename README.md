# retro-ps

Classic PostScript interpreter binaries running inside a CPU-emulator sandbox, on the command line or in your browser.

The [HP C2089A "PostScript Cartridge Plus"](https://www.pagetable.com/?p=1673) turns an HP LaserJet II/III into a PostScript printer. It contains a 2 MB ROM with Adobe's reference Level 2 PostScript interpreter (version 2010.118). We feed it bytes, emulate the 68k, fake the bits of the printer's main board it expects to be there, and collect each page it draws – and its status/error messages.

The original cart only renders at 300 DPI, with a 0.25" margin, on some pre-defined paper sizes. We generalise on top of that:

- Any resolution.
- Any paper size.
- Any lpi for the screening dots. We default to 1/5 of the printing dpi.
- No margins.

The only hard limit is 16,000 pixels in each dimension.

## CLI

```
cargo build --release
target/release/retro-ps some.ps -o /tmp/out
```

Output is one 1-bit grayscale PNG per page; the cart's PS-side `print` / `==` / error output goes to stderr unprefixed, like a real PostScript printer.

Flags:

| | |
|---|---|
| `<INPUT>` | PostScript file to render (positional) |
| `-o, --output PREFIX` | output prefix; files land at `<prefix>_NN.png` (default `page`) |
| `--rom PATH` | use a different ROM image instead of the baked-in one |
| `--paper-w PT` / `--paper-h PT` | paper size in PostScript points (default `612` × `792`) |
| `--paper-dpi N` | render DPI (default `300`) |
| `--paper-w-px N` / `--paper-h-px N` | raw pixel override; wins over `--paper-w` × DPI |
| `--lpi N` / `--screen-angle DEG` | force halftone frequency / angle via a `setscreen` prolog |
| `--lj3` | mimic the original LaserJet III: 612 × 792 at 300 DPI with the 0.25" hardware margin and imageable-area crop |
| `--exit-after N` | stop after N captured pages |
| `--max-insns N` | hard cap on cart instructions executed |
| `-v, --verbose` | also print emulator boot/teardown chatter, LCD status text, and per-page write notices |
| `--ram-snapshot PATH` / `--ram-snapshot-force` | dump cart RAM to disk for offline inspection |

## Browser

```
cd retro-ps-web
./build.sh                            # wasm-pack into pkg/
python3 -m http.server 8080
open http://127.0.0.1:8080/
```

Drop a `.ps` file onto the page; the wasm runs in a Web Worker so the render doesn't block the UI.

A live build is at <https://www.pagetable.com/retro-ps/>.

## Internals

A few things worth knowing about how this differs from a real cartridge in a real LaserJet:

- **CPU and RAM.** The emulator's 68020 sees more than the original 68000's max. 16 MB of RAM here. That headroom is what lets the cart render high-DPI pages without us rewriting its allocator.
- **Pixel ceiling.** The PS interpreter's `clip` operator caps content at about 16,000 device pixels per axis. That's a cart-firmware limit we can't paper over without patching the ROM, so the practical DPI max scales with paper size — about 1450 DPI on Letter, less on bigger paper.
- **No mainboard, no engine.** We have the cartridge ROM, not the LaserJet III's mainboard ROM, and we don't have a print engine at all. The emulator stands in for the half-dozen low-memory soft-traps the mainboard would have provided (printer-model byte, IPC byte stream, engine status polling) and fakes the engine-done interrupt so the cart's state machine can move on to the next page.
- **Halftone scaling.** Adobe hand-tuned the cart's halftone cell for 300 DPI; above that the default cell renders too sparse and grayscale fills look chalky. We inject a DPI-scaled `setscreen` prolog so the cart builds an appropriately-sized dot itself.

## Author

Claude, directed by Michael Steil &lt;mist64@mac.com&gt;.

## License

Public domain.
