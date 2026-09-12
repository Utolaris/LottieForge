use std::{
    fs::File,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use crate::options::MAX_FRAMES;
use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use rlottie::{Animation, Size, Surface};

#[derive(Clone, Copy, Debug)]
pub struct AnimationMetadata {
    pub width: usize,
    pub height: usize,
    pub duration_seconds: f64,
}

#[derive(Clone, Debug)]
pub struct LoadedAnimation {
    json: Arc<Vec<u8>>,
    resource_path: PathBuf,
    pub metadata: AnimationMetadata,
}

#[derive(Clone, Copy, Debug)]
pub struct RenderSettings {
    pub fps: u32,
    pub play_speed: f64,
    pub width: usize,
    pub height: usize,
    pub rotation_degrees: f64,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    pub threads: usize,
}

impl RenderSettings {
    pub fn output_frame_count(self, duration_seconds: f64) -> Result<usize> {
        let duration = duration_seconds / self.play_speed;
        if !duration.is_finite() || duration <= 0.0 {
            bail!("the animation has no positive duration");
        }

        let frames = (duration * f64::from(self.fps)).ceil();
        if !frames.is_finite() {
            bail!("the animation is too long to render");
        }
        if frames > MAX_FRAMES as f64 {
            bail!(
                "this animation needs {frames:.0} frames at {fps} fps, which exceeds the \
                 {MAX_FRAMES} frame limit; raise --play-speed or lower --fps",
                fps = self.fps,
            );
        }
        Ok(frames as usize)
    }
}

pub fn load_animation(input: &Path) -> Result<LoadedAnimation> {
    let json = read_lottie_json(input)?;
    if json.contains(&0) {
        bail!("the animation JSON contains a NUL byte");
    }

    let resource_path = input
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let animation = Animation::from_data(json.clone(), "tgs-convert-inspect", &resource_path)
        .ok_or_else(|| anyhow!("rlottie could not load {}", input.display()))?;
    let size = animation.size();
    let duration_seconds = declared_duration_seconds(&json).unwrap_or_else(|| animation.duration());
    if size.width == 0 || size.height == 0 {
        bail!("rlottie reported an empty animation viewport");
    }
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        bail!("rlottie reported an invalid animation duration");
    }

    Ok(LoadedAnimation {
        json: Arc::new(json),
        resource_path,
        metadata: AnimationMetadata {
            width: size.width,
            height: size.height,
            duration_seconds,
        },
    })
}

pub fn render_sequence(
    animation: &LoadedAnimation,
    settings: RenderSettings,
    output_directory: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<usize> {
    let frame_count = settings.output_frame_count(animation.metadata.duration_seconds)?;
    let workers = settings.threads.min(frame_count).max(1);
    let progress = Arc::new(RenderProgress::new(frame_count));
    let error = Arc::new(Mutex::new(None));

    let worker_result: Result<()> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker_id in 0..workers {
            let frame_start = worker_id * frame_count / workers;
            let frame_end = (worker_id + 1) * frame_count / workers;
            let json = Arc::clone(&animation.json);
            let resource_path = animation.resource_path.clone();
            let cancel = Arc::clone(&cancel);
            let progress = Arc::clone(&progress);
            let error = Arc::clone(&error);
            let metadata = animation.metadata;
            let output_directory = output_directory.to_path_buf();

            handles.push(scope.spawn(move || {
                let result = render_worker(
                    &json,
                    &resource_path,
                    worker_id,
                    frame_start,
                    frame_end,
                    metadata,
                    settings,
                    &output_directory,
                    &cancel,
                    &progress,
                );
                if let Err(render_error) = result {
                    cancel.store(true, Ordering::Release);
                    let mut slot = error.lock().expect("render error mutex poisoned");
                    if slot.is_none() {
                        *slot = Some(render_error);
                    }
                }
            }));
        }

        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow!("a frame-rendering worker panicked"))?;
        }
        Ok(())
    });
    worker_result?;

    if let Some(render_error) = error.lock().expect("render error mutex poisoned").take() {
        return Err(render_error);
    }
    if cancel.load(Ordering::Acquire) {
        bail!("conversion cancelled");
    }

    eprintln!();
    Ok(frame_count)
}

