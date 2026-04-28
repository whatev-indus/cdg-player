use crate::cdg::{AnyPacket, CdegInstruction, CdegPacket, Instruction, Packet, tile_channel};

pub const WIDTH: usize = 300;
pub const HEIGHT: usize = 216;

// Safe (visible) area starts at (6, 12) and is 288×192 — border surrounds it.
const BORDER_X: usize = 6;
const BORDER_Y: usize = 12;

// Tile dimensions (one packet paints one tile)
const TILE_W: usize = 6;
const TILE_H: usize = 12;

/// 16-color palette in packed 0x00RRGGBB format.
pub type Palette = [u32; 16];

// ── Per-plane CD+G graphics state ─────────────────────────────────────────────

/// Standard CD+G graphics state for one plane.
pub struct Screen {
    /// Color indices, one byte per pixel (values 0-15).
    pub pixels: Box<[u8; WIDTH * HEIGHT]>,
    pub palette: Palette,
    /// Border color index — rendered over the border zone at display time only.
    /// Never written into `pixels` so tile data drawn in the border survives scrolling.
    pub border_color: u8,
    /// Sub-tile horizontal display offset (0-5 px), set by scroll commands.
    pub h_offset: i8,
    /// Sub-tile vertical display offset (0-11 px), set by scroll commands.
    pub v_offset: i8,
}

impl Screen {
    pub fn new() -> Self {
        Self {
            pixels: Box::new([0u8; WIDTH * HEIGHT]),
            palette: [0u32; 16],
            border_color: 0,
            h_offset: 0,
            v_offset: 0,
        }
    }

    /// Apply a standard CD+G packet to this plane's graphics state.
    pub fn apply(&mut self, packet: &Packet) {
        match packet.instruction {
            Instruction::MemoryPreset => self.memory_preset(packet),
            Instruction::BorderPreset => self.border_preset(packet),
            Instruction::TileBlock => self.tile_block(&packet.data, false),
            Instruction::TileBlockXor => self.tile_block(&packet.data, true),
            Instruction::LoadColorTableLow => self.load_clut(&packet.data, 0),
            Instruction::LoadColorTableHigh => self.load_clut(&packet.data, 8),
            Instruction::ScrollPreset => self.scroll(packet, false),
            Instruction::ScrollCopy => self.scroll(packet, true),
            Instruction::DefineTransparent => {}
        }
    }

