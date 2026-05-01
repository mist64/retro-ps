use m68k::core::memory::AddressBus;

pub const ROM_BASE: u32 = 0x0040_0000;
pub const ROM_SIZE: u32 = 0x0020_0000;
pub const RAM_BASE: u32 = 0x00B0_0000;
pub const RAM_SIZE: u32 = 0x0800_0000;
pub const LOW_SIZE: u32 = 0x0000_1000;

pub struct Bus {
    pub rom: Vec<u8>,
    pub ram: Vec<u8>,
    pub low: Vec<u8>,
}

impl Bus {
    /// Build a Bus around a 2 MB ROM image. Allocates 128 MB of RAM
    /// (zero-initialised) and a 4 KB lowmem page pre-filled with RTS
    /// (`0x4E75`) so cart-side jsrs into the host-ROM soft-trap region
    /// return cleanly when we haven't hooked a specific PC.
    pub fn new(rom: Vec<u8>) -> Self {
        assert_eq!(rom.len(), ROM_SIZE as usize, "ROM size mismatch");
        const RTS_OPCODE: u16 = 0x4E75;
        let mut low = vec![0u8; LOW_SIZE as usize];
        for word in low.chunks_exact_mut(2) {
            word.copy_from_slice(&RTS_OPCODE.to_be_bytes());
        }
        Bus {
            rom,
            ram: vec![0u8; RAM_SIZE as usize],
            low,
        }
    }
}

#[inline]
fn in_rom(a: u32) -> bool {
    (ROM_BASE..ROM_BASE + ROM_SIZE).contains(&a)
}
#[inline]
fn in_ram(a: u32) -> bool {
    (RAM_BASE..RAM_BASE.wrapping_add(RAM_SIZE)).contains(&a)
}
#[inline]
fn in_low(a: u32) -> bool {
    a < LOW_SIZE
}

impl AddressBus for Bus {
    fn read_byte(&mut self, a: u32) -> u8 {
        if in_rom(a) {
            self.rom[(a - ROM_BASE) as usize]
        } else if in_ram(a) {
            self.ram[(a - RAM_BASE) as usize]
        } else if in_low(a) {
            self.low[a as usize]
        } else {
            0xFF
        }
    }
    fn read_word(&mut self, a: u32) -> u16 {
        ((self.read_byte(a) as u16) << 8) | self.read_byte(a.wrapping_add(1)) as u16
    }
    fn read_long(&mut self, a: u32) -> u32 {
        ((self.read_word(a) as u32) << 16) | self.read_word(a.wrapping_add(2)) as u32
    }
    fn write_byte(&mut self, a: u32, v: u8) {
        if in_ram(a) {
            self.ram[(a - RAM_BASE) as usize] = v;
        } else if in_rom(a) {
            self.rom[(a - ROM_BASE) as usize] = v;
        } else if in_low(a) {
            self.low[a as usize] = v;
        }
    }
    fn write_word(&mut self, a: u32, v: u16) {
        self.write_byte(a, (v >> 8) as u8);
        self.write_byte(a.wrapping_add(1), (v & 0xFF) as u8);
    }
    fn write_long(&mut self, a: u32, v: u32) {
        self.write_word(a, (v >> 16) as u16);
        self.write_word(a.wrapping_add(2), (v & 0xFFFF) as u16);
    }
}
