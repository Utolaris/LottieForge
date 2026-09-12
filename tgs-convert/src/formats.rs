//! Everything that differs per output format.
//!
//! Kept in one module so adding a format means editing one `match` and one
//! test list, instead of hunting through option parsing, the encoder and the
//! CLI. `ffmpeg.rs` only knows how to run a program — what to run is decided
//! here.

use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    WebmVp9,
    MovProres4444,
    Webp,
    Gif,
}

impl OutputFormat {
    pub const fn file_extension(self) -> &'static str {
        match self {
            Self::WebmVp9 => "webm",
            Self::MovProres4444 => "mov",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::WebmVp9 => "VP9 alpha WebM",
            Self::MovProres4444 => "ProRes 4444 alpha MOV",
            Self::Webp => "animated WebP",
            Self::Gif => "animated GIF",
        }
    }

    pub const fn default_fps(self) -> u32 {
        match self {
            Self::Gif => 50,
            Self::WebmVp9 | Self::MovProres4444 | Self::Webp => 60,
        }
    }

    /// Builds the ffmpeg invocation for this format.
    pub fn plan(self, settings: &EncodeSettings) -> EncodePlan {
        match self {
            Self::WebmVp9 => EncodePlan::Single {
                arguments: vp9_arguments(settings),
                progress: settings.progress("VP9 alpha WebM"),
            },
            Self::MovProres4444 => EncodePlan::Single {
                arguments: prores_arguments(settings),
                progress: settings.progress("ProRes 4444 alpha MOV"),
            },
            Self::Webp => EncodePlan::Webp {
                arguments: webp_arguments(settings),
                progress: settings.progress("animated WebP"),
                expected_duration_ms: settings.expected_duration_ms(),
            },
            Self::Gif => EncodePlan::Gif {
                arguments: gif_arguments(settings),
            },
        }
    }
}

/// What the encoder needs, derived from the conversion options.
///
/// Small on purpose: it carries only what turns into ffmpeg arguments, so the
/// encoder never has to see the whole CLI-shaped options struct.
#[derive(Clone, Copy, Debug)]
pub struct EncodeSettings {
    pub fps: u32,
    pub frame_count: usize,
    pub quality: u8,
    pub threads: usize,
}

impl EncodeSettings {
    /// The label shown while encoding, measured against the output duration.
    fn progress(self, label: &'static str) -> Progress {
        Progress {
            label,
            duration_seconds: self.frame_count as f64 / f64::from(self.fps),
        }
    }