    /// Render the plane to a 32-bit 0x00RRGGBB framebuffer, applying scroll offsets.
    /// The border zone is always filled with `border_color`; pixel buffer data there
    /// is preserved so it scrolls into the interior correctly.
    pub fn render(&self, out: &mut [u32]) {
        let hoff = self.h_offset as isize;
        let voff = self.v_offset as isize;
        let border_rgb = self.palette[self.border_color as usize];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if y < BORDER_Y || y >= HEIGHT - BORDER_Y || x < BORDER_X || x >= WIDTH - BORDER_X {
                    out[y * WIDTH + x] = border_rgb;
                } else {
                    let sx = (x as isize - hoff).rem_euclid(WIDTH as isize) as usize;
                    let sy = (y as isize - voff).rem_euclid(HEIGHT as isize) as usize;
                    let idx = self.pixels[sy * WIDTH + sx] as usize;
                    out[y * WIDTH + x] = self.palette[idx];
                }
            }
        }
    }

    // ── Pub(crate) helpers used by CdegScreen ─────────────────────────────────

    /// Fill all pixels with `color` and reset scroll offsets (MemoryPreset for secondary).
    pub(crate) fn fill(&mut self, color: u8) {
        self.pixels.fill(color);
        self.h_offset = 0;
        self.v_offset = 0;
    }

    /// Paint a tile using raw data bytes (Item 2 SetFont / XorFont share this format).
    pub(crate) fn tile_block(&mut self, data: &[u8; 16], xor: bool) {
        let color0 = data[0] & 0x0F;
        let color1 = data[1] & 0x0F;
        let tile_row = (data[2] & 0x1F) as usize;
        let tile_col = (data[3] & 0x3F) as usize;

        let base_x = tile_col * TILE_W;
        let base_y = tile_row * TILE_H;
        if base_x + TILE_W > WIDTH || base_y + TILE_H > HEIGHT {
            return;
        }

        for row in 0..TILE_H {
            let byte = data[4 + row];
            for col in 0..TILE_W {
                let bit = (byte >> (5 - col)) & 0x01;
                let color = if bit == 0 { color0 } else { color1 };
                let px = &mut self.pixels[(base_y + row) * WIDTH + (base_x + col)];
                *px = if xor { *px ^ color } else { color };
            }
        }
    }

    /// Load 8 CLUT entries from raw data bytes (same layout as LoadColorTable commands).
    pub(crate) fn load_clut(&mut self, data: &[u8; 16], base: usize) {
        for i in 0..8 {
            let hi = data[i * 2] & 0x3F;
            let lo = data[i * 2 + 1] & 0x3F;
            let r4 = (hi >> 2) & 0x0F;
            let g4 = ((hi & 0x03) << 2) | ((lo >> 4) & 0x03);
            let b4 = lo & 0x0F;
            let r = (r4 * 17) as u32;
            let g = (g4 * 17) as u32;
            let b = (b4 * 17) as u32;
            self.palette[base + i] = (r << 16) | (g << 8) | b;
        }
    }

    // ── Private instruction handlers ──────────────────────────────────────────

    fn memory_preset(&mut self, p: &Packet) {
        self.fill(p.data[0] & 0x0F);
    }

    fn border_preset(&mut self, p: &Packet) {
        self.border_color = p.data[0] & 0x0F;
    }

    fn scroll(&mut self, p: &Packet, wrap: bool) {
        let h_cmd = (p.data[1] >> 4) & 0x03;
        let h_off = (p.data[1] & 0x07) as i8;
        let v_cmd = (p.data[2] >> 4) & 0x03;
        let v_off = (p.data[2] & 0x0F) as i8;
        let fill = p.data[0] & 0x0F;

        self.h_offset = h_off;
        self.v_offset = v_off;

        match h_cmd {
            1 => self.scroll_h(TILE_W as isize, fill, wrap),
            2 => self.scroll_h(-(TILE_W as isize), fill, wrap),
            _ => {}
        }
        match v_cmd {
            1 => self.scroll_v(TILE_H as isize, fill, wrap),
            2 => self.scroll_v(-(TILE_H as isize), fill, wrap),
            _ => {}
        }
    }

    fn scroll_h(&mut self, delta: isize, fill: u8, wrap: bool) {
        let mut new = [0u8; WIDTH * HEIGHT];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let src_x = (x as isize - delta).rem_euclid(WIDTH as isize) as usize;
                let in_vacated = if delta > 0 {
                    x < delta as usize
                } else {
                    x >= (WIDTH as isize + delta) as usize
                };
                new[y * WIDTH + x] = if in_vacated && !wrap {
                    fill
                } else {
                    self.pixels[y * WIDTH + src_x]
                };
            }
        }
        *self.pixels = new;
    }

    fn scroll_v(&mut self, delta: isize, fill: u8, wrap: bool) {
        let mut new = [0u8; WIDTH * HEIGHT];
        for y in 0..HEIGHT {
            let src_y = (y as isize - delta).rem_euclid(HEIGHT as isize) as usize;
            let in_vacated = if delta > 0 {
                y < delta as usize
            } else {
                y >= (HEIGHT as isize + delta) as usize
            };
            for x in 0..WIDTH {
                new[y * WIDTH + x] = if in_vacated && !wrap {
                    fill
                } else {
                    self.pixels[src_y * WIDTH + x]
                };
            }
        }
        *self.pixels = new;
    }
}

// ── CD+EG mode registers ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    NoWrite,
    Primary,
    Secondary,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// 256-color: `clut256[primary_idx | (secondary_idx << 4)]`
    Color256,
    /// Show only the primary plane with its 16-color palette.
    Primary,
    /// Show only the secondary plane with its 16-color palette.
    Secondary,
    /// Mix: `mix(primary_palette[pi], secondary_palette[si])` per pixel.
    Mix,
}

