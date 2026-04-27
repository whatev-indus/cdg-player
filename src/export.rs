/// Export CD+G / CD+EG tracks to MKV files with AV1 video and FLAC audio.
///
/// Requires ffmpeg (with libsvtav1) to be installed and on PATH.
/// Video is rendered at 30 fps (every 10 CDG packets at 300 pps).
/// Each track is exported as its own MKV in the chosen output directory,
/// paired with its own audio so picture and audio stay in sync.
use crate::cdg::{AnyPacket, PacketIter};
use crate::cue::Track;
use crate::renderer::{CdegScreen, HEIGHT, WIDTH};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    let ffmpeg = find_ffmpeg();

    let mut child = Command::new(&ffmpeg)
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
            "-c:v",
            "libsvtav1",
            "-crf",
            "30",
            "-b:v",
            "0",
            "-preset",
            "6",
            "-c:a",
            "flac",
            "-compression_level",
            "8",
            out_path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "Failed to spawn ffmpeg at {}: {e}\nInstall ffmpeg and make sure it is available to the app.",
                ffmpeg.display()
            )
        })?;

    // Take *owned* stdin handle so dropping it sends EOF to ffmpeg.
    let mut stdin = child.stdin.take().unwrap();

    // ── Render and pipe frames ──────────────────────────────────────────────
    let total_frames = (packets.len() / PACKETS_PER_FRAME).max(1);
    let mut screen = CdegScreen::new(cdeg_on);
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

fn find_ffmpeg() -> PathBuf {
    if let Some(path) = std::env::var_os("FFMPEG") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(ffmpeg_binary_name());
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    for candidate in ffmpeg_fallback_locations() {
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return candidate;
        }
    }

    PathBuf::from(ffmpeg_binary_name())
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

fn ffmpeg_fallback_locations() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/opt/local/bin/ffmpeg",
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &[]
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        &[
            "/usr/local/bin/ffmpeg",
            "/usr/bin/ffmpeg",
            "/snap/bin/ffmpeg",
        ]
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
