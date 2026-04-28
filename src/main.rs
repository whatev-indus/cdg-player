#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod cdg;
mod config;
mod cue;
mod export;
mod icons;
mod renderer;

use cdg::{AnyPacket, PACKETS_PER_SECOND, PacketIter, channels_present};
use config::{Config, DiscEntry, DiscSource, scan_library};
use cue::{CHANNELS, SAMPLE_RATE};
use export::{CancelToken, ExportState, Progress};
use renderer::{CdegScreen, HEIGHT, WIDTH};

use eframe::egui;
use egui::{ColorImage, TextureHandle, TextureOptions};
use rodio::{OutputStream, OutputStreamHandle, Sink, buffer::SamplesBuffer};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// ── Playback ──────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum PlayState {
    Playing,
    Paused,
    Stopped,
}

struct Player {
    packets: Vec<(u32, Option<AnyPacket>)>,
    screen: CdegScreen,
    state: PlayState,
    packet_idx: usize,
    epoch: Instant,
    paused_at: Option<Instant>,
    total_packets: usize,
    _stream: OutputStream,
    sink: Sink,
    audio_samples: Vec<i16>,
    /// True if this disc contains Item 2 (CD+EG) packets.
    pub is_cdeg: bool,
    /// Which of the 16 tile channels are present on this disc.
    pub channels_present: [bool; 16],
}

impl Player {
    fn new(track: &cue::Track, cdg_path: &PathBuf, cdeg_enabled: bool) -> Self {
        let cdg_raw = std::fs::read(cdg_path).unwrap_or_default();
        let cdg_offset = track.cdg_offset() as usize;
        // Limit CDG data to this track's sectors only (4 packets × 24 bytes each).
        let cdg_end =
            (cdg_offset + track.sectors as usize * 4 * cdg::PACKET_SIZE).min(cdg_raw.len());
        let cdg_data = &cdg_raw[cdg_offset.min(cdg_raw.len())..cdg_end];
        let packets: Vec<_> = PacketIter::new(cdg_data).collect();
        let total = packets.len();
        let disc_channels = channels_present(cdg_data);

        // Auto-detect whether this disc has any CD+EG (Item 2) data.
        let has_cdeg = packets
            .iter()
            .any(|(_, p)| matches!(p, Some(AnyPacket::Item2(_))));
        let cdeg_on = cdeg_enabled && has_cdeg;

        let audio_samples = track.load_audio();
        let (_stream, stream_handle) = OutputStream::try_default().expect("audio output");
        let sink = make_sink(&stream_handle, &audio_samples);
        Player {
            packets,
            screen: CdegScreen::new(cdeg_on),
            state: PlayState::Playing,
            packet_idx: 0,
            epoch: Instant::now(),
            paused_at: None,
            total_packets: total,
            _stream,
            sink,
            audio_samples,
            is_cdeg: has_cdeg,
            channels_present: disc_channels,
        }
    }

    /// Audio-only player — no CDG packets, just audio. Used when no .cdg is present.
    fn audio_only(track: &cue::Track, cdeg_enabled: bool) -> Self {
        let audio_samples = track.load_audio();
        let (_stream, stream_handle) = OutputStream::try_default().expect("audio output");
        let sink = make_sink(&stream_handle, &audio_samples);
        // Drive duration from audio length instead of packet count.
        let total_packets = (audio_samples.len() as f64
            / (cue::SAMPLE_RATE as f64 * cue::CHANNELS as f64)
            * PACKETS_PER_SECOND as f64) as usize;
        Player {
            packets: vec![],
            screen: CdegScreen::new(cdeg_enabled),
            state: PlayState::Playing,
            packet_idx: 0,
            epoch: Instant::now(),
            paused_at: None,
            total_packets,
            _stream,
            sink,
            audio_samples,
            is_cdeg: false,
            channels_present: [false; 16],
        }
    }

    fn elapsed(&self) -> Duration {
        let elapsed = match self.paused_at {
            Some(t) => t.duration_since(self.epoch),
            None => self.epoch.elapsed(),
        };
        elapsed.min(self.total_duration())
    }

    fn total_duration(&self) -> Duration {
        Duration::from_secs_f64(self.total_packets as f64 / PACKETS_PER_SECOND as f64)
    }

    fn seek_to(&mut self, target: usize) {
        let target = target.min(self.total_packets);
        let channels = self.screen.active_channels;
        self.screen = CdegScreen::new(self.screen.cdeg_enabled);
        self.screen.active_channels = channels;
        for i in 0..target {
            if let (_, Some(ref pkt)) = self.packets[i] {
                self.screen.apply(pkt);
            }
        }
        self.packet_idx = target;
        let offset = Duration::from_secs_f64(target as f64 / PACKETS_PER_SECOND as f64);
        self.epoch = Instant::now() - offset;
        if self.paused_at.is_some() {
            self.paused_at = Some(Instant::now());
        }
        self.sink.stop();
        let sample_pos = (target as f64 / PACKETS_PER_SECOND as f64 * SAMPLE_RATE as f64) as usize
            * CHANNELS as usize;
        if !self.audio_samples.is_empty() && sample_pos < self.audio_samples.len() {
            self.sink.append(SamplesBuffer::new(
                CHANNELS,
                SAMPLE_RATE,
                self.audio_samples[sample_pos..].to_vec(),
            ));
            if self.state == PlayState::Paused {
                self.sink.pause();
            }
        }
    }

    fn play(&mut self) {
        if self.state == PlayState::Stopped {
            self.seek_to(0);
        }
        if let Some(paused_at) = self.paused_at.take() {
            self.epoch += Instant::now() - paused_at;
        }
        self.state = PlayState::Playing;
        self.sink.play();
    }

    fn pause(&mut self) {
        if self.state == PlayState::Playing {
            self.paused_at = Some(Instant::now());
            self.state = PlayState::Paused;
            self.sink.pause();
        }
    }

    fn stop(&mut self) {
        self.sink.stop();
        let channels = self.screen.active_channels;
        self.screen = CdegScreen::new(self.screen.cdeg_enabled);
        self.screen.active_channels = channels;
        self.packet_idx = 0;
        self.epoch = Instant::now();
        self.paused_at = Some(Instant::now());
        self.state = PlayState::Stopped;
    }