/// 6-bit-per-channel color for the 256-entry CD+EG extended CLUT.
/// High 4 bits loaded by `LoadClut256High`, low 2 bits by `LoadClut256Low`.
#[derive(Clone, Copy, Default)]
struct CdegColor {
    r: u8, // 6-bit
    g: u8,
    b: u8,
}

impl CdegColor {
    fn to_rgb32(self) -> u32 {
        let r = (self.r << 2) as u32;
        let g = (self.g << 2) as u32;
        let b = (self.b << 2) as u32;
        (r << 16) | (g << 8) | b
    }
}

// ── CD+EG composite screen ────────────────────────────────────────────────────

/// Full CD+EG graphics state: two independent planes, mode registers, and an
/// extended 256-entry CLUT.  When `cdeg_enabled` is false, Item 2 packets are
/// ignored and the state behaves identically to a standard CD+G decoder.
pub struct CdegScreen {
    pub primary: Screen,
    pub secondary: Screen,
    pub write_mode: WriteMode,
    pub display_mode: DisplayMode,
    pub cdeg_enabled: bool,
    /// Which of the 16 tile channels are enabled for rendering.
    pub active_channels: [bool; 16],
    clut256: Box<[CdegColor; 256]>,
}

impl CdegScreen {
    pub fn new(cdeg_enabled: bool) -> Self {
        let mut active_channels = [false; 16];
        active_channels[0] = true;
        active_channels[1] = true;
        Self {
            primary: Screen::new(),
            secondary: Screen::new(),
            write_mode: WriteMode::Primary,
            display_mode: DisplayMode::Primary,
            cdeg_enabled,
            active_channels,
            clut256: Box::new([CdegColor::default(); 256]),
        }
    }

    /// Apply a packet to the graphics state.
    pub fn apply(&mut self, pkt: &AnyPacket) {
        match pkt {
            AnyPacket::Item1(p) => self.apply_item1(p),
            AnyPacket::Item2(p) => {
                if self.cdeg_enabled {
                    self.apply_item2(p);
                }
            }
        }
    }

    /// Render to a 32-bit 0x00RRGGBB framebuffer according to current display mode.
    pub fn render(&self, out: &mut [u32]) {
        match self.display_mode {
            DisplayMode::Primary => self.primary.render(out),
            DisplayMode::Secondary => self.secondary.render(out),
            DisplayMode::Color256 => self.render_256color(out),
            DisplayMode::Mix => self.render_mix(out),
        }
    }

    // ── Item 1 dispatch ───────────────────────────────────────────────────────

    fn apply_item1(&mut self, pkt: &Packet) {
        if matches!(pkt.instruction, Instruction::TileBlock | Instruction::TileBlockXor) {
            if !self.active_channels[tile_channel(&pkt.data)] {
                return;
            }
        }
        if matches!(self.display_mode, DisplayMode::Color256) {
            // 256-color mode: primary gets normal writes; secondary gets special preset handling.
            match self.write_mode {
                WriteMode::Both => {
                    match pkt.instruction {
                        Instruction::MemoryPreset
                        | Instruction::BorderPreset
                        | Instruction::TileBlock
                        | Instruction::TileBlockXor => self.primary.apply(pkt),
                        _ => {}
                    }
                    match pkt.instruction {
                        Instruction::MemoryPreset => self.secondary.fill(0),
                        Instruction::BorderPreset => self.secondary.border_color = 0,
                        _ => {}
                    }
                }
                WriteMode::NoWrite => {}
                // Primary/Secondary shouldn't occur in 256-color mode per spec,
                // but fall through gracefully.
                WriteMode::Primary => self.primary.apply(pkt),
                WriteMode::Secondary => self.secondary.apply(pkt),
            }
        } else {
            if matches!(self.write_mode, WriteMode::Primary | WriteMode::Both) {
                self.primary.apply(pkt);
            }
            if matches!(self.write_mode, WriteMode::Secondary | WriteMode::Both) {
                self.secondary.apply(pkt);
            }
        }
    }

    // ── Item 2 dispatch ───────────────────────────────────────────────────────

