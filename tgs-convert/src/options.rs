use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::formats::{EncodeSettings, OutputFormat, gif_supports_fps};

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

/// Bounds shared by the CLI parser and the library entry point, so the two
/// cannot drift apart. Defined as endpoints because clap's range parser works
/// in `i64` while the fields are narrower types.
pub const FPS_MIN: u32 = 1;
pub const FPS_MAX: u32 = 240;
pub const QUALITY_MAX: u8 = 100;
pub const PLAY_SPEED_MIN: f64 = 0.1;
pub const PLAY_SPEED_MAX: f64 = 10.0;
pub const MIN_THREADS: usize = 1;

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
        if self.fps < FPS_MIN || self.fps > FPS_MAX {
            bail!("--fps must be in the range {FPS_MIN}..={FPS_MAX}");
        }
        if self.output_format == OutputFormat::Gif && !gif_supports_fps(self.fps) {
            bail!(
                "GIF --fps must be one of 1, 2, 4, 5, 10, 20, 25, or 50 so every frame delay is an integer multiple of 10ms"
            );
        }
        if self.quality > QUALITY_MAX {
            bail!("--quality must be in the range 0..={QUALITY_MAX}");
        }
        if self.play_speed < PLAY_SPEED_MIN || self.play_speed > PLAY_SPEED_MAX {
            bail!("--play-speed must be in the range {PLAY_SPEED_MIN}..={PLAY_SPEED_MAX}");
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
        if self.threads < MIN_THREADS {
            bail!("--threads must be at least {MIN_THREADS}");
        }
        if !self.input.is_file() {
            bail!("input is not a file: {}", self.input.display());
        }
        Ok(())
    }

    /// The subset of these options the encoder actually needs.
    pub fn encode_settings(&self, frame_count: usize) -> EncodeSettings {
        EncodeSettings {
            fps: self.fps,
            frame_count,
            quality: self.quality,
            threads: self.threads,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ConvertOptions, FPS_MAX, PLAY_SPEED_MAX};
    use crate::formats::OutputFormat;

    fn options() -> ConvertOptions {
        ConvertOptions {
            input: PathBuf::from("input.tgs"),
            output: PathBuf::from("output.webm"),
            fps: 60,
            width: None,
            height: None,
            quality: 100,
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
    fn validate_rejects_values_outside_the_shared_bounds() {
        let out_of_range_fps = ConvertOptions {
            fps: FPS_MAX + 1,
            ..options()
        };
        let message = out_of_range_fps
            .validate()
            .expect_err("fps above the maximum must be rejected")
            .to_string();
        assert!(message.contains("--fps"), "{message}");

        let too_fast = ConvertOptions {
            play_speed: PLAY_SPEED_MAX * 2.0,
            ..options()
        };
        let message = too_fast
            .validate()
            .expect_err("a play speed above the maximum must be rejected")
            .to_string();
        assert!(message.contains("--play-speed"), "{message}");
    }

    #[test]
    fn validate_keeps_gif_on_the_ten_millisecond_grid() {
        let off_grid = ConvertOptions {
            fps: 30,
            output_format: OutputFormat::Gif,
            ..options()
        };
        let message = off_grid
            .validate()
            .expect_err("30 fps is not expressible as 10ms delays")
            .to_string();
        assert!(message.contains("10ms"), "{message}");
    }
}