    fn finish(&mut self) {
        self.packet_idx = self.total_packets;
        self.paused_at = Some(self.epoch + self.total_duration());
        self.state = PlayState::Stopped;
    }

    fn tick(&mut self) -> bool {
        if self.state != PlayState::Playing {
            return false;
        }
        let due = (self.epoch.elapsed().as_secs_f64() * PACKETS_PER_SECOND as f64) as usize;
        let due = due.min(self.total_packets);
        while self.packet_idx < due && self.packet_idx < self.packets.len() {
            if let (_, Some(ref pkt)) = self.packets[self.packet_idx] {
                self.screen.apply(pkt);
            }
            self.packet_idx += 1;
        }
        if self.packet_idx >= self.total_packets {
            self.finish();
            true
        } else {
            false
        }
    }
}

fn make_sink(handle: &OutputStreamHandle, samples: &[i16]) -> Sink {
    let sink = Sink::try_new(handle).expect("sink");
    if !samples.is_empty() {
        sink.append(SamplesBuffer::new(CHANNELS, SAMPLE_RATE, samples.to_vec()));
    }
    sink
}

// ── App ───────────────────────────────────────────────────────────────────────

struct App {
    config: Config,
    library: Vec<DiscEntry>,
    player: Option<Player>,
    tracks: Vec<cue::Track>,
    cdg_path: Option<PathBuf>,
    track_idx: usize,
    texture: Option<TextureHandle>,
    volume: f32,
    export_progress: Option<Progress>,
    export_cancel: Option<CancelToken>,
    /// Whether to enable CD+EG decoding on supported discs.
    cdeg_enabled: bool,
    /// Which of the 16 tile channels are active for playback and export.
    active_channels: [bool; 16],
    /// Temp directory created when a disc was loaded from a ZIP.
    /// Deleted when a new disc is loaded or the app exits.
    zip_temp_dir: Option<PathBuf>,
    /// Error message to display in the central panel (dismissed on next open).
    open_error: Option<String>,
}

impl Drop for App {
    fn drop(&mut self) {
        self.cleanup_zip_temp();
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(egui::Color32::from_gray(220));
        cc.egui_ctx.set_visuals(visuals);
        let config = Config::load();
        let library = config
            .library_path
            .as_deref()
            .map(scan_library)
            .unwrap_or_default();
        let mut active_channels = [false; 16];
        active_channels[0] = true;
        active_channels[1] = true;
        let mut app = App {
            config,
            library,
            player: None,
            tracks: vec![],
            cdg_path: None,
            track_idx: 0,
            texture: None,
            volume: 1.0,
            export_progress: None,
            export_cancel: None,
            cdeg_enabled: true,
            active_channels,
            zip_temp_dir: None,
            open_error: None,
        };
        // Optional CLI args: <cue> <track#> <cdg>
        let args: Vec<String> = std::env::args().collect();
        if args.len() == 4 {
            let tracks = cue::parse_cue(&PathBuf::from(&args[1]));
            let track_num: u32 = args[2].parse().unwrap_or(1);
            let idx = tracks
                .iter()
                .position(|t| t.number == track_num)
                .unwrap_or(0);
            app.cdg_path = Some(PathBuf::from(&args[3]));
            app.tracks = tracks;
            app.load_track(idx);
        }
        app
    }