    fn apply_item2(&mut self, pkt: &CdegPacket) {
        // MemoryControl is always handled regardless of current display mode.
        if let CdegInstruction::MemoryControl = pkt.instruction {
            self.memory_control(&pkt.data);
            return;
        }

        let is_256color = matches!(self.display_mode, DisplayMode::Color256);

        // The first two Item 2 CLUT commands (instructions 16 and 17, i.e. start == 0 or 8)
        // are mode 2 commands deliberately chosen because CDG decoders ignore them entirely.
        // On CD+EG, they drive dissolve/cross-fade effects by manipulating the primary and
        // secondary 16-colour palettes — the only case where a command does something on
        // CD+EG that is a no-op on CDG (rather than the usual reverse).
        let is_early_clut = matches!(
            pkt.instruction,
            CdegInstruction::LoadClut256High { start: 0 | 8 }
        );

        if !is_256color && !is_early_clut {
            return;
        }

        match pkt.instruction {
            CdegInstruction::MemoryControl => unreachable!(),

            CdegInstruction::SetFont => {
                if self.active_channels[tile_channel(&pkt.data)] {
                    self.secondary.tile_block(&pkt.data, false);
                }
            }
            CdegInstruction::XorFont => {
                if self.active_channels[tile_channel(&pkt.data)] {
                    self.secondary.tile_block(&pkt.data, true);
                }
            }

            CdegInstruction::LoadClut256High { start } => {
                if !is_256color {
                    // Not 256-colour mode: apply to the 16-colour primary/secondary palettes
                    // to produce the dissolve/cross-fade effect intended by the disc author.
                    let base = start as usize; // 0 or 8
                    if matches!(self.write_mode, WriteMode::Primary | WriteMode::Both) {
                        self.primary.load_clut(&pkt.data, base);
                    }
                    if matches!(self.write_mode, WriteMode::Secondary | WriteMode::Both) {
                        self.secondary.load_clut(&pkt.data, base);
                    }
                } else {
                    self.load_clut256_high(start as usize, &pkt.data);
                }
            }

            CdegInstruction::LoadClut256Low { start } => {
                if is_256color {
                    self.load_clut256_low(start as usize, &pkt.data);
                }
            }
        }
    }

    // ── CD+EG CLUT loaders ────────────────────────────────────────────────────

    fn memory_control(&mut self, data: &[u8; 16]) {
        let mode = data[0];
        let write = mode & 0x03;
        let display = (mode >> 2) & 0x03;

        // In 256-color mode only NoWrite (0) or Both (3) are valid write modes.
        if display == 0 && (write == 1 || write == 2) {
            return;
        }

        self.write_mode = match write {
            0 => WriteMode::NoWrite,
            1 => WriteMode::Primary,
            2 => WriteMode::Secondary,
            _ => WriteMode::Both,
        };
        self.display_mode = match display {
            0 => DisplayMode::Color256,
            1 => DisplayMode::Primary,
            2 => DisplayMode::Secondary,
            _ => DisplayMode::Mix,
        };
    }

    /// Load high 4 bits of 8 consecutive CLUT entries from a 16-byte data block.
    /// Uses the same 4-bit RGB layout as the standard LoadColorTable commands.
    fn load_clut256_high(&mut self, start: usize, data: &[u8; 16]) {
        for i in 0..8 {
            let idx = start + i;
            if idx >= 256 {
                break;
            }
            let hi = data[i * 2] & 0x3F;
            let lo = data[i * 2 + 1] & 0x3F;
            let r4 = (hi >> 2) & 0x0F;
            let g4 = ((hi & 0x03) << 2) | ((lo >> 4) & 0x03);
            let b4 = lo & 0x0F;
            let c = &mut self.clut256[idx];
            // Place the 4-bit values into the upper 4 bits of the 6-bit field.
            c.r = (c.r & 0x03) | (r4 << 2);
            c.g = (c.g & 0x03) | (g4 << 2);
            c.b = (c.b & 0x03) | (b4 << 2);
        }
    }

