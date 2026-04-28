/// Export CD+G / CD+EG tracks to MKV files with AV1 video and FLAC audio.
///
/// Requires ffmpeg with libsvtav1 support, either bundled with the app or
/// installed on the system.
/// Video is rendered at 30 fps (every 10 CDG packets at 300 pps).
/// Each track is exported as its own MKV in the chosen output directory,
/// paired with its own audio so picture and audio stay in sync.
use crate::cdg::{AnyPacket, PacketIter};
use crate::cue::Track;
use crate::renderer::{CdegScreen, HEIGHT, WIDTH};
use std::io::Write;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const EXPORT_FPS: u32 = 30;
const PACKETS_PER_FRAME: usize = (300 / EXPORT_FPS) as usize; // 10
const CDG_BYTES_PER_SECTOR: usize = 96; // 4 packets × 24 bytes

/// Progress shared between the export thread and the UI.
pub type Progress = Arc<Mutex<ExportState>>;

/// Cancel token — set to `true` to request the export thread to stop.
pub type CancelToken = Arc<AtomicBool>;

const WINDOWS_RESERVED_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

#[derive(Clone)]
pub enum ExportState {
    Idle,
    /// `track_idx` is 0-based; `total` is total track count.
    Running {
        track_idx: usize,
        total: usize,
        frame_frac: f32,
    },
    Done,
    Error(String),
}

/// Spawn a per-track export on a background thread.
/// Each track in `tracks` is written to `output_dir/Track NN.mkv`.
/// Returns a `(Progress, CancelToken)` pair — set the token to cancel.
pub fn export_all_async(
    tracks: Vec<Track>,
    cdg_path: PathBuf,
    cdeg_enabled: bool,
    active_channels: [bool; 16],
    output_dir: PathBuf,
    disc_title: String,
) -> (Progress, CancelToken) {
    let progress: Progress = Arc::new(Mutex::new(ExportState::Running {
        track_idx: 0,
        total: tracks.len(),
        frame_frac: 0.0,
    }));
    let cancel: CancelToken = Arc::new(AtomicBool::new(false));
    let prog = Arc::clone(&progress);
    let cancel_thread = Arc::clone(&cancel);

    std::thread::spawn(move || {
        let total = tracks.len();
        let cdg_raw = match std::fs::read(&cdg_path) {
            Ok(b) => b,
            Err(e) => {
                *prog.lock().unwrap() = ExportState::Error(format!("Cannot read CDG: {e}"));
                return;
            }
        };

        for (idx, track) in tracks.iter().enumerate() {
            if cancel_thread.load(Ordering::Relaxed) {
                *prog.lock().unwrap() = ExportState::Idle;
                return;
            }

            *prog.lock().unwrap() = ExportState::Running {
                track_idx: idx,
                total,
                frame_frac: 0.0,
            };

            // Slice CDG data for this track only.
            let cdg_start = track.cdg_offset() as usize;
            let cdg_end =
                (cdg_start + track.sectors as usize * CDG_BYTES_PER_SECTOR).min(cdg_raw.len());
            let cdg_data = &cdg_raw[cdg_start.min(cdg_raw.len())..cdg_end];
            let packets: Vec<_> = PacketIter::new(cdg_data).collect();

            // Auto-detect CD+EG for this track.
            let has_cdeg = packets
                .iter()
                .any(|(_, p)| matches!(p, Some(AnyPacket::Item2(_))));
            let cdeg_on = cdeg_enabled && has_cdeg;

            let audio = track.load_audio();
            let suffix = if has_cdeg {
                if cdeg_on { " - CD+EG" } else { " - CD+G" }
            } else {
                ""
            };
            let file_stem = sanitize_output_stem(&format!(
                "{disc_title} - Track {:02}{suffix}",
                track.number
            ));
            let out_path = output_dir.join(format!("{file_stem}.mkv"));

            if let Err(e) = run_export(
                &packets,
                &audio,
                cdeg_on,
                active_channels,
                &out_path,
                &prog,
                idx,
                total,
                &cancel_thread,
            ) {
                *prog.lock().unwrap() =
                    ExportState::Error(format!("Track {:02}: {e}", track.number));
                return;
            }

            // Check again after each track in case cancel fired mid-export.
            if cancel_thread.load(Ordering::Relaxed) {
                *prog.lock().unwrap() = ExportState::Idle;
                return;
            }
        }

        *prog.lock().unwrap() = ExportState::Done;
    });

    (progress, cancel)
}