    fn open_disc_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Disc image", &["cue", "zip", "7z"])
            .pick_file()
        else {
            return;
        };

        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("zip") => self.open_zip(path),
            Some("7z") => self.open_7z(path),
            _ => self.load_cue(path),
        }
    }

    fn cleanup_zip_temp(&mut self) {
        if let Some(ref dir) = self.zip_temp_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        self.zip_temp_dir = None;
    }

    fn open_zip(&mut self, zip_path: PathBuf) {
        self.open_error = None;
        match extract_disc_zip(&zip_path) {
            Ok((temp_dir, cue_path)) => {
                self.cleanup_zip_temp();
                self.zip_temp_dir = Some(temp_dir);
                self.load_cue(cue_path);
            }
            Err(e) => {
                self.open_error = Some(e);
            }
        }
    }

    fn open_7z(&mut self, path: PathBuf) {
        self.open_error = None;
        match extract_disc_7z(&path) {
            Ok((temp_dir, cue_path)) => {
                self.cleanup_zip_temp();
                self.zip_temp_dir = Some(temp_dir);
                self.load_cue(cue_path);
            }
            Err(e) => {
                self.open_error = Some(e);
            }
        }
    }

    fn load_cue(&mut self, cue_path: PathBuf) {
        // If we're loading a plain .cue (not from a ZIP), drop any previous temp dir.
        if self
            .zip_temp_dir
            .as_ref()
            .map_or(true, |d| !cue_path.starts_with(d))
        {
            self.cleanup_zip_temp();
        }

        let cdg = cue_path.with_extension("cdg");
        let cdg_path = {
            // Resolve the CDG path, then discard it if the file is empty.
            let candidate = if cdg.exists() {
                Some(cdg)
            } else {
                // CDG file name doesn't match the cue — find any .cdg in the same folder.
                cue_path.parent().and_then(|dir| {
                    std::fs::read_dir(dir)
                        .ok()?
                        .flatten()
                        .find(|e| {
                            e.path()
                                .extension()
                                .and_then(|x| x.to_str())
                                .map_or(false, |x| x.eq_ignore_ascii_case("cdg"))
                        })
                        .map(|e| e.path())
                })
            };
            candidate.filter(|p| p.metadata().map(|m| m.len() > 0).unwrap_or(false))
        };

        self.cdg_path = cdg_path;
        self.tracks = cue::parse_cue(&cue_path);
        self.track_idx = 0;
        self.player = None;
        self.texture = None;

        // Scan track 0 to find which channels are present, then set defaults:
        // enable 0 & 1 if both exist, otherwise enable 0 only.
        let disc_ch = if let (Some(cdg_path), Some(track)) =
            (&self.cdg_path, self.tracks.first())
        {
            std::fs::read(cdg_path)
                .map(|raw| {
                    let off = track.cdg_offset() as usize;
                    let end =
                        (off + track.sectors as usize * 4 * cdg::PACKET_SIZE).min(raw.len());
                    channels_present(&raw[off.min(raw.len())..end])
                })
                .unwrap_or([false; 16])
        } else {
            [false; 16]
        };
        let mut active = [false; 16];
        active[0] = true;
        if disc_ch[0] && disc_ch[1] {
            active[1] = true;
        }
        self.active_channels = active;

        self.load_track(0);
    }

    fn load_track(&mut self, idx: usize) {
        let Some(track) = self.tracks.get(idx) else {
            return;
        };
        self.track_idx = idx;

        if let Some(ref cdg) = self.cdg_path {
            let mut player = Player::new(track, cdg, self.cdeg_enabled);
            player.screen.active_channels = self.active_channels;
            player.sink.set_volume(self.volume);
            self.player = Some(player);
        } else {
            // No .cdg — audio-only player (no video packets).
            let mut player = Player::audio_only(track, self.cdeg_enabled);
            player.screen.active_channels = self.active_channels;
            player.sink.set_volume(self.volume);
            self.player = Some(player);
        }
    }

    fn return_to_library(&mut self) {
        if let Some(ref mut p) = self.player {
            p.stop();
        }
        self.player = None;
        self.tracks = vec![];
        self.cdg_path = None;
        self.texture = None;
        self.cleanup_zip_temp();
    }

    fn refresh_library(&mut self) {
        self.library = self
            .config
            .library_path
            .as_deref()
            .map(scan_library)
            .unwrap_or_default();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut advance_to_track = None;
        let mut return_to_library = false;
        if let Some(ref mut p) = self.player {
            if p.tick() {
                if self.track_idx + 1 < self.tracks.len() {
                    advance_to_track = Some(self.track_idx + 1);
                } else {
                    return_to_library = true;
                }
            }
        }

        if let Some(next_idx) = advance_to_track {
            self.load_track(next_idx);
        } else if return_to_library {
            self.return_to_library();
        }

        // ── Bottom toolbar ────────────────────────────────────────────────
        let toolbar = egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(3.0);
            let in_library = self.player.is_none();

            // ── Row 1: open | library | transport | track/export ─────────
            ui.horizontal(|ui| {
                let body_font = egui::TextStyle::Body.resolve(ui.style());
                let bp = ui.spacing().button_padding;
                let sp = ui.spacing().item_spacing.x;
                let avail = ui.available_width();

                let measure_btn = |text: &str| -> f32 {
                    ui.fonts(|f| {
                        f.layout_no_wrap(
                            text.to_string(),
                            body_font.clone(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                    }) + bp.x * 2.0
                };

                let exporting = matches!(
                    self.export_progress
                        .as_ref()
                        .and_then(|p| p.lock().ok())
                        .as_deref(),
                    Some(ExportState::Running { .. })
                );
                let can_export = !self.tracks.is_empty() && self.cdg_path.is_some();

                if in_library {
                    let total_w = measure_btn("Open") + sp + measure_btn("Set Library");
                    ui.add_space(((avail - total_w) / 2.0).max(0.0));

                    if ui.button("Open").clicked() {
                        self.open_disc_dialog();
                    }

                    if ui.button("Set Library").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.config.set_library(dir);
                            self.refresh_library();
                        }
                    }
                } else {
                    let status_text = self.export_progress.as_ref().and_then(|prog| {
                        match &*prog.lock().ok()? {
                            ExportState::Running { track_idx, total, frame_frac } => Some(format!(
                                "Track {}/{} - {:.0}%",
                                track_idx + 1,
                                total,
                                frame_frac * 100.0
                            )),
                            ExportState::Done => Some("Export done.".to_string()),
                            ExportState::Error(e) => Some(e.clone()),
                            ExportState::Idle => None,
                        }
                    });
                    let mut total_w = measure_btn("Open")
                        + sp
                        + measure_btn("Library")
                        + sp
                        + 8.0
                        + sp
                        + measure_btn("⏮")
                        + sp
                        + measure_btn("⏸")
                        + sp
                        + measure_btn("⏭");
                    if !self.tracks.is_empty() {
                        total_w += sp + 8.0 + sp + 70.0 + sp + 8.0;
                    }
                    total_w += sp
                        + if exporting {
                            measure_btn("Cancel")
                        } else {
                            measure_btn("Export Track") + sp + measure_btn("Export Album")
                        };
                    if status_text.is_some() {
                        total_w += sp + 8.0;
                    }
                    ui.add_space(((avail - total_w) / 2.0).max(0.0));

                    if ui.button("Open").clicked() {
                        self.open_disc_dialog();
                    }

                    if ui.button("Library").clicked() {
                        return_to_library = true;
                    }

                    if !return_to_library {
                        ui.label("|");

                        let is_playing = {
                            let p = self.player.as_ref().unwrap();
                            p.state == PlayState::Playing
                        };
                        let cdeg_on = self
                            .player
                            .as_ref()
                            .map(|p| p.screen.cdeg_enabled)
                            .unwrap_or(self.cdeg_enabled);
                        let disc_title = self
                            .cdg_path
                            .as_ref()
                            .and_then(|p| p.file_stem())
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "Disc".to_string());

                        if !self.tracks.is_empty() {
                            let track_label =
                                format!("Track {:02}", self.tracks[self.track_idx].number);
                            egui::ComboBox::from_id_salt("track_select")
                                .selected_text(track_label)
                                .width(70.0)
                                .show_ui(ui, |ui| {
                                    for i in 0..self.tracks.len() {
                                        let l = format!("Track {:02}", self.tracks[i].number);
                                        if ui.selectable_label(self.track_idx == i, l).clicked() {
                                            self.load_track(i);
                                        }
                                    }
                                });

                            ui.label("|");
                        }

                        let can_prev = self.track_idx > 0;
                        if ui.add_enabled(can_prev, egui::Button::new("⏮")).clicked() {
                            self.load_track(self.track_idx - 1);
                        }

                        let play_label = if is_playing { "⏸" } else { "▶" };
                        if ui.button(play_label).clicked() {
                            if is_playing {
                                self.player.as_mut().unwrap().pause();
                            } else {
                                self.player.as_mut().unwrap().play();
                            }
                        }

                        let can_next = self.track_idx + 1 < self.tracks.len();
                        if ui.add_enabled(can_next, egui::Button::new("⏭")).clicked() {
                            self.load_track(self.track_idx + 1);
                        }

                        ui.label("|");
                        if exporting {
                            if ui.button("Cancel").clicked() {
                                if let Some(ref tok) = self.export_cancel {
                                    tok.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        } else {
                            if ui.add_enabled(can_export, egui::Button::new("Export Track")).clicked() {
                                if let Some(dir) = rfd::FileDialog::new()
                                    .set_title("Choose output folder for MKV files")
                                    .pick_folder()
                                {
                                    let track = self.tracks[self.track_idx].clone();
                                    let (prog, cancel) = export::export_all_async(
                                        vec![track],
                                        self.cdg_path.clone().unwrap(),
                                        cdeg_on,
                                        self.active_channels,
                                        dir,
                                        disc_title.clone(),
                                    );
                                    self.export_progress = Some(prog);
                                    self.export_cancel = Some(cancel);
                                }
                            }
                            if ui.add_enabled(can_export, egui::Button::new("Export Album")).clicked() {
                                if let Some(dir) = rfd::FileDialog::new()
                                    .set_title("Choose output folder for MKV files")
                                    .pick_folder()
                                {
                                    let cp = self
                                        .player
                                        .as_ref()
                                        .map(|p| p.channels_present)
                                        .unwrap_or([false; 16]);
                                    let mut album_channels = [false; 16];
                                    album_channels[0] = true;
                                    if cp[0] && cp[1] {
                                        album_channels[1] = true;
                                    }
                                    let (prog, cancel) = export::export_all_async(
                                        self.tracks.clone(),
                                        self.cdg_path.clone().unwrap(),
                                        cdeg_on,
                                        album_channels,
                                        dir,
                                        disc_title.clone(),
                                    );
                                    self.export_progress = Some(prog);
                                    self.export_cancel = Some(cancel);
                                }
                            }
                        }
                    }
                }
            });

            ui.separator();

            // ── Row 2: seek | time | volume ──────────────────────────────
            ui.horizontal(|ui| {
                let icon_sz = icons::icon_size(22.0);
                let sp = ui.spacing().item_spacing.x;
                let body_font = egui::TextStyle::Body.resolve(ui.style());
                let avail = ui.available_width();
                let volume_slider_w = 72.0;

                if !in_library && !self.tracks.is_empty() {
                    let p = self.player.as_ref().unwrap();
                    let elapsed = p.elapsed();
                    let total = p.total_duration();
                    let fmt = |d: Duration| {
                        let s = d.as_secs();
                        format!("{:02}:{:02}", s / 60, s % 60)
                    };
                    let time_str = format!("{} / {}", fmt(elapsed), fmt(total));
                    let time_w = ui.fonts(|f| {
                        f.layout_no_wrap(
                            time_str.clone(),
                            body_font.clone(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                    });
                    let sep_w = ui.fonts(|f| {
                        f.layout_no_wrap(
                            "|".to_string(),
                            body_font.clone(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                    });
                    let volume_cluster_pad = 32.0;
                    let controls_w = time_w
                        + sp
                        + sep_w
                        + sp
                        + icon_sz.x
                        + sp
                        + volume_slider_w
                        + sp
                        + icon_sz.x
                        + volume_cluster_pad;
                    let edge_gutter = 12.0;
                    let seek_w = (avail - sp - controls_w - edge_gutter).max(160.0);
                    let total_w = seek_w + sp + controls_w;
                    ui.add_space(((avail - total_w) / 2.0).max(0.0));
                    let total_secs = total.as_secs_f64().max(1.0);
                    let progress = (elapsed.as_secs_f64() / total_secs).clamp(0.0, 1.0) as f32;
                    let bar_height = 18.0;
                    let (seek_rect, seek_resp) = ui.allocate_exact_size(
                        egui::vec2(seek_w, bar_height),
                        egui::Sense::click_and_drag(),
                    );
                    let track_rect = egui::Rect::from_center_size(
                        seek_rect.center(),
                        egui::vec2(seek_rect.width(), 6.0),
                    );
                    let visuals = ui.visuals();
                    let bg_fill = visuals.widgets.inactive.bg_fill;
                    let fg_fill = visuals.selection.bg_fill;
                    let thumb_x = track_rect.left() + track_rect.width() * progress;
                    let played_rect = egui::Rect::from_min_max(
                        track_rect.min,
                        egui::pos2(thumb_x, track_rect.bottom()),
                    );

                    ui.painter().rect_filled(track_rect, 3.0, bg_fill);
                    ui.painter().rect_filled(played_rect, 3.0, fg_fill);
                    ui.painter()
                        .circle_filled(egui::pos2(thumb_x, track_rect.center().y), 6.0, fg_fill);

                    if (seek_resp.clicked() || seek_resp.dragged())
                        && seek_resp.interact_pointer_pos().is_some()
                    {
                        let pointer = seek_resp.interact_pointer_pos().unwrap();
                        let frac =
                            ((pointer.x - track_rect.left()) / track_rect.width()).clamp(0.0, 1.0);
                        let target = (frac as f64 * total_secs * PACKETS_PER_SECOND as f64) as usize;
                        self.player.as_mut().unwrap().seek_to(target);
                    }

                    ui.label(time_str);
                    ui.label("|");
                } else {
                    let vol_w = icon_sz.x + sp + volume_slider_w + sp + icon_sz.x;
                    let low_icon_shift =
                        icon_sz.x * (1.0 - 39.389 / 75.0) * 0.72;
                    ui.add_space((((avail - vol_w) / 2.0) - (low_icon_shift * 0.5)).max(0.0));
                }

                let (icon_rect, _) = ui.allocate_exact_size(icon_sz, egui::Sense::hover());
                icons::sound_lo(ui.painter(), icon_rect);
                if ui
                    .add_sized(
                        [volume_slider_w, 18.0],
                        egui::Slider::new(&mut self.volume, 0.0f32..=1.0).show_value(false),
                    )
                    .changed()
                {
                    if let Some(ref p) = self.player {
                        p.sink.set_volume(self.volume);
                    }
                }
                let (icon_rect, _) = ui.allocate_exact_size(icon_sz, egui::Sense::hover());
                icons::sound_hi(ui.painter(), icon_rect);
            });

            ui.separator();

            let status_text = if in_library {
                None
            } else {
                self.export_progress.as_ref().and_then(|prog| {
                    match &*prog.lock().ok()? {
                        ExportState::Running { track_idx, total, frame_frac } => Some(format!(
                            "Track {}/{} - {:.0}%",
                            track_idx + 1,
                            total,
                            frame_frac * 100.0
                        )),
                        ExportState::Done => Some("Export done.".to_string()),
                        ExportState::Error(e) => Some(e.clone()),
                        ExportState::Idle => None,
                    }
                })
            };
            if let Some(status_text) = status_text {
                ui.separator();
                ui.horizontal(|ui| {
                    let avail = ui.available_width();
                    let body_font = egui::TextStyle::Body.resolve(ui.style());
                    let text_w = ui.fonts(|f| {
                        f.layout_no_wrap(
                            status_text.clone(),
                            body_font.clone(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                    });
                    ui.add_space(((avail - text_w) / 2.0).max(0.0));
                    match self
                        .export_progress
                        .as_ref()
                        .and_then(|prog| prog.lock().ok())
                        .as_deref()
                    {
                        Some(ExportState::Error(_)) => {
                            ui.colored_label(egui::Color32::RED, status_text);
                        }
                        _ => {
                            ui.label(status_text);
                        }
                    }
                });
            }

            // ── Row 4: channel selector (player) or library context hint ──
            ui.horizontal(|ui| {
                if in_library {
                    let msg = if self.library.is_empty() {
                        "Click \"Set Library\" to navigate to your CD+G disc images, or click \"Open\" to load a file."
                    } else {
                        "Select a CD+G title or click \"Open\" to load a file."
                    };
                    let avail = ui.available_width();
                    let body_font = egui::TextStyle::Body.resolve(ui.style());
                    let text_w = ui
                        .fonts(|f| {
                            f.layout_no_wrap(
                                msg.to_string(),
                                body_font,
                                egui::Color32::TRANSPARENT,
                            )
                            .size()
                            .x
                        })
                        .min(avail);
                    ui.add_space(((avail - text_w) / 2.0).max(0.0));
                    ui.label(egui::RichText::new(msg).color(egui::Color32::GRAY));
                } else {
                    // CD+EG toggle | Channels (centered)
                    let is_cdeg_disc =
                        self.player.as_ref().map(|p| p.is_cdeg).unwrap_or(false);
                    let label_text = "Channels:";
                    let spacing = ui.spacing().item_spacing.x;
                    let btn_padding = ui.spacing().button_padding;
                    let body_font = egui::TextStyle::Body.resolve(ui.style());

                    let label_width = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label_text.to_string(),
                            body_font.clone(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                    });
                    let btn_w = 26.0;
                    let btn_gap = 2.0;
                    let buttons_total = 16.0 * btn_w + 15.0 * btn_gap;

                    let cdeg_btn_min_w = ui.fonts(|f| {
                        f.layout_no_wrap(
                            "CD+EG".to_string(),
                            body_font.clone(),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x
                    }) + btn_padding.x * 2.0;
                    let cdeg_extra = if is_cdeg_disc {
                        let sep_w = ui.fonts(|f| {
                            f.layout_no_wrap(
                                "|".to_string(),
                                body_font.clone(),
                                egui::Color32::WHITE,
                            )
                            .size()
                            .x
                        });
                        cdeg_btn_min_w + spacing + sep_w + spacing
                    } else {
                        0.0
                    };

                    let content_w = cdeg_extra + label_width + spacing + buttons_total;
                    ui.add_space(((ui.available_width() - content_w) / 2.0).max(0.0));

                    if is_cdeg_disc {
                        if let Some(ref mut player) = self.player {
                            let enabled = player.screen.cdeg_enabled;
                            let (lbl, fg, bg) = if enabled {
                                (
                                    "CD+EG",
                                    egui::Color32::BLACK,
                                    egui::Color32::from_rgb(80, 180, 80),
                                )
                            } else {
                                (
                                    "CD+G",
                                    egui::Color32::from_gray(180),
                                    egui::Color32::from_gray(50),
                                )
                            };
                            let btn = egui::Button::new(
                                egui::RichText::new(lbl).size(11.0).color(fg).strong(),
                            )
                            .fill(bg)
                            .corner_radius(4.0)
                            .min_size(egui::vec2(cdeg_btn_min_w, 0.0));
                            let resp = ui.add(btn).on_hover_text(if enabled {
                                "Showing CD+EG graphics — click to switch to CD+G"
                            } else {
                                "Showing CD+G graphics — click to switch to CD+EG"
                            });
                            if resp.clicked() {
                                let new_enabled = !enabled;
                                self.cdeg_enabled = new_enabled;
                                let channels = player.screen.active_channels;
                                let pos = player.packet_idx;
                                player.screen = CdegScreen::new(new_enabled);
                                player.screen.active_channels = channels;
                                for i in 0..pos {
                                    if let (_, Some(ref pkt)) = player.packets[i] {
                                        player.screen.apply(pkt);
                                    }
                                }
                            }
                        }
                        ui.label("|");
                    }

                    ui.label(label_text);
                    ui.spacing_mut().item_spacing.x = btn_gap;
                    for ch in 0..16usize {
                        let (present, active) = self
                            .player
                            .as_ref()
                            .map(|p| (p.channels_present[ch], p.screen.active_channels[ch]))
                            .unwrap_or((false, false));
                        let (text_color, fill) = if present && active {
                            (egui::Color32::BLACK, egui::Color32::from_rgb(80, 180, 80))
                        } else {
                            (egui::Color32::WHITE, egui::Color32::from_gray(80))
                        };
                        let btn = egui::Button::new(
                            egui::RichText::new(format!("{ch}"))
                                .size(11.0)
                                .color(text_color)
                                .strong(),
                        )
                        .fill(fill)
                        .corner_radius(4.0)
                        .min_size(egui::vec2(26.0, 0.0));
                        if ui.add_enabled(present, btn).clicked() {
                            let new_active = !active;
                            self.active_channels[ch] = new_active;
                            if let Some(ref mut player) = self.player {
                                player.screen.active_channels[ch] = new_active;
                                let channels = player.screen.active_channels;
                                let cdeg = player.screen.cdeg_enabled;
                                let pos = player.packet_idx;
                                player.screen = CdegScreen::new(cdeg);
                                player.screen.active_channels = channels;
                                for i in 0..pos {
                                    if let (_, Some(ref pkt)) = player.packets[i] {
                                        player.screen.apply(pkt);
                                    }
                                }
                            }
                        }
                    }
                }
            });

            ui.add_space(3.0);
        });

        if return_to_library {
            self.return_to_library();
        }

        // ── Central panel ─────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if let Some(ref e) = self.open_error.clone() {
                    // ── Archive / open error ───────────────────────────────
                    let avail = ui.available_size();
                    ui.allocate_ui_with_layout(avail, egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.add_space(avail.y * 0.4);
                        ui.label(
                            egui::RichText::new(e)
                                .size(15.0)
                                .color(egui::Color32::from_rgb(220, 80, 80)),
                        );
                    });
                } else if self.player.is_some() && self.cdg_path.is_none() {
                    // ── Audio-only (no .cdg) ───────────────────────────────
                    let avail = ui.available_size();
                    ui.allocate_ui_with_layout(avail, egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.add_space(avail.y * 0.4);
                        ui.label(
                            egui::RichText::new("Playing audio only — no graphics data on this disc.")
                                .size(15.0)
                                .color(egui::Color32::GRAY),
                        );
                    });
                } else if self.player.is_some() {
                    // ── Video ──────────────────────────────────────────────
                    let screen = self.player.as_ref().map(|p| &p.screen).unwrap();
                    let mut fb = vec![0u32; WIDTH * HEIGHT];
                    screen.render(&mut fb);
                    let pixels: Vec<u8> = fb.iter().flat_map(|&p| {
                        [((p>>16)&0xFF) as u8, ((p>>8)&0xFF) as u8, (p&0xFF) as u8, 255u8]
                    }).collect();
                    let image = ColorImage::from_rgba_unmultiplied([WIDTH, HEIGHT], &pixels);
                    match &mut self.texture {
                        Some(t) => t.set(image, TextureOptions::NEAREST),
                        None    => self.texture = Some(
                            ctx.load_texture("cdg", image, TextureOptions::NEAREST)
                        ),
                    }
                    if let Some(tex) = &self.texture {
                        let avail  = ui.available_size();
                        let aspect = WIDTH as f32 / HEIGHT as f32;
                        let (w, h) = if avail.x / avail.y > aspect {
                            (avail.y * aspect, avail.y)
                        } else {
                            (avail.x, avail.x / aspect)
                        };
                        ui.add_space((avail.y - h) / 2.0);
                        ui.horizontal(|ui| {
                            ui.add_space((avail.x - w) / 2.0);
                            ui.image((tex.id(), egui::vec2(w, h)));
                        });
                    }
                } else {
                    // ── Library browser or setup prompt ────────────────────
                    let avail = ui.available_size();
                    ui.allocate_ui_with_layout(
                        avail,
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            ui.add_space(20.0);
                            if self.config.library_path.is_none() {
                                ui.label(
                                    egui::RichText::new("No library set")
                                        .size(22.0)
                                        .color(egui::Color32::WHITE),
                                );
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new(
                                        "Click 'Settings' > 'Set Library Location...'\n\
                                         to point the player to your library of files."
                                    )
                                    .size(14.0)
                                    .color(egui::Color32::GRAY),
                                );
                            } else if self.library.is_empty() {
                                ui.label(
                                    egui::RichText::new("Library is empty")
                                        .size(20.0)
                                        .color(egui::Color32::WHITE),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(
                                        "Click the \"Set Library\" button to select your disc image folder."
                                    )
                                    .size(14.0)
                                    .color(egui::Color32::GRAY),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("Library")
                                        .size(20.0)
                                        .color(egui::Color32::WHITE)
                                        .strong(),
                                );
                                ui.add_space(10.0);
                                egui::ScrollArea::vertical().show(ui, |ui| {
                                    let entries: Vec<(String, DiscSource)> = self.library
                                        .iter()
                                        .map(|d| {
                                            let src = match &d.source {
                                                DiscSource::Cue(p)    => DiscSource::Cue(p.clone()),
                                                DiscSource::Zip(p)    => DiscSource::Zip(p.clone()),
                                                DiscSource::SevenZ(p) => DiscSource::SevenZ(p.clone()),
                                            };
                                            (d.title.clone(), src)
                                        })
                                        .collect();

                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    let pad = egui::vec2(12.0, 8.0);
                                    let font_id = egui::FontId::proportional(15.0);

                                    for (title, source) in entries {
                                        let avail_w = ui.available_width();

                                        // Measure wrapped text so the row height is exact.
                                        let galley = ui.fonts(|f| f.layout(
                                            title.clone(),
                                            font_id.clone(),
                                            egui::Color32::WHITE,
                                            avail_w - pad.x * 2.0,
                                        ));
                                        let row_h = galley.size().y + pad.y * 2.0;

                                        let (rect, resp) = ui.allocate_exact_size(
                                            egui::vec2(avail_w, row_h),
                                            egui::Sense::click(),
                                        );

                                        if ui.is_rect_visible(rect) {
                                            if resp.hovered() {
                                                ui.painter().rect_filled(
                                                    rect,
                                                    6.0,
                                                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 25),
                                                );
                                            }
                                            ui.painter().galley(
                                                rect.min + pad,
                                                galley,
                                                egui::Color32::WHITE,
                                            );
                                        }

                                        if resp.clicked() {
                                            match source {
                                                DiscSource::Cue(p)    => self.load_cue(p),
                                                DiscSource::Zip(p)    => self.open_zip(p),
                                                DiscSource::SevenZ(p) => self.open_7z(p),
                                            }
                                        }
                                    }
                                });
                            }
                        },
                    );
                }
            });

        // Enforce aspect ratio: height = width × (H/W) + toolbar
        let toolbar_h = toolbar.response.rect.height();
        let win = ctx.screen_rect();
        let correct_h = win.width() * HEIGHT as f32 / WIDTH as f32 + toolbar_h;
        if (correct_h - win.height()).abs() > 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                win.width(),
                correct_h,
            )));
        }

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