    /// The total duration the animated WebP should report, in whole
    /// milliseconds. Truncated the same way the GIF delay grid is.
    fn expected_duration_ms(self) -> u64 {
        (self.frame_count as u64 * 1_000) / u64::from(self.fps)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub label: &'static str,
    pub duration_seconds: f64,
}

/// How the rendered frames become an output file.
#[derive(Clone, Debug)]
pub enum EncodePlan {
    /// One ffmpeg pass whose progress can be read from `-progress pipe:1`.
    Single {
        arguments: Vec<String>,
        progress: Progress,
    },
    /// One ffmpeg pass plus the WebP duration fix-up, which can only run once
    /// the file exists.
    Webp {
        arguments: Vec<String>,
        progress: Progress,
        expected_duration_ms: u64,
    },
    /// GIF needs a palette pass, so FFmpeg's stderr is inherited instead of
    /// parsed for progress.
    Gif { arguments: Vec<String> },
}

/// True when every frame delay at this frame rate is a whole number of 10ms
/// units, which is all the GIF format can express.
pub const fn gif_supports_fps(fps: u32) -> bool {
    fps <= 50 && fps > 0 && 100 % fps == 0
}

fn base_arguments(settings: &EncodeSettings) -> Vec<String> {
    vec![
        "-hide_banner".to_owned(),
        "-y".to_owned(),
        "-framerate".to_owned(),
        settings.fps.to_string(),
        "-i".to_owned(),
        "%05d.png".to_owned(),
        "-frames:v".to_owned(),
        settings.frame_count.to_string(),
    ]
}

fn vp9_arguments(settings: &EncodeSettings) -> Vec<String> {
    let mut arguments = base_arguments(settings);
    arguments.extend([
        "-c:v".to_owned(),
        "libvpx-vp9".to_owned(),
        "-crf".to_owned(),
        vp9_crf(settings.quality).to_string(),
        "-b:v".to_owned(),
        "0".to_owned(),
        "-cpu-used".to_owned(),
        vp9_cpu_used(settings.quality).to_string(),
        "-threads".to_owned(),
        settings.threads.to_string(),
        "-row-mt".to_owned(),
        "1".to_owned(),
        "-tile-columns".to_owned(),
        "2".to_owned(),
        "-tile-rows".to_owned(),
        "1".to_owned(),
        "-frame-parallel".to_owned(),
        "1".to_owned(),
        "-auto-alt-ref".to_owned(),
        "1".to_owned(),
        "-lag-in-frames".to_owned(),
        "25".to_owned(),
        "-pix_fmt".to_owned(),
        "yuva420p".to_owned(),
    ]);
    arguments
}

fn prores_arguments(settings: &EncodeSettings) -> Vec<String> {
    let mut arguments = base_arguments(settings);
    arguments.extend([
        "-c:v".to_owned(),
        "prores_ks".to_owned(),
        "-profile:v".to_owned(),
        "4".to_owned(),
        "-bits_per_mb".to_owned(),
        prores_bits_per_mb(settings.quality).to_string(),
        "-alpha_bits".to_owned(),
        "16".to_owned(),
        "-threads".to_owned(),
        settings.threads.to_string(),
        "-pix_fmt".to_owned(),
        "yuva444p10le".to_owned(),
        "-movflags".to_owned(),
        "+faststart".to_owned(),
    ]);
    arguments
}

fn webp_arguments(settings: &EncodeSettings) -> Vec<String> {
    // Quality 100 uses lossless mode. Below that, compression level 6 makes
    // libwebp_anim pathologically slow, so lossy output drops to level 5.
    let lossless = settings.quality == 100;
    let encoder_quality = if lossless { 75 } else { settings.quality };
    let compression_level = if lossless { 6 } else { 5 };
    let pixel_format = if lossless { "bgra" } else { "yuva420p" };

    let mut arguments = base_arguments(settings);
    arguments.extend([
        "-c:v".to_owned(),
        "libwebp_anim".to_owned(),
        "-lossless".to_owned(),
        u8::from(lossless).to_string(),
        "-compression_level".to_owned(),
        compression_level.to_string(),
        "-quality".to_owned(),
        encoder_quality.to_string(),
        "-pix_fmt".to_owned(),
        pixel_format.to_owned(),
        "-fps_mode".to_owned(),
        "passthrough".to_owned(),
        "-threads".to_owned(),
        settings.threads.to_string(),
        "-loop".to_owned(),
        "0".to_owned(),
        "-f".to_owned(),
        "webp".to_owned(),
    ]);
    arguments
}

fn gif_arguments(settings: &EncodeSettings) -> Vec<String> {
    let mut arguments = base_arguments(settings);
    arguments.extend([
        "-filter_complex".to_owned(),
        format!(
            "[0:v]split[s0][s1];[s0]palettegen=max_colors={}:stats_mode=diff[p];\
             [s1][p]paletteuse=alpha_threshold=128",
            gif_max_colors(settings.quality)
        ),
        "-loop".to_owned(),
        "0".to_owned(),
    ]);
    arguments
}

/// Mirrors the existing C# WebM quality mapping.
fn vp9_crf(quality: u8) -> u8 {
    match quality {
        95..=100 => 15,
        90..=94 => 20,
        80..=89 => 25,
        70..=79 => 30,
        60..=69 => 35,
        50..=59 => 40,
        40..=49 => 45,
        30..=39 => 50,
        _ => 55,
    }
}

/// Mirrors the existing C# WebM quality mapping.
fn vp9_cpu_used(quality: u8) -> u8 {
    match quality {
        90..=100 => 0,
        80..=89 => 1,
        70..=79 => 2,
        60..=69 => 3,
        50..=59 => 4,
        40..=49 => 5,
        30..=39 => 6,
        _ => 8,
    }
}

/// Maps the shared 0..100 quality control to the ProRes encoder's
/// per-macroblock bitrate ceiling. ProRes 4444 profile and alpha depth stay
/// fixed regardless of this setting.
fn prores_bits_per_mb(quality: u8) -> u16 {
    match quality {
        95..=100 => 8_000,
        90..=94 => 7_000,
        80..=89 => 6_000,
        70..=79 => 5_000,
        60..=69 => 4_000,
        50..=59 => 3_500,
        40..=49 => 3_000,
        30..=39 => 2_500,
        _ => 2_000,
    }
}

fn gif_max_colors(quality: u8) -> u16 {
    u16::from(quality) * 224 / 100 + 32
}

/// Corrects the last animated WebP frame delay so the file's total duration
/// matches the frame count and frame rate.
///
/// FFmpeg rounds each frame delay to whole milliseconds, which can leave the
/// file one millisecond short. FFmpeg may also merge adjacent identical
/// frames, so the last frame is where the correction has to land.
pub fn normalize_webp_duration(output: &Path, expected_duration_ms: u64) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(output)
        .with_context(|| {
            format!(
                "failed to open {} for WebP timing verification",
                output.display()
            )
        })?;
    let file_length = file.metadata()?.len();
    let mut riff_header = [0_u8; 12];
    file.read_exact(&mut riff_header)
        .with_context(|| format!("failed to read WebP header from {}", output.display()))?;
    if &riff_header[..4] != b"RIFF" || &riff_header[8..] != b"WEBP" {
        bail!(
            "FFmpeg output is not a RIFF WebP file: {}",
            output.display()
        );
    }