fn run_export(
    packets: &[(u32, Option<AnyPacket>)],
    audio: &[i16],
    cdeg_on: bool,
    active_channels: [bool; 16],
    out_path: &Path,
    progress: &Progress,
    track_idx: usize,
    total: usize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    // ── Write temp WAV ──────────────────────────────────────────────────────
    let wav_path = out_path.with_extension("_tmp.wav");
    write_wav(&wav_path, audio).map_err(|e| format!("WAV write failed: {e}"))?;

    // ── Spawn ffmpeg ────────────────────────────────────────────────────────
    // Input 0 (pipe:0) : raw RGB24 video at 30 fps
    // Input 1 (wav)    : PCM audio
    // -map 0:v -map 1:a: explicitly include both streams
    // Output: AV1 + FLAC in MKV
    let size_str = format!("{}x{}", WIDTH, HEIGHT);
    let fps_str = EXPORT_FPS.to_string();
    let ffmpeg_candidates = ffmpeg_candidates();
    let ffmpeg = ffmpeg_candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .unwrap_or_else(|| PathBuf::from(ffmpeg_binary_name()));
    let video_encoder = choose_av1_encoder(&ffmpeg);

    let mut command = Command::new(&ffmpeg);
    let video_args = video_encoder.ffmpeg_args();
    command
        .args([
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgb24",
            "-video_size",
            &size_str,
            "-framerate",
            &fps_str,
            "-i",
            "pipe:0",
            "-i",
            wav_path.to_string_lossy().as_ref(),
            "-map",
            "0:v", // explicitly map video stream
            "-map",
            "1:a", // explicitly map audio stream
            "-c:a",
            "flac",
            "-compression_level",
            "8",
        ])
        .args(video_args)
        .arg(out_path.to_string_lossy().as_ref())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .map_err(|e| {
            let searched = ffmpeg_candidates
                .iter()
                .map(|candidate| candidate.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Failed to spawn ffmpeg at {}: {e}\nSearched: {}\nInstall ffmpeg and make sure it is available to the app.",
                ffmpeg.display(),
                searched
            )
        })?;

    // Take *owned* stdin handle so dropping it sends EOF to ffmpeg.
    let mut stdin = child.stdin.take().unwrap();

    // ── Render and pipe frames ──────────────────────────────────────────────
    let total_frames = (packets.len() / PACKETS_PER_FRAME).max(1);
    let mut screen = CdegScreen::new(cdeg_on);
    screen.active_channels = active_channels;
    let mut pkt_idx = 0;
    let mut frame = 0usize;
    let mut rgb_buf = vec![0u8; WIDTH * HEIGHT * 3];

    while pkt_idx < packets.len() {
        let end = (pkt_idx + PACKETS_PER_FRAME).min(packets.len());
        for i in pkt_idx..end {
            if let (_, Some(ref pkt)) = packets[i] {
                screen.apply(pkt);
            }
        }
        pkt_idx = end;

        let mut fb = vec![0u32; WIDTH * HEIGHT];
        screen.render(&mut fb);
        for (i, &p) in fb.iter().enumerate() {
            rgb_buf[i * 3] = ((p >> 16) & 0xFF) as u8;
            rgb_buf[i * 3 + 1] = ((p >> 8) & 0xFF) as u8;
            rgb_buf[i * 3 + 2] = (p & 0xFF) as u8;
        }

        if let Err(e) = stdin.write_all(&rgb_buf) {
            drop(stdin);
            let message = describe_ffmpeg_failure(child, &wav_path, out_path).unwrap_or_else(|| {
                format!("ffmpeg pipe write failed: {e}")
            });
            return Err(message);
        }

        frame += 1;
        *progress.lock().unwrap() = ExportState::Running {
            track_idx,
            total,
            frame_frac: frame as f32 / total_frames as f32,
        };

        if cancel.load(Ordering::Relaxed) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(out_path);
            let _ = std::fs::remove_file(&wav_path);
            return Ok(()); // caller checks cancel flag and sets Idle
        }
    }

    // Drop owned stdin → sends EOF → ffmpeg finishes encoding video then flushes audio.
    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|e| format!("ffmpeg wait failed: {e}"))?;
    let _ = std::fs::remove_file(&wav_path);

    if output.status.success() {
        Ok(())
    } else {
        Err(ffmpeg_error_message(&output.stderr))
    }
}

