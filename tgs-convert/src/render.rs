use std::{
    fs::File,
    io::{Cursor, Read},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use crate::options::MAX_FRAMES;
use crate::transform::FrameTransform;
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

impl LoadedAnimation {
    /// Each worker needs its own rlottie instance, and rlottie keys its
    /// internal animation cache by this string, so it has to be unique per
    /// worker.
    fn new_renderer(&self, worker_id: usize) -> Result<Animation> {
        let cache_key = next_cache_key(&format!("worker-{worker_id}"));
        Animation::from_data(self.json.to_vec(), cache_key, &self.resource_path)
            .ok_or_else(|| anyhow!("rlottie could not initialize renderer worker {worker_id}"))
    }
}

/// Builds a cache key that has never been used before in this process.
///
/// rlottie keeps a process-global animation cache keyed by this string: asking
/// for a key it has already seen hands back the *first* animation stored under
/// it, whatever data you pass this time. A constant key would therefore mean
/// only one animation could ever be loaded per process, and concurrent loads
/// would race over the same entry.
fn next_cache_key(prefix: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tgs-convert-{prefix}-{}-{unique}", std::process::id())
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

/// The output timeline, derived once from the source duration.
///
/// Computing both numbers together keeps the duration reported at the end in
/// step with the number of frames that were actually rendered.
#[derive(Clone, Copy, Debug)]
pub struct Timeline {
    pub frames: usize,
    pub duration_seconds: f64,
}

pub fn timeline(source_seconds: f64, play_speed: f64, fps: u32) -> Result<Timeline> {
    let duration_seconds = source_seconds / play_speed;
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        bail!("the animation has no positive duration");
    }

    let frames = (duration_seconds * f64::from(fps)).ceil();
    if !frames.is_finite() {
        bail!("the animation is too long to render");
    }
    if frames > MAX_FRAMES as f64 {
        bail!(
            "this animation needs {frames:.0} frames at {fps} fps, which exceeds the \
             {MAX_FRAMES} frame limit; raise --play-speed or lower --fps"
        );
    }
    Ok(Timeline {
        frames: frames as usize,
        duration_seconds,
    })
}

pub fn load_animation(input: &Path) -> Result<LoadedAnimation> {
    let json = read_lottie_json(input)?;
    if json.contains(&0) {
        bail!("the animation JSON contains a NUL byte");
    }

    let resource_path = crate::parent_directory(input).to_path_buf();
    let animation = Animation::from_data(json.clone(), next_cache_key("inspect"), &resource_path)
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
    timeline: Timeline,
    output_directory: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<usize> {
    let workers = settings.threads.min(timeline.frames).max(1);
    let progress = Arc::new(RenderProgress::new(timeline.frames));
    let error = Arc::new(Mutex::new(None));

    let worker_result: Result<()> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker_id in 0..workers {
            let frame_start = worker_id * timeline.frames / workers;
            let frame_end = (worker_id + 1) * timeline.frames / workers;
            let worker = FrameWorker {
                animation,
                settings,
                output_directory,
                cancel: &cancel,
                progress: &progress,
            };
            let cancel = Arc::clone(&cancel);
            let error = Arc::clone(&error);
            handles.push(scope.spawn(move || {
                if let Err(render_error) = worker.render(worker_id, frame_start..frame_end) {
                    cancel.store(true, Ordering::Release);
                    let mut slot = error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    if let Some(render_error) = error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Err(render_error);
    }
    if cancel.load(Ordering::Acquire) {
        bail!("conversion cancelled");
    }

    eprintln!();
    Ok(timeline.frames)
}

/// Everything one rendering thread needs, bundled so the worker does not have
/// to take ten separate arguments.
struct FrameWorker<'a> {
    animation: &'a LoadedAnimation,
    settings: RenderSettings,
    output_directory: &'a Path,
    cancel: &'a AtomicBool,
    progress: &'a RenderProgress,
}

impl FrameWorker<'_> {
    fn render(&self, worker_id: usize, frames: Range<usize>) -> Result<()> {
        let mut animation = self.animation.new_renderer(worker_id)?;
        let mut surface = Surface::new(Size::new(self.settings.width, self.settings.height));
        let transform = FrameTransform {
            rotation_degrees: self.settings.rotation_degrees,
            flip_horizontal: self.settings.flip_horizontal,
            flip_vertical: self.settings.flip_vertical,
        };

        for frame_index in frames {
            if self.cancel.load(Ordering::Acquire) {
                bail!("conversion cancelled");
            }

            let output_time = frame_index as f64 / f64::from(self.settings.fps);
            let animation_time = (output_time * self.settings.play_speed)
                .min(self.animation.metadata.duration_seconds);
            let position =
                (animation_time / self.animation.metadata.duration_seconds).clamp(0.0, 1.0);
            let source_frame = animation.frame_at_pos(position as f32);
            animation.render(source_frame, &mut surface);

            let rgba = transform.apply(
                surface_to_premultiplied_rgba(&surface),
                self.settings.width,
                self.settings.height,
            );
            let path = self.output_directory.join(format!("{frame_index:05}.png"));
            write_png(&path, self.settings.width, self.settings.height, &rgba)
                .with_context(|| format!("failed to write {}", path.display()))?;
            self.progress.report();
        }

        Ok(())
    }
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
    total: usize,
}

impl RenderProgress {
    fn new(total: usize) -> Self {
        Self {
            completed: AtomicUsize::new(0),
            total,
        }
    }

    /// Reports only when the whole percent changes, which caps the output at
    /// about a hundred lines no matter how many frames there are.
    fn report(&self) {
        let completed = self.completed.fetch_add(1, Ordering::AcqRel) + 1;
        let percent = completed * 100 / self.total;
        if completed != self.total && percent == (completed - 1) * 100 / self.total {
            return;
        }
        eprint!(
            "\rRendering transparent PNG frames: {completed}/{} ({percent}%)",
            self.total
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{declared_duration_seconds, timeline};

    #[test]
    fn declared_duration_includes_the_lottie_out_point() {
        assert_eq!(
            declared_duration_seconds(br#"{ "fr": 60, "ip": 30, "op": 210 }"#),
            Some(3.0)
        );
    }

    #[test]
    fn timeline_honours_play_speed() {
        let timeline = timeline(3.0, 2.0, 240).unwrap();
        assert_eq!(timeline.frames, 360);
        assert_eq!(timeline.duration_seconds, 1.5);
    }

    #[test]
    fn timeline_rejects_animations_beyond_the_frame_limit() {
        // A hostile Lottie can self-report any duration it likes.
        let error = timeline(10_000.0, 1.0, 60)
            .expect_err("a 10,000 second animation must be rejected")
            .to_string();
        assert!(error.contains("frame limit"), "{error}");
        assert!(error.contains("600000"), "{error}");

        // A much slower play speed stretches the same animation over the limit.
        assert!(timeline(100.0, 0.1, 60).is_err());
        assert!(timeline(1.0, 10.0, 60).is_ok());
    }
}