    let riff_end = u64::from(u32::from_le_bytes(
        riff_header[4..8]
            .try_into()
            .expect("RIFF size is four bytes"),
    )) + 8;
    if riff_end > file_length || riff_end < 12 {
        bail!(
            "FFmpeg produced a truncated WebP file: {}",
            output.display()
        );
    }

    let mut offset = 12_u64;
    let mut total_duration_ms = 0_u64;
    let mut last_duration = None;
    while offset < riff_end {
        if riff_end - offset < 8 {
            bail!(
                "FFmpeg produced a malformed WebP chunk table: {}",
                output.display()
            );
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk_header = [0_u8; 8];
        file.read_exact(&mut chunk_header)?;
        let chunk_length = u64::from(u32::from_le_bytes(
            chunk_header[4..]
                .try_into()
                .expect("chunk size is four bytes"),
        ));
        let payload_offset = offset + 8;
        let padded_length = chunk_length
            .checked_add(chunk_length % 2)
            .context("WebP chunk length overflow")?;
        let next_offset = payload_offset
            .checked_add(padded_length)
            .context("WebP chunk offset overflow")?;
        if next_offset > riff_end {
            bail!(
                "FFmpeg produced a truncated WebP chunk: {}",
                output.display()
            );
        }

        if &chunk_header[..4] == b"ANMF" {
            if chunk_length < 16 {
                bail!(
                    "FFmpeg produced a malformed WebP animation frame: {}",
                    output.display()
                );
            }
            let duration_offset = payload_offset + 12;
            file.seek(SeekFrom::Start(duration_offset))?;
            let mut duration_bytes = [0_u8; 3];
            file.read_exact(&mut duration_bytes)?;
            let duration_ms = u32::from(duration_bytes[0])
                | (u32::from(duration_bytes[1]) << 8)
                | (u32::from(duration_bytes[2]) << 16);
            total_duration_ms += u64::from(duration_ms);
            last_duration = Some((duration_offset, duration_ms));
        }
        offset = next_offset;
    }

    let Some((last_duration_offset, last_duration_ms)) = last_duration else {
        bail!(
            "FFmpeg output has no WebP animation frames: {}",
            output.display()
        );
    };
    let correction = i128::from(expected_duration_ms) - i128::from(total_duration_ms);
    if correction == 0 {
        return Ok(());
    }
    if correction.abs() > 1 {
        // The file is already encoded and perfectly usable. A timing drift
        // larger than one millisecond is worth mentioning, but not worth
        // throwing away a finished conversion.
        eprintln!(
            "warning: WebP duration is {total_duration_ms}ms, expected {expected_duration_ms}ms; \
             keeping the file FFmpeg produced"
        );
        return Ok(());
    }

    let corrected_duration = i128::from(last_duration_ms) + correction;
    if !(0..=0xFF_FFFF).contains(&corrected_duration) {
        bail!("corrected WebP frame duration is out of range");
    }
    let corrected_bytes = (corrected_duration as u32).to_le_bytes();
    file.seek(SeekFrom::Start(last_duration_offset))?;
    file.write_all(&corrected_bytes[..3])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        EncodeSettings, OutputFormat, gif_max_colors, gif_supports_fps, normalize_webp_duration,
        prores_bits_per_mb, vp9_cpu_used, vp9_crf,
    };

    fn settings(quality: u8) -> EncodeSettings {
        EncodeSettings {
            fps: 60,
            frame_count: 180,
            quality,
            threads: 8,
        }
    }

    fn has(arguments: &[String], pair: [&str; 2]) -> bool {
        arguments.windows(2).any(|window| window == pair)
    }

    #[test]
    fn every_format_produces_a_plan() {
        // Exhaustive by construction: a new variant that is not handled in
        // `plan` fails to compile rather than reaching an unreachable!().
        for format in [
            OutputFormat::WebmVp9,
            OutputFormat::MovProres4444,
            OutputFormat::Webp,
            OutputFormat::Gif,
        ] {
            let plan = format.plan(&settings(100));
            let arguments = match &plan {
                super::EncodePlan::Single { arguments, .. }
                | super::EncodePlan::Webp { arguments, .. }
                | super::EncodePlan::Gif { arguments } => arguments,
            };
            assert!(has(arguments, ["-hide_banner", "-y"]));
            assert!(has(arguments, ["-i", "%05d.png"]));
            assert!(has(arguments, ["-frames:v", "180"]));
        }
    }

