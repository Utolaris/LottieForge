mod ffmpeg;
mod options;
mod render;
pub mod telegram;

use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use tempfile::Builder;

pub use options::{ConvertOptions, OutputFormat};
use render::{RenderSettings, load_animation, render_sequence};

#[derive(Clone, Copy, Debug)]
pub struct ConversionReport {
    pub width: usize,
    pub height: usize,
    pub frames: usize,
    pub duration_seconds: f64,
}

pub fn convert(options: &ConvertOptions) -> Result<ConversionReport> {
    options.validate()?;
    let output_parent = output_parent(&options.output)?;
    fs::create_dir_all(output_parent)
        .with_context(|| format!("failed to create {}", output_parent.display()))?;
    let absolute_output = absolute_output_path(&options.output)?;

    let cancel = Arc::new(AtomicBool::new(false));
    install_cancel_handler(Arc::clone(&cancel))?;
    let animation = load_animation(&options.input)?;
    let (width, height) = resolve_output_size(
        options.width,
        options.height,
        animation.metadata.width,
        animation.metadata.height,
    )?;
    let render_settings = RenderSettings {
        fps: options.fps,
        play_speed: options.play_speed,
        width,
        height,
        rotation_degrees: options.rotation_degrees,
        flip_horizontal: options.flip_horizontal,
        flip_vertical: options.flip_vertical,
        threads: options.threads,
    };
    let duration_seconds = animation.metadata.duration_seconds / options.play_speed;
    let temporary_directory = Builder::new()
        .prefix("tgs-frames-")
        .tempdir_in(output_parent)
        .context("failed to create temporary frame directory")?;

    eprintln!(
        "Rendering {} at {width}x{height}, {} fps, {} worker(s)",
        options.input.display(),
        options.fps,
        options.threads
    );
    let frames = render_sequence(
        &animation,
        render_settings,
        temporary_directory.path(),
        Arc::clone(&cancel),
    )?;
    if cancel.load(Ordering::Acquire) {
        bail!("conversion cancelled");
    }

    eprintln!(
        "Encoding {} as {}",
        options.output.display(),
        options.output_format.description()
    );
    ffmpeg::encode(
        temporary_directory.path(),
        options,
        frames,
        &absolute_output,
        Arc::clone(&cancel),
    )?;

    Ok(ConversionReport {
        width,
        height,
        frames,
        duration_seconds,
    })
}

fn install_cancel_handler(cancel: Arc<AtomicBool>) -> Result<()> {
    ctrlc::set_handler(move || {
        cancel.store(true, Ordering::Release);
        eprintln!("\nCancellation requested; stopping after the active frame.");
    })
    .map_err(|error| anyhow!("failed to install Ctrl-C handler: {error}"))
}

fn output_parent(output: &Path) -> Result<&Path> {
    Ok(output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(".")))
}

fn absolute_output_path(output: &Path) -> Result<std::path::PathBuf> {
    if output.is_absolute() {
        Ok(output.to_owned())
    } else {
        std::env::current_dir()
            .context("failed to resolve current working directory")
            .map(|directory| directory.join(output))
    }
}

fn resolve_output_size(
    requested_width: Option<usize>,
    requested_height: Option<usize>,
    intrinsic_width: usize,
    intrinsic_height: usize,
) -> Result<(usize, usize)> {
    match (requested_width, requested_height) {
        (Some(width), Some(height)) => Ok((width, height)),
        (Some(width), None) => {
            let height = scale_dimension(intrinsic_height, width, intrinsic_width)?;
            Ok((width, height))
        }
        (None, Some(height)) => {
            let width = scale_dimension(intrinsic_width, height, intrinsic_height)?;
            Ok((width, height))
        }
        (None, None) => Ok((intrinsic_width, intrinsic_height)),
    }
}

fn scale_dimension(source: usize, target_other: usize, source_other: usize) -> Result<usize> {
    if source_other == 0 {
        bail!("cannot scale output size from a zero intrinsic dimension");
    }
    let scaled =
        (source as u128 * target_other as u128 + source_other as u128 / 2) / source_other as u128;
    if scaled > usize::MAX as u128 {
        bail!("requested output size is out of range");
    }
    Ok((scaled as usize).max(1))
}

#[cfg(test)]
mod tests {
    use super::resolve_output_size;

    #[test]
    fn single_width_preserves_aspect_ratio() {
        assert_eq!(
            resolve_output_size(Some(256), None, 512, 384).unwrap(),
            (256, 192)
        );
    }

    #[test]
    fn single_height_preserves_aspect_ratio() {
        assert_eq!(
            resolve_output_size(None, Some(192), 512, 384).unwrap(),
            (256, 192)
        );
    }

    #[test]
    fn explicit_both_dimensions_win() {
        assert_eq!(
            resolve_output_size(Some(100), Some(200), 512, 384).unwrap(),
            (100, 200)
        );
        assert_eq!(
            resolve_output_size(None, None, 512, 384).unwrap(),
            (512, 384)
        );
    }
}
