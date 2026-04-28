/// CD+G / CD+EG subchannel packet — 24 bytes raw, 16 bytes of usable data.
/// The Q/R/S/T/U/V/W subchannel bytes interleaved on a CD produce these
/// packets at a rate of 300 per second (75 sectors/sec × 4 packets/sector).
pub const PACKET_SIZE: usize = 24;
pub const PACKETS_PER_SECOND: u32 = 300;

/// Item 1: standard CD+G packets (command byte bits 5-0 = 0x09)
const CDG_ITEM1: u8 = 0x09;
/// Item 2: CD+EG extension packets (command byte bits 5-0 = 0x0A)
const CDG_ITEM2: u8 = 0x0A;

// ── Standard CD+G (Item 1) instructions ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    MemoryPreset,       // 1  — fill screen with colour index
    BorderPreset,       // 2  — fill border with colour index
    TileBlock,          // 6  — paint 12×6 tile (normal)
    ScrollPreset,       // 20 — scroll and fill with colour
    ScrollCopy,         // 24 — scroll and wrap
    DefineTransparent,  // 28 — define transparent colour (rarely used)
    LoadColorTableLow,  // 30 — load palette entries 0-7
    LoadColorTableHigh, // 31 — load palette entries 8-15
    TileBlockXor,       // 38 — paint 12×6 tile (XOR mode)
}

impl Instruction {
    fn from_byte(b: u8) -> Option<Self> {
        match b & 0x3F {
            1 => Some(Self::MemoryPreset),
            2 => Some(Self::BorderPreset),
            6 => Some(Self::TileBlock),
            20 => Some(Self::ScrollPreset),
            24 => Some(Self::ScrollCopy),
            28 => Some(Self::DefineTransparent),
            30 => Some(Self::LoadColorTableLow),
            31 => Some(Self::LoadColorTableHigh),
            38 => Some(Self::TileBlockXor),
            _ => None,
        }
    }
}

/// Parsed representation of a single CD+G (Item 1) subchannel packet.
#[derive(Debug, Clone)]
pub struct Packet {
    pub instruction: Instruction,
    /// 16 data bytes (bits 5-0 of the raw data bytes, parity bits stripped).
    pub data: [u8; 16],
}

impl Packet {
    fn parse(raw: &[u8; PACKET_SIZE]) -> Option<Self> {
        let instruction = Instruction::from_byte(raw[1])?;
        let mut data = [0u8; 16];
        for (i, b) in raw[4..20].iter().enumerate() {
            data[i] = b & 0x3F;
        }
        Some(Packet { instruction, data })
    }
}

// ── CD+EG (Item 2) instructions ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdegInstruction {
    /// Instruction 3 — set write/display mode registers.
    MemoryControl,
    /// Instruction 6  — write tile to secondary plane (normal).
    SetFont,
    /// Instruction 14 — write tile to secondary plane (XOR).
    XorFont,
    /// Instructions 16-47 — load high 4 bits of 8 entries in the 256-entry CLUT.
    /// `start` is the first CLUT index to update (multiple of 8, 0-248).
    LoadClut256High { start: u8 },
    /// Instructions 48-63 — load low 2 bits of 16 entries in the 256-entry CLUT.
    /// `start` is the first CLUT index to update (multiple of 16, 0-240).
    LoadClut256Low { start: u8 },
}

impl CdegInstruction {
    fn from_byte(b: u8) -> Option<Self> {
        match b & 0x3F {
            3 => Some(Self::MemoryControl),
            6 => Some(Self::SetFont),
            14 => Some(Self::XorFont),
            i @ 16..=47 => Some(Self::LoadClut256High {
                start: (i - 16) * 8,
            }),
            i @ 48..=63 => Some(Self::LoadClut256Low {
                start: (i - 48) * 16,
            }),
            _ => None,
        }
    }
}

/// Parsed representation of a CD+EG (Item 2) subchannel packet.
#[derive(Debug, Clone)]
pub struct CdegPacket {
    pub instruction: CdegInstruction,
    /// 16 data bytes (bits 5-0 of the raw data bytes, parity bits stripped).
    pub data: [u8; 16],
}

/// Either a standard CD+G packet (Item 1) or a CD+EG extension packet (Item 2).
#[derive(Debug, Clone)]
pub enum AnyPacket {
    Item1(Packet),
    Item2(CdegPacket),
}

/// Extract the 4-bit channel number from a tile packet's first two data bytes.
/// Bits 5-4 of data[0] supply the high 2 bits; bits 5-4 of data[1] supply the low 2 bits.
pub fn tile_channel(data: &[u8; 16]) -> usize {
    (((data[0] & 0x30) >> 2) | ((data[1] & 0x30) >> 4)) as usize
}

/// Scan a raw CDG byte slice and return a 16-element mask of which channels
/// have at least one TileBlock / XorFont packet addressed to them.
pub fn channels_present(data: &[u8]) -> [bool; 16] {
    let mut present = [false; 16];
    for (_, pkt) in PacketIter::new(data) {
        match pkt {
            Some(AnyPacket::Item1(p))
                if matches!(p.instruction, Instruction::TileBlock | Instruction::TileBlockXor) =>
            {
                present[tile_channel(&p.data)] = true;
            }
            Some(AnyPacket::Item2(p))
                if matches!(
                    p.instruction,
                    CdegInstruction::SetFont | CdegInstruction::XorFont
                ) =>
            {
                present[tile_channel(&p.data)] = true;
            }
            _ => {}
        }
    }
    present
}

// ── Packet iterator ───────────────────────────────────────────────────────────

/// Iterator that yields parsed packets from a raw `.cdg` byte slice.
pub struct PacketIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PacketIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for PacketIter<'a> {
    type Item = (u32, Option<AnyPacket>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + PACKET_SIZE > self.data.len() {
            return None;
        }
        let raw: &[u8; PACKET_SIZE] = self.data[self.pos..self.pos + PACKET_SIZE]
            .try_into()
            .unwrap();
        let index = (self.pos / PACKET_SIZE) as u32;
        self.pos += PACKET_SIZE;

        let cmd = raw[0] & 0x3F;
        let packet = if cmd == CDG_ITEM1 {
            Packet::parse(raw).map(AnyPacket::Item1)
        } else if cmd == CDG_ITEM2 {
            let instr = CdegInstruction::from_byte(raw[1])?;
            let mut data = [0u8; 16];
            for (i, b) in raw[4..20].iter().enumerate() {
                data[i] = b & 0x3F;
            }
            Some(AnyPacket::Item2(CdegPacket {
                instruction: instr,
                data,
            }))
        } else {
            None
        };

        Some((index, packet))
    }
}