    #[test]
    fn quality_mapping_matches_the_desktop_converter() {
        assert_eq!(vp9_crf(100), 15);
        assert_eq!(vp9_crf(94), 20);
        assert_eq!(vp9_crf(89), 25);
        assert_eq!(vp9_crf(29), 55);
        assert_eq!(vp9_cpu_used(100), 0);
        assert_eq!(vp9_cpu_used(30), 6);
        assert_eq!(vp9_cpu_used(0), 8);
    }

    #[test]
    fn prores_quality_mapping_keeps_high_quality_default() {
        assert_eq!(prores_bits_per_mb(100), 8_000);
        assert_eq!(prores_bits_per_mb(90), 7_000);
        assert_eq!(prores_bits_per_mb(0), 2_000);
    }

    #[test]
    fn gif_fps_matches_ten_millisecond_delays() {
        assert!(gif_supports_fps(50));
        assert!(gif_supports_fps(25));
        assert!(gif_supports_fps(20));
        assert!(!gif_supports_fps(30));
        assert!(!gif_supports_fps(51));
    }

    #[test]
    fn webp_uses_ffmpeg_animation_encoder_in_lossless_mode_at_quality_100() {
        let arguments = OutputFormat::Webp.plan(&settings(100)).arguments_of();
        assert!(has(&arguments, ["-c:v", "libwebp_anim"]));
        assert!(has(&arguments, ["-lossless", "1"]));
        assert!(has(&arguments, ["-compression_level", "6"]));
        assert!(has(&arguments, ["-quality", "75"]));
        assert!(has(&arguments, ["-pix_fmt", "bgra"]));
        assert!(has(&arguments, ["-loop", "0"]));
        assert!(has(&arguments, ["-f", "webp"]));
    }

    #[test]
    fn webp_uses_requested_lossy_quality_and_yuva_pixel_format() {
        let arguments = OutputFormat::Webp.plan(&settings(0)).arguments_of();
        assert!(has(&arguments, ["-lossless", "0"]));
        assert!(has(&arguments, ["-quality", "0"]));
        assert!(has(&arguments, ["-compression_level", "5"]));
        assert!(has(&arguments, ["-pix_fmt", "yuva420p"]));

        let quality_one = OutputFormat::Webp.plan(&settings(1)).arguments_of();
        assert!(has(&quality_one, ["-quality", "1"]));
    }

    #[test]
    fn gif_uses_palettegen_paletteuse_with_transparency_and_infinite_loop() {
        let arguments = OutputFormat::Gif.plan(&settings(100)).arguments_of();
        let filter = arguments
            .windows(2)
            .find(|window| window[0] == "-filter_complex")
            .map(|window| window[1].as_str())
            .expect("filter_complex argument is present");
        assert!(filter.contains("palettegen=max_colors=256"));
        assert!(filter.contains("stats_mode=diff"));
        assert!(filter.contains("paletteuse=alpha_threshold=128"));
        assert!(has(&arguments, ["-loop", "0"]));
    }

    #[test]
    fn gif_max_colors_maps_quality_across_the_gif_palette() {
        assert_eq!(gif_max_colors(0), 32);
        assert_eq!(gif_max_colors(50), 144);
        assert_eq!(gif_max_colors(100), 256);
    }

    #[test]
    fn webp_duration_normalization_corrects_the_last_frame_by_one_millisecond() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        let mut body = b"WEBP".to_vec();
        for duration in [16_u32, 17, 16] {
            body.extend_from_slice(b"ANMF");
            body.extend_from_slice(&16_u32.to_le_bytes());
            let mut payload = [0_u8; 16];
            payload[12..15].copy_from_slice(&duration.to_le_bytes()[..3]);
            body.extend_from_slice(&payload);
        }
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
        webp.extend_from_slice(&body);
        fs::write(temporary.path(), webp).unwrap();

        normalize_webp_duration(temporary.path(), 50).unwrap();

        let corrected = fs::read(temporary.path()).unwrap();
        assert_eq!(&corrected[80..83], &[17, 0, 0]);
    }

    impl super::EncodePlan {
        fn arguments_of(&self) -> Vec<String> {
            match self {
                Self::Single { arguments, .. }
                | Self::Webp { arguments, .. }
                | Self::Gif { arguments } => arguments.clone(),
            }
        }
    }
}