fn sanitize_output_stem(stem: &str) -> String {
    let mut sanitized = stem
        .chars()
        .map(|c| {
            if c.is_control() || WINDOWS_RESERVED_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();

    sanitized = sanitized
        .trim_end_matches([' ', '.'])
        .trim()
        .to_string();

    if sanitized.is_empty() {
        "Track".to_string()
    } else {
        sanitized
    }
}

enum Av1Encoder {
    SvtAv1,
    AomAv1,
}

impl Av1Encoder {
    fn ffmpeg_args(&self) -> [&'static str; 8] {
        match self {
            Av1Encoder::SvtAv1 => ["-c:v", "libsvtav1", "-crf", "30", "-b:v", "0", "-preset", "6"],
            Av1Encoder::AomAv1 => [
                "-c:v",
                "libaom-av1",
                "-crf",
                "30",
                "-b:v",
                "0",
                "-cpu-used",
                "6",
            ],
        }
    }
}

fn choose_av1_encoder(ffmpeg: &Path) -> Av1Encoder {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let Ok(output) = output else {
        return Av1Encoder::SvtAv1;
    };

    let encoders = String::from_utf8_lossy(&output.stdout);
    if encoders.contains(" libsvtav1 ") {
        Av1Encoder::SvtAv1
    } else if encoders.contains(" libaom-av1 ") {
        Av1Encoder::AomAv1
    } else {
        Av1Encoder::SvtAv1
    }
}

fn ffmpeg_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(path) = std::env::var_os("FFMPEG") {
        let path = PathBuf::from(path);
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }

    for candidate in ffmpeg_local_candidates() {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(ffmpeg_binary_name());
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }

    for candidate in ffmpeg_fallback_locations() {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    candidates
}

fn ffmpeg_binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "ffmpeg.exe"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "ffmpeg"
    }
}

fn ffmpeg_local_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(current_dir) = std::env::current_dir() {
        candidates.push(current_dir.join(ffmpeg_binary_name()));
        candidates.push(current_dir.join("bundle").join(ffmpeg_binary_name()));
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            candidates.push(dir.join(ffmpeg_binary_name()));
            candidates.push(dir.join("bundle").join(ffmpeg_binary_name()));
            candidates.push(dir.join("ffmpeg").join(ffmpeg_binary_name()));
        }
    }

    candidates
}

fn ffmpeg_fallback_locations() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/opt/local/bin/ffmpeg",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();

        if let Some(program_data) = std::env::var_os("ProgramData") {
            candidates.push(
                PathBuf::from(program_data)
                    .join("chocolatey")
                    .join("bin")
                    .join("ffmpeg.exe"),
            );
        }

        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            candidates.push(
                PathBuf::from(&user_profile)
                    .join("scoop")
                    .join("shims")
                    .join("ffmpeg.exe"),
            );
            candidates.push(
                PathBuf::from(user_profile)
                    .join("AppData")
                    .join("Local")
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links")
                    .join("ffmpeg.exe"),
            );
        }

        candidates
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        vec![
            "/usr/local/bin/ffmpeg",
            "/usr/bin/ffmpeg",
            "/snap/bin/ffmpeg",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

fn describe_ffmpeg_failure(
    child: std::process::Child,
    wav_path: &Path,
    out_path: &Path,
) -> Option<String> {
    let output = child.wait_with_output().ok()?;
    let _ = std::fs::remove_file(wav_path);
    let _ = std::fs::remove_file(out_path);
    Some(ffmpeg_error_message(&output.stderr))
}

fn ffmpeg_error_message(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let detail = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("ffmpeg exited with an unknown error.");

    format!("ffmpeg failed: {detail}")
}

fn write_wav(path: &Path, samples: &[i16]) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}