#[allow(clippy::too_many_arguments)]
fn render_worker(
    json: &[u8],
    resource_path: &Path,
    worker_id: usize,
    frame_start: usize,
    frame_end: usize,
    metadata: AnimationMetadata,
    settings: RenderSettings,
    output_directory: &Path,
    cancel: &AtomicBool,
    progress: &RenderProgress,
) -> Result<()> {
    let cache_key = format!("tgs-convert-{}-{worker_id}", std::process::id());
    let mut animation = Animation::from_data(json.to_vec(), cache_key, resource_path)
        .ok_or_else(|| anyhow!("rlottie could not initialize renderer worker {worker_id}"))?;
    let mut surface = Surface::new(Size::new(settings.width, settings.height));

    for frame_index in frame_start..frame_end {
        if cancel.load(Ordering::Acquire) {
            bail!("conversion cancelled");
        }

        let output_time = frame_index as f64 / f64::from(settings.fps);
        let animation_time = (output_time * settings.play_speed).min(metadata.duration_seconds);
        let position = (animation_time / metadata.duration_seconds).clamp(0.0, 1.0);
        let source_frame = animation.frame_at_pos(position as f32);
        animation.render(source_frame, &mut surface);

        let rgba = transform_frame(
            surface_to_premultiplied_rgba(&surface),
            settings.width,
            settings.height,
            settings,
        );
        let path = output_directory.join(format!("{frame_index:05}.png"));
        write_png(&path, settings.width, settings.height, &rgba)
            .with_context(|| format!("failed to write {}", path.display()))?;
        progress.report();
    }

    Ok(())
}

fn read_lottie_json(input: &Path) -> Result<Vec<u8>> {
    let bytes =
        std::fs::read(input).with_context(|| format!("failed to read {}", input.display()))?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoded = Vec::new();
        GzDecoder::new(Cursor::new(bytes))
            .read_to_end(&mut decoded)
            .with_context(|| format!("failed to decompress {}", input.display()))?;
        Ok(decoded)
    } else {
        Ok(bytes)
    }
}

fn declared_duration_seconds(json: &[u8]) -> Option<f64> {
    let root: serde_json::Value = serde_json::from_slice(json).ok()?;
    let frame_rate = root.get("fr")?.as_f64()?;
    let in_point = root.get("ip")?.as_f64()?;
    let out_point = root.get("op")?.as_f64()?;
    let duration = (out_point - in_point) / frame_rate;
    (frame_rate > 0.0 && duration.is_finite() && duration > 0.0).then_some(duration)
}

fn surface_to_premultiplied_rgba(surface: &Surface) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(surface.width() * surface.height() * 4);
    for pixel in surface.data() {
        rgba.extend_from_slice(&[pixel.r, pixel.g, pixel.b, pixel.a]);
    }
    rgba
}

fn transform_frame(
    mut rgba: Vec<u8>,
    width: usize,
    height: usize,
    settings: RenderSettings,
) -> Vec<u8> {
    if settings.flip_horizontal || settings.flip_vertical {
        rgba = flip_rgba(
            &rgba,
            width,
            height,
            settings.flip_horizontal,
            settings.flip_vertical,
        );
    }

    let angle = settings.rotation_degrees.rem_euclid(360.0);
    if angle.abs() > f64::EPSILON {
        rgba = rotate_premultiplied_rgba(&rgba, width, height, angle.to_radians());
    }

    unpremultiply_rgba(&mut rgba);
    rgba
}

