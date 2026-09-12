use std::path::PathBuf;

use anyhow::{Result, bail};

/// Upper bound on either output dimension.
///
/// Bounds the per-worker frame buffer and the per-frame PNG, and covers both
/// explicitly requested sizes and the side scaled from the aspect ratio.
pub const MAX_DIMENSION: usize = 4096;

/// Upper bound on the number of rendered frames.
///
/// Every frame is written to a temporary directory before FFmpeg reads it, so
/// this is what stops an untrusted Lottie (`.tgs` files are downloaded from
/// Telegram) from filling the disk. The animation duration is self-reported by
/// the file via `(op - ip) / fr` and is not otherwise constrained.
///
/// Roughly 5.5 minutes at 60 fps, or 1.4 minutes at the 240 fps ceiling.
pub const MAX_FRAMES: usize = 20_000;

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
}

#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub fps: u32,
    pub width: Option<usize>,
    pub height: Option<usize>,
    pub quality: u8,
    pub play_speed: f64,
    pub rotation_degrees: f64,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    pub threads: usize,
    pub ffmpeg: PathBuf,
    pub output_format: OutputFormat,
}

impl ConvertOptions {
    pub fn validate(&self) -> Result<()> {
        if self.fps == 0 || self.fps > 240 {
            bail!("--fps must be in the range 1..=240");
        }
        if self.output_format == OutputFormat::Gif && !gif_supports_fps(self.fps) {
            bail!(
                "GIF --fps must be one of 1, 2, 4, 5, 10, 20, 25, or 50 so every frame delay is an integer multiple of 10ms"
            );
        }
        if self.quality > 100 {
            bail!("--quality must be in the range 0..=100");
        }
        if !(0.1..=10.0).contains(&self.play_speed) {
            bail!("--play-speed must be in the range 0.1..=10.0");
        }
        if !self.rotation_degrees.is_finite() {
            bail!("--rotation must be a finite number");
        }
        if self.width == Some(0) || self.height == Some(0) {
            bail!("--width and --height must be positive when set");
        }
        if self.output == self.input {
            bail!("--output must not overwrite the input file");
        }
        if self.threads == 0 {
            bail!("--threads must be at least 1");
        }
        if !self.input.is_file() {
            bail!("input is not a file: {}", self.input.display());
        }
        Ok(())
    }

    /// Mirrors the existing C# WebM quality mapping.
    pub const fn vp9_crf(&self) -> u8 {
        match self.quality {
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
    pub const fn vp9_cpu_used(&self) -> u8 {
        match self.quality {
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
    pub const fn prores_bits_per_mb(&self) -> u16 {
        match self.quality {
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
}

pub const fn gif_supports_fps(fps: u32) -> bool {
    fps <= 50 && fps > 0 && 100 % fps == 0
}

#[cfg(test)]
mod tests {
    use super::{ConvertOptions, OutputFormat, gif_supports_fps};
    use std::path::PathBuf;

    fn options(quality: u8) -> ConvertOptions {
        ConvertOptions {
            input: PathBuf::from("input.tgs"),
            output: PathBuf::from("output.webm"),
            fps: 60,
            width: None,
            height: None,
            quality,
            play_speed: 1.0,
            rotation_degrees: 0.0,
            flip_horizontal: false,
            flip_vertical: false,
            threads: 1,
            ffmpeg: PathBuf::from("ffmpeg"),
            output_format: OutputFormat::WebmVp9,
        }
    }

    #[test]
    fn quality_mapping_matches_the_desktop_converter() {
        assert_eq!(options(100).vp9_crf(), 15);
        assert_eq!(options(94).vp9_crf(), 20);
        assert_eq!(options(89).vp9_crf(), 25);
        assert_eq!(options(29).vp9_crf(), 55);
        assert_eq!(options(100).vp9_cpu_used(), 0);
        assert_eq!(options(30).vp9_cpu_used(), 6);
        assert_eq!(options(0).vp9_cpu_used(), 8);
    }

    #[test]
    fn prores_quality_mapping_keeps_high_quality_default() {
        assert_eq!(options(100).prores_bits_per_mb(), 8_000);
        assert_eq!(options(90).prores_bits_per_mb(), 7_000);
        assert_eq!(options(0).prores_bits_per_mb(), 2_000);
    }

    #[test]
    fn gif_fps_matches_ten_millisecond_delays() {
        assert!(gif_supports_fps(50));
        assert!(gif_supports_fps(25));
        assert!(gif_supports_fps(20));
        assert!(!gif_supports_fps(30));
        assert!(!gif_supports_fps(51));
    }
}
