use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Args, Parser};
use tgs_convert::{
    ConvertOptions, OutputFormat, convert,
    telegram::{
        TelegramDownloadOptions, download_sticker_set, parse_sticker_set_name, resolve_bot_token,
    },
};

#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert",
    version,
    about = "Parallel TGS/Lottie JSON to transparent VP9 WebM converter"
)]
struct Cli {
    #[command(flatten)]
    conversion: ConversionArgs,
}

#[derive(Debug, Args)]
struct ConversionArgs {
    /// Input .tgs, .json, or gzip-compressed Lottie JSON file.
    input: PathBuf,

    /// Destination file. Defaults to the input basename in the same directory.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output frame rate. GIF accepts only 1, 2, 4, 5, 10, 20, 25, or 50 FPS.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: Option<u32>,

    /// Output width. Defaults to the animation's intrinsic width.
    #[arg(long)]
    width: Option<usize>,

    /// Output height. Defaults to the animation's intrinsic height.
    #[arg(long)]
    height: Option<usize>,

    /// Desktop-compatible quality percentage, mapped to VP9 CRF and cpu-used.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
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

    /// FFmpeg executable path or command name (used by WebM, MOV, and WebP output).
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert mov",
    version,
    about = "Parallel TGS/Lottie JSON to transparent Apple ProRes 4444 MOV converter"
)]
struct MovCli {
    #[command(flatten)]
    conversion: ConversionArgs,
}

#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert webp",
    version,
    about = "Parallel TGS/Lottie JSON to transparent animated WebP converter"
)]
struct WebpCli {
    #[command(flatten)]
    conversion: ConversionArgs,
}

#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert gif",
    version,
    about = "Parallel TGS/Lottie JSON to animated GIF converter (FFmpeg palettegen/paletteuse)"
)]
struct GifCli {
    #[command(flatten)]
    conversion: ConversionArgs,
}

#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert telegram-download",
    version,
    about = "Download every sticker or custom emoji from a Telegram pack"
)]
struct TelegramDownloadCli {
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
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("telegram-download")) {
        return run_telegram_download();
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("mov")) {
        return run_mov();
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("webp")) {
        return run_webp();
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("gif")) {
        return run_gif();
    }

    let cli = Cli::parse();
    run_conversion(cli.conversion, OutputFormat::WebmVp9)
}

fn run_mov() -> Result<()> {
    let cli = MovCli::parse_from(
        std::iter::once(std::ffi::OsString::from("tgs-convert mov"))
            .chain(std::env::args_os().skip(2)),
    );
    run_conversion(cli.conversion, OutputFormat::MovProres4444)
}

fn run_webp() -> Result<()> {
    let cli = WebpCli::parse_from(
        std::iter::once(std::ffi::OsString::from("tgs-convert webp"))
            .chain(std::env::args_os().skip(2)),
    );
    run_conversion(cli.conversion, OutputFormat::Webp)
}

fn run_gif() -> Result<()> {
    let cli = GifCli::parse_from(
        std::iter::once(std::ffi::OsString::from("tgs-convert gif"))
            .chain(std::env::args_os().skip(2)),
    );
    run_conversion(cli.conversion, OutputFormat::Gif)
}

fn run_conversion(cli: ConversionArgs, output_format: OutputFormat) -> Result<()> {
    let fps = cli.fps.unwrap_or_else(|| output_format.default_fps());
    let output = cli
        .output
        .unwrap_or_else(|| default_output(&cli.input, output_format));
    let report = convert(&ConvertOptions {
        input: cli.input,
        output: output.clone(),
        fps,
        width: cli.width,
        height: cli.height,
        quality: cli.quality,
        play_speed: cli.play_speed,
        rotation_degrees: cli.rotation,
        flip_horizontal: cli.flip_horizontal,
        flip_vertical: cli.flip_vertical,
        threads: cli.threads,
        ffmpeg: cli.ffmpeg,
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

fn run_telegram_download() -> Result<()> {
    let cli = TelegramDownloadCli::parse_from(
        std::iter::once(std::ffi::OsString::from("tgs-convert telegram-download"))
            .chain(std::env::args_os().skip(2)),
    );
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