fn flip_rgba(
    source: &[u8],
    width: usize,
    height: usize,
    horizontal: bool,
    vertical: bool,
) -> Vec<u8> {
    let mut output = vec![0; source.len()];
    for y in 0..height {
        for x in 0..width {
            let source_x = if horizontal { width - 1 - x } else { x };
            let source_y = if vertical { height - 1 - y } else { y };
            let source_offset = (source_y * width + source_x) * 4;
            let destination_offset = (y * width + x) * 4;
            output[destination_offset..destination_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    output
}

fn rotate_premultiplied_rgba(source: &[u8], width: usize, height: usize, radians: f64) -> Vec<u8> {
    let mut output = vec![0; source.len()];
    let cosine = radians.cos();
    let sine = radians.sin();
    let center_x = (width as f64 - 1.0) / 2.0;
    let center_y = (height as f64 - 1.0) / 2.0;

    for y in 0..height {
        for x in 0..width {
            let dx = x as f64 - center_x;
            let dy = y as f64 - center_y;
            let source_x = cosine * dx + sine * dy + center_x;
            let source_y = -sine * dx + cosine * dy + center_y;
            let pixel = bilinear_sample(source, width, height, source_x, source_y);
            let destination_offset = (y * width + x) * 4;
            output[destination_offset..destination_offset + 4].copy_from_slice(&pixel);
        }
    }
    output
}

fn bilinear_sample(source: &[u8], width: usize, height: usize, x: f64, y: f64) -> [u8; 4] {
    let left = x.floor() as isize;
    let top = y.floor() as isize;
    let fraction_x = x - left as f64;
    let fraction_y = y - top as f64;
    let samples = [
        sample_pixel(source, width, height, left, top),
        sample_pixel(source, width, height, left + 1, top),
        sample_pixel(source, width, height, left, top + 1),
        sample_pixel(source, width, height, left + 1, top + 1),
    ];
    let weights = [
        (1.0 - fraction_x) * (1.0 - fraction_y),
        fraction_x * (1.0 - fraction_y),
        (1.0 - fraction_x) * fraction_y,
        fraction_x * fraction_y,
    ];

    let mut output = [0; 4];
    for channel in 0..4 {
        let value = samples
            .iter()
            .zip(weights)
            .map(|(sample, weight)| f64::from(sample[channel]) * weight)
            .sum::<f64>();
        output[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    output
}

fn sample_pixel(source: &[u8], width: usize, height: usize, x: isize, y: isize) -> [u8; 4] {
    if x < 0 || y < 0 || x >= width as isize || y >= height as isize {
        return [0; 4];
    }
    let offset = (y as usize * width + x as usize) * 4;
    [
        source[offset],
        source[offset + 1],
        source[offset + 2],
        source[offset + 3],
    ]
}

fn unpremultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let expanded = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = expanded.min(255) as u8;
        }
    }
}

fn write_png(path: &Path, width: usize, height: usize, rgba: &[u8]) -> Result<()> {
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(())
}

struct RenderProgress {
    completed: AtomicUsize,
    last_reported: AtomicUsize,
    total: usize,
    report_every: usize,
}

impl RenderProgress {
    fn new(total: usize) -> Self {
        Self {
            completed: AtomicUsize::new(0),
            last_reported: AtomicUsize::new(0),
            total,
            report_every: (total / 100).max(1),
        }
    }

    fn report(&self) {
        let completed = self.completed.fetch_add(1, Ordering::AcqRel) + 1;
        let last = self.last_reported.load(Ordering::Acquire);
        if completed != self.total && completed.saturating_sub(last) < self.report_every {
            return;
        }
        if self
            .last_reported
            .compare_exchange(last, completed, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            eprint!(
                "\rRendering transparent PNG frames: {completed}/{} ({:.0}%)",
                self.total,
                completed as f64 * 100.0 / self.total as f64
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RenderSettings, declared_duration_seconds, flip_rgba, transform_frame};

    #[test]
    fn declared_duration_includes_the_lottie_out_point() {
        assert_eq!(
            declared_duration_seconds(br#"{ "fr": 60, "ip": 30, "op": 210 }"#),
            Some(3.0)
        );
    }

    #[test]
    fn output_frame_count_honours_play_speed() {
        let settings = RenderSettings {
            fps: 240,
            play_speed: 2.0,
            width: 2,
            height: 2,
            rotation_degrees: 0.0,
            flip_horizontal: false,
            flip_vertical: false,
            threads: 1,
        };
        assert_eq!(settings.output_frame_count(3.0).unwrap(), 360);
    }

    #[test]
    fn output_frame_count_rejects_animations_beyond_the_frame_limit() {
        let settings = RenderSettings {
            fps: 60,
            play_speed: 1.0,
            width: 2,
            height: 2,
            rotation_degrees: 0.0,
            flip_horizontal: false,
            flip_vertical: false,
            threads: 1,
        };
        // A hostile Lottie can self-report any duration it likes.
        let error = settings
            .output_frame_count(10_000.0)
            .expect_err("a 10,000 second animation must be rejected")
            .to_string();
        assert!(error.contains("frame limit"), "{error}");
        assert!(error.contains("600000"), "{error}");

        // A much slower play speed stretches the same animation back over the limit.
        let stretched = RenderSettings {
            play_speed: 0.1,
            ..settings
        };
        assert!(stretched.output_frame_count(100.0).is_err());
        assert_eq!(settings.output_frame_count(3.0).unwrap(), 180);
    }

    #[test]
    fn horizontal_flip_reorders_pixels() {
        let input = vec![1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255];
        let output = flip_rgba(&input, 2, 2, true, false);
        assert_eq!(output[0], 2);
        assert_eq!(output[4], 1);
        assert_eq!(output[8], 4);
        assert_eq!(output[12], 3);
    }

    #[test]
    fn transparent_pixels_remain_transparent_after_transform() {
        let settings = RenderSettings {
            fps: 60,
            play_speed: 1.0,
            width: 2,
            height: 2,
            rotation_degrees: 45.0,
            flip_horizontal: true,
            flip_vertical: true,
            threads: 1,
        };
        let output = transform_frame(vec![0; 16], 2, 2, settings);
        assert!(output.chunks_exact(4).all(|pixel| pixel[3] == 0));
    }
}
