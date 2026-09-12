use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tgs_convert::{
    ConvertOptions, FPS_MAX, FPS_MIN, OutputFormat, QUALITY_MAX, convert,
    telegram::{
        TelegramDownloadOptions, download_sticker_set, parse_sticker_set_name, resolve_bot_token,
    },
};

#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert",
    version,
    about = "Parallel TGS/Lottie JSON to transparent VP9 WebM converter",
    // Mixing top-level options with a subcommand is rejected instead of
    // silently ignored.
    args_conflicts_with_subcommands = true,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    default_conversion: DefaultConversion,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Convert to a transparent Apple ProRes 4444 MOV.
    Mov(ConversionArgs),
    /// Convert to a transparent animated WebP.
    Webp(ConversionArgs),
    /// Convert to an animated GIF (1, 2, 4, 5, 10, 20, 25 or 50 fps).
    Gif(ConversionArgs),
    /// Download every sticker or custom emoji from a Telegram pack.
    TelegramDownload(TelegramArgs),
}

/// The bare `tgs-convert <INPUT>` form, which converts to WebM.
///
/// The input is optional here purely because clap will not let a required
/// positional argument coexist with an optional subcommand; `into_conversion`
/// checks it before anything else runs.
#[derive(Debug, Args)]
struct DefaultConversion {
    /// Input .tgs, .json, or gzip-compressed Lottie JSON file.
    input: Option<PathBuf>,

    #[command(flatten)]
    options: SharedConversionOptions,
}

impl DefaultConversion {
    fn into_conversion(self) -> Result<ConversionArgs> {
        Ok(ConversionArgs {
            input: self.input.context("an input file is required")?,
            options: self.options,
        })
    }
}

#[derive(Debug, Args)]
struct ConversionArgs {
    /// Input .tgs, .json, or gzip-compressed Lottie JSON file.
    input: PathBuf,

    #[command(flatten)]
    options: SharedConversionOptions,
}

#[derive(Debug, Args)]
struct SharedConversionOptions {
    /// Destination file. Defaults to the input basename in the same directory.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output frame rate. GIF accepts only 1, 2, 4, 5, 10, 20, 25, or 50 FPS.
    #[arg(long, value_parser = clap::value_parser!(u32).range(i64::from(FPS_MIN)..=i64::from(FPS_MAX)))]
    fps: Option<u32>,

    /// Output width. Defaults to the animation's intrinsic width.
    #[arg(long)]
    width: Option<usize>,

    /// Output height. Defaults to the animation's intrinsic height.
    #[arg(long)]
    height: Option<usize>,

    /// Desktop-compatible quality percentage, mapped to each encoder's settings.
    #[arg(long, default_value_t = QUALITY_MAX, value_parser = clap::value_parser!(u8).range(0..=i64::from(QUALITY_MAX)))]
    quality: u8,

    /// Playback multiplier in the range 0.1..=10.0.
    #[arg(long, default_value_t = 1.0)]
    play_speed: f64,

    /// Clockwise rotation in degrees, applied around the frame center.
    #[arg(long, default_value_t = 0.0)]
    rotation: f64,

    /// Mirror the rendered frames horizontally.
    #[arg(long)]
    flip_horizontal: bool,

    /// Mirror the rendered frames vertically.
    #[arg(long)]
    flip_vertical: bool,

    /// Concurrent rendering workers. Defaults to logical CPU availability.
    #[arg(long, default_value_t = default_threads())]
    threads: usize,

    /// FFmpeg executable path or command name (used by every output format).
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

#[derive(Debug, Args)]
struct TelegramArgs {
    /// Telegram t.me/addstickers or t.me/addemoji link, or a sticker-set name.
    link_or_name: String,

    /// Directory for the downloaded sticker files. Defaults to the sticker-set name.
    #[arg(short = 'o', long = "output-dir")]
    output_directory: Option<PathBuf>,

    /// Concurrent metadata and file-download workers.
    #[arg(long, default_value_t = default_threads())]
    threads: usize,

    /// Temporary Telegram bot token for this run only; overrides the OS
    /// credential store (macOS Keychain / Windows PasswordVault).
    #[arg(long)]
    token: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => run_conversion(
            cli.default_conversion.into_conversion()?,
            OutputFormat::WebmVp9,
        ),
        Some(Command::Mov(arguments)) => run_conversion(arguments, OutputFormat::MovProres4444),
        Some(Command::Webp(arguments)) => run_conversion(arguments, OutputFormat::Webp),
        Some(Command::Gif(arguments)) => run_conversion(arguments, OutputFormat::Gif),
        Some(Command::TelegramDownload(arguments)) => run_telegram_download(arguments),
    }
}

fn run_conversion(cli: ConversionArgs, output_format: OutputFormat) -> Result<()> {
    let options = cli.options;
    let fps = options.fps.unwrap_or_else(|| output_format.default_fps());
    let output = options
        .output
        .unwrap_or_else(|| default_output(&cli.input, output_format));
    let report = convert(&ConvertOptions {
        input: cli.input,
        output: output.clone(),
        fps,
        width: options.width,
        height: options.height,
        quality: options.quality,
        play_speed: options.play_speed,
        rotation_degrees: options.rotation,
        flip_horizontal: options.flip_horizontal,
        flip_vertical: options.flip_vertical,
        threads: options.threads,
        ffmpeg: options.ffmpeg,
        output_format,
    })?;

    println!(
        "Wrote {} ({}x{}, {} frames, {:.3}s)",
        output.display(),
        report.width,
        report.height,
        report.frames,
        report.duration_seconds
    );
    Ok(())
}

fn run_telegram_download(cli: TelegramArgs) -> Result<()> {
    let set_name = parse_sticker_set_name(&cli.link_or_name)?;
    let token = resolve_bot_token(cli.token.as_deref())?;
    let output_directory = cli
        .output_directory
        .unwrap_or_else(|| PathBuf::from(&set_name));
    let report = download_sticker_set(&TelegramDownloadOptions {
        link_or_name: cli.link_or_name,
        output_directory,
        threads: cli.threads,
        token,
    })?;
    println!(
        "Downloaded {} sticker(s) from {} ({}) to {}",
        report.files,
        report.set_name,
        report.title,
        report.output_directory.display()
    );
    Ok(())
}

fn default_output(input: &std::path::Path, output_format: OutputFormat) -> PathBuf {
    input.with_extension(output_format.file_extension())
}

fn default_threads() -> usize {
    match std::thread::available_parallelism() {
        Ok(parallelism) => parallelism.get(),
        Err(_) => 1,
    }
}