// ── Archive extraction ────────────────────────────────────────────────────────

/// Strip non-ASCII characters from a filename stem, keeping the extension.
/// Used to ensure .cue FILE references and actual filenames always match,
/// regardless of NFC/NFD or encoding differences in the archive.
fn sanitize_filename(name: &str) -> String {
    let p = Path::new(name);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let stem: String = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .chars()
        .filter(|c| c.is_ascii())
        .collect();
    if ext.is_empty() {
        stem
    } else {
        format!("{stem}.{ext}")
    }
}

/// Rewrite FILE references in a .cue sheet to use sanitized filenames.
fn sanitize_cue(content: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(content);
    let result = text
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.to_ascii_uppercase().starts_with("FILE ") {
                if let (Some(s), Some(e)) = (line.find('"'), line.rfind('"')) {
                    if s < e {
                        let original = &line[s + 1..e];
                        let sanitized = sanitize_filename(original);
                        return format!("{}{}{}", &line[..s + 1], sanitized, &line[e..]);
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    result.into_bytes()
}

/// Extract disc-relevant files (.cue, .bin, .cdg) from a ZIP archive into a
/// temporary directory.  Works for both regular ZIPs and TorrentZip (STORED).
/// Returns `(temp_dir, cue_path)` on success.
fn extract_disc_zip(zip_path: &Path) -> Result<(PathBuf, PathBuf), String> {
    use std::io::Read;

    let file = std::fs::File::open(zip_path).map_err(|e| format!("Cannot open ZIP: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Invalid ZIP file: {e}"))?;

    // Unique temp dir per process so concurrent launches don't collide.
    let temp_dir = std::env::temp_dir().join(format!("cdg-player-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("Cannot create temp dir: {e}"))?;

    let mut cue_path: Option<PathBuf> = None;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("ZIP entry error: {e}"))?;

        // Skip directories and files we don't care about.
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".cue") && !lower.ends_with(".bin") && !lower.ends_with(".cdg") {
            continue;
        }

        // Reject compressed entries.
        if entry.compression() != zip::CompressionMethod::Stored {
            return Err("CD+G Player supports uncompressed archives only.".to_string());
        }

        let file_name = Path::new(&name)
            .file_name()
            .ok_or_else(|| format!("Bad entry name: {name}"))?
            .to_string_lossy();

        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("Failed to read {name} from ZIP: {e}"))?;

        // Sanitize the on-disk filename and rewrite .cue FILE refs to match.
        let sanitized = sanitize_filename(&file_name);
        let out_path = temp_dir.join(&sanitized);
        let data = if lower.ends_with(".cue") {
            sanitize_cue(&buf)
        } else {
            buf
        };
        std::fs::write(&out_path, &data).map_err(|e| format!("Failed to write {name}: {e}"))?;

        if lower.ends_with(".cue") {
            cue_path = Some(out_path);
        }
    }

    let cue =
        cue_path.ok_or_else(|| "CD+G Player requires a disc image to have a .cue.".to_string())?;
    Ok((temp_dir, cue))
}

/// Extract disc files from a 7z archive (STORE or any supported method).
/// Works for both regular 7z and Torrent7z.
/// Returns `(temp_dir, cue_path)` on success.
fn extract_disc_7z(path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let temp_dir = std::env::temp_dir().join(format!("cdg-player-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("Cannot create temp dir: {e}"))?;

    let mut reader = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
        .map_err(|e| format!("Cannot open 7z: {e}"))?;

    // Check that relevant files are stored uncompressed before extracting.
    for entry in reader.archive().files.iter() {
        let name = entry.name().to_ascii_lowercase();
        if entry.is_directory() {
            continue;
        }
        if !name.ends_with(".cue") && !name.ends_with(".bin") && !name.ends_with(".cdg") {
            continue;
        }
        if !entry.has_stream() {
            continue;
        }
        let mut methods = Vec::new();
        if reader
            .file_compression_methods(entry.name(), &mut methods)
            .is_ok()
        {
            let is_store = methods.is_empty()
                || methods
                    .iter()
                    .all(|m| *m == sevenz_rust2::EncoderMethod::COPY);
            if !is_store {
                return Err("CD+G Player supports uncompressed archives only.".to_string());
            }
        }
    }

    let mut cue_path: Option<PathBuf> = None;

    reader
        .for_each_entries(|entry, source| {
            let name = entry.name().to_ascii_lowercase();
            if entry.is_directory()
                || (!name.ends_with(".cue") && !name.ends_with(".bin") && !name.ends_with(".cdg"))
            {
                std::io::copy(source, &mut std::io::sink())?; // must consume reader
                return Ok(true);
            }
            // Flatten any folder prefix and sanitize the filename.
            let file_name = Path::new(entry.name())
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(entry.name()))
                .to_string_lossy()
                .into_owned();
            let sanitized = sanitize_filename(&file_name);
            let out_path = temp_dir.join(&sanitized);
            if name.ends_with(".cue") {
                let mut buf = Vec::new();
                std::io::copy(source, &mut buf)?;
                std::fs::write(&out_path, sanitize_cue(&buf))?;
            } else {
                let mut out = std::fs::File::create(&out_path)?;
                std::io::copy(source, &mut out)?;
            }
            Ok(true)
        })
        .map_err(|e| format!("7z extraction failed: {e}"))?;

    // Find the .cue that was extracted.
    for entry in std::fs::read_dir(&temp_dir)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("cue") {
            cue_path = Some(p);
            break;
        }
    }

    let cue =
        cue_path.ok_or_else(|| "CD+G Player requires a disc image to have a .cue.".to_string())?;
    Ok((temp_dir, cue))
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn app_icon() -> egui::IconData {
    let bytes = include_bytes!("../logos/CDG Logo.png");
    let img = image::load_from_memory(bytes)
        .expect("CDG Logo.png is a valid PNG")
        .into_rgba8();
    let (width, height) = img.dimensions();
    let mut rgba = img.into_raw();

    // Apply a squircle mask matching the macOS Dock icon shape.
    // Corner radius is ~22.5% of the icon width (Apple HIG standard).
    let r = width as f32 * 0.225;
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    for y in 0..height {
        for x in 0..width {
            let ax = (x as f32 - cx + 0.5).abs();
            let ay = (y as f32 - cy + 0.5).abs();
            let inside = if ax <= cx - r || ay <= cy - r {
                true
            } else {
                let dx = ax - (cx - r);
                let dy = ay - (cy - r);
                dx * dx + dy * dy <= r * r
            };
            if !inside {
                rgba[((y * width + x) * 4 + 3) as usize] = 0;
            }
        }
    }

    egui::IconData {
        rgba,
        width,
        height,
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Each entry: (key, path). Missing paths are silently skipped.
    // Fonts are appended as fallbacks in order, so egui tries each in turn
    // before rendering a missing-glyph box.
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &str)] = &[
        ("latin_ext", "/System/Library/Fonts/Helvetica.ttc"),
        ("cjk", "/System/Library/Fonts/PingFang.ttc"),
        ("arabic", "/System/Library/Fonts/GeezaPro.ttc"),
        ("thai", "/System/Library/Fonts/Thonburi.ttf"),
        ("devanagari", "/System/Library/Fonts/Kohinoor.ttc"),
        (
            "hebrew",
            "/System/Library/Fonts/Supplemental/Arial Hebrew.ttf",
        ),
        (
            "armenian",
            "/System/Library/Fonts/Supplemental/Mshtakan.ttf",
        ),
        ("georgian", "/System/Library/Fonts/Supplemental/BPG.ttf"),
        ("tibetan", "/System/Library/Fonts/Supplemental/Kailasa.ttf"),
        (
            "myanmar",
            "/System/Library/Fonts/Supplemental/Myanmar MN.ttc",
        ),
        ("khmer", "/System/Library/Fonts/Supplemental/Khmer MN.ttc"),
        ("lao", "/System/Library/Fonts/Supplemental/Lao MN.ttf"),
        (
            "sinhala",
            "/System/Library/Fonts/Supplemental/Sinhala MN.ttc",
        ),
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[(&str, &str)] = &[
        ("latin_ext", "C:\\Windows\\Fonts\\segoeui.ttf"),
        ("cjk", "C:\\Windows\\Fonts\\msgothic.ttc"),
        ("arabic", "C:\\Windows\\Fonts\\arial.ttf"), // covers Arabic + Hebrew
        ("thai", "C:\\Windows\\Fonts\\tahoma.ttf"),
        ("devanagari", "C:\\Windows\\Fonts\\mangal.ttf"),
        ("tamil", "C:\\Windows\\Fonts\\latha.ttf"),
        ("telugu", "C:\\Windows\\Fonts\\gautami.ttf"),
        ("kannada", "C:\\Windows\\Fonts\\tunga.ttf"),
        ("malayalam", "C:\\Windows\\Fonts\\kartika.ttf"),
        ("sinhala", "C:\\Windows\\Fonts\\iskpota.ttf"),
        ("myanmar", "C:\\Windows\\Fonts\\mmrtext.ttf"),
        ("ethiopic", "C:\\Windows\\Fonts\\nyala.ttf"),
        ("georgian", "C:\\Windows\\Fonts\\sylfaen.ttf"),
        ("armenian", "C:\\Windows\\Fonts\\sylfaen.ttf"),
        ("khmer", "C:\\Windows\\Fonts\\leelawad.ttf"),
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[(&str, &str)] = &[
        (
            "latin_ext",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ),
        (
            "latin_ext2",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ),
        ("cjk", "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"),
        (
            "cjk2",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ),
        (
            "arabic",
            "/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf",
        ),
        (
            "arabic2",
            "/usr/share/fonts/truetype/arabic/NotoNaskhArabic-Regular.ttf",
        ),
        (
            "hebrew",
            "/usr/share/fonts/truetype/noto/NotoSansHebrew-Regular.ttf",
        ),
        (
            "devanagari",
            "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
        ),
        (
            "thai",
            "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
        ),
        (
            "tamil",
            "/usr/share/fonts/truetype/noto/NotoSansTamil-Regular.ttf",
        ),
        (
            "telugu",
            "/usr/share/fonts/truetype/noto/NotoSansTelugu-Regular.ttf",
        ),
        (
            "kannada",
            "/usr/share/fonts/truetype/noto/NotoSansKannada-Regular.ttf",
        ),
        (
            "malayalam",
            "/usr/share/fonts/truetype/noto/NotoSansMalayalam-Regular.ttf",
        ),
        (
            "bengali",
            "/usr/share/fonts/truetype/noto/NotoSansBengali-Regular.ttf",
        ),
        (
            "sinhala",
            "/usr/share/fonts/truetype/noto/NotoSansSinhala-Regular.ttf",
        ),
        (
            "myanmar",
            "/usr/share/fonts/truetype/noto/NotoSansMyanmar-Regular.ttf",
        ),
        (
            "khmer",
            "/usr/share/fonts/truetype/noto/NotoSansKhmer-Regular.ttf",
        ),
        (
            "lao",
            "/usr/share/fonts/truetype/noto/NotoSansLao-Regular.ttf",
        ),
        (
            "tibetan",
            "/usr/share/fonts/truetype/noto/NotoSansTibetan-Regular.ttf",
        ),
        (
            "ethiopic",
            "/usr/share/fonts/truetype/noto/NotoSansEthiopic-Regular.ttf",
        ),
        (
            "georgian",
            "/usr/share/fonts/truetype/noto/NotoSansGeorgian-Regular.ttf",
        ),
        (
            "armenian",
            "/usr/share/fonts/truetype/noto/NotoSansArmenian-Regular.ttf",
        ),
    ];

    for (key, path) in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts
                .font_data
                .insert((*key).to_owned(), egui::FontData::from_owned(data).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push((*key).to_owned());
            }
        }
    }

    ctx.set_fonts(fonts);
}

fn main() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CD+G Player")
            .with_inner_size([WIDTH as f32 * 2.0, HEIGHT as f32 * 2.0 + 48.0])
            .with_min_inner_size([WIDTH as f32, HEIGHT as f32 + 48.0])
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "CD+G Player",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .expect("eframe error");
}