    /// Load low 2 bits of 16 consecutive CLUT entries from a 16-byte data block.
    /// Each byte packs r[1:0] g[1:0] b[1:0] in bits 5-0.
    fn load_clut256_low(&mut self, start: usize, data: &[u8; 16]) {
        for i in 0..16 {
            let idx = start + i;
            if idx >= 256 {
                break;
            }
            let byte = data[i];
            let c = &mut self.clut256[idx];
            c.r = (c.r & 0x3C) | ((byte >> 4) & 0x03);
            c.g = (c.g & 0x3C) | ((byte >> 2) & 0x03);
            c.b = (c.b & 0x3C) | (byte & 0x03);
        }
    }

    // ── Rendering modes ───────────────────────────────────────────────────────

    fn render_256color(&self, out: &mut [u32]) {
        let h1 = self.primary.h_offset as isize;
        let v1 = self.primary.v_offset as isize;
        let h2 = self.secondary.h_offset as isize;
        let v2 = self.secondary.v_offset as isize;
        let border_rgb = self.clut256
            [self.primary.border_color as usize | ((self.secondary.border_color as usize) << 4)]
            .to_rgb32();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if y < BORDER_Y || y >= HEIGHT - BORDER_Y || x < BORDER_X || x >= WIDTH - BORDER_X {
                    out[y * WIDTH + x] = border_rgb;
                    continue;
                }
                let sx1 = (x as isize - h1).rem_euclid(WIDTH as isize) as usize;
                let sy1 = (y as isize - v1).rem_euclid(HEIGHT as isize) as usize;
                let sx2 = (x as isize - h2).rem_euclid(WIDTH as isize) as usize;
                let sy2 = (y as isize - v2).rem_euclid(HEIGHT as isize) as usize;
                let p_idx = self.primary.pixels[sy1 * WIDTH + sx1] as usize;
                let s_idx = self.secondary.pixels[sy2 * WIDTH + sx2] as usize;
                out[y * WIDTH + x] = self.clut256[p_idx | (s_idx << 4)].to_rgb32();
            }
        }
    }

    fn render_mix(&self, out: &mut [u32]) {
        let h1 = self.primary.h_offset as isize;
        let v1 = self.primary.v_offset as isize;
        let h2 = self.secondary.h_offset as isize;
        let v2 = self.secondary.v_offset as isize;

        // Mix: sum the 4-bit channel values and scale to 8-bit.
        // Our palette stores `r4 * 17`; recover 4-bit then apply hcs64's formula:
        // mixed = (p4 + s4) * 16 clamped to 255.
        let mix = |p8: u32, s8: u32| -> u32 { ((p8 / 17 + s8 / 17) * 16).min(255) };

        let bp = self.primary.palette[self.primary.border_color as usize];
        let bs = self.secondary.palette[self.secondary.border_color as usize];
        let border_rgb = (mix((bp >> 16) & 0xFF, (bs >> 16) & 0xFF) << 16)
            | (mix((bp >> 8) & 0xFF, (bs >> 8) & 0xFF) << 8)
            | mix(bp & 0xFF, bs & 0xFF);

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if y < BORDER_Y || y >= HEIGHT - BORDER_Y || x < BORDER_X || x >= WIDTH - BORDER_X {
                    out[y * WIDTH + x] = border_rgb;
                    continue;
                }
                let sx1 = (x as isize - h1).rem_euclid(WIDTH as isize) as usize;
                let sy1 = (y as isize - v1).rem_euclid(HEIGHT as isize) as usize;
                let sx2 = (x as isize - h2).rem_euclid(WIDTH as isize) as usize;
                let sy2 = (y as isize - v2).rem_euclid(HEIGHT as isize) as usize;

                let p_rgb = self.primary.palette[self.primary.pixels[sy1 * WIDTH + sx1] as usize];
                let s_rgb =
                    self.secondary.palette[self.secondary.pixels[sy2 * WIDTH + sx2] as usize];

                let r = mix((p_rgb >> 16) & 0xFF, (s_rgb >> 16) & 0xFF);
                let g = mix((p_rgb >> 8) & 0xFF, (s_rgb >> 8) & 0xFF);
                let b = mix(p_rgb & 0xFF, s_rgb & 0xFF);
                out[y * WIDTH + x] = (r << 16) | (g << 8) | b;
            }
        }
    }
}
