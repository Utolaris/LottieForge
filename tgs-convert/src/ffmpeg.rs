use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::options::{ConvertOptions, OutputFormat};

pub fn encode(
    frames_directory: &Path,
    options: &ConvertOptions,
    frame_count: usize,
    width: usize,
    height: usize,
    output: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    if options.output_format == OutputFormat::Gif {
        return encode_gif(
            frames_directory,
            options,
            frame_count,
            width,
            height,
            output,
            cancel,
        );
    }
    let duration_seconds = frame_count as f64 / f64::from(options.fps);
    let expected_webp_duration_ms = (frame_count as u64 * 1_000) / u64::from(options.fps);
    let fps = options.fps.to_string();
    let frame_count_argument = frame_count.to_string();
    let threads = options.threads.to_string();
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-y".to_owned(),
        "-framerate".to_owned(),
        fps,
        "-i".to_owned(),
        "%05d.png".to_owned(),
        "-frames:v".to_owned(),
        frame_count_argument,
    ];

    let progress_name = match options.output_format {
        OutputFormat::WebmVp9 => {
            let crf = options.vp9_crf().to_string();
            let cpu_used = options.vp9_cpu_used().to_string();
            arguments.extend([
                "-c:v".to_owned(),
                "libvpx-vp9".to_owned(),
                "-crf".to_owned(),
                crf,
                "-b:v".to_owned(),
                "0".to_owned(),
                "-cpu-used".to_owned(),
                cpu_used,
                "-threads".to_owned(),
                threads.clone(),
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
            "VP9 alpha WebM"
        }
        OutputFormat::MovProres4444 => {
            let bits_per_mb = options.prores_bits_per_mb().to_string();
            arguments.extend([
                "-c:v".to_owned(),
                "prores_ks".to_owned(),
                "-profile:v".to_owned(),
                "4".to_owned(),
                "-bits_per_mb".to_owned(),
                bits_per_mb,
                "-alpha_bits".to_owned(),
                "16".to_owned(),
                "-threads".to_owned(),
                threads.clone(),
                "-pix_fmt".to_owned(),
                "yuva444p10le".to_owned(),
                "-movflags".to_owned(),
                "+faststart".to_owned(),
            ]);
            "ProRes 4444 alpha MOV"
        }
        OutputFormat::Webp => {
            append_webp_arguments(&mut arguments, options.quality, &threads);
            "animated WebP"
        }
        OutputFormat::Gif => {
            unreachable!("GIF is encoded by gifski before FFmpeg arguments are built")
        }
    };
    arguments.extend([
        "-progress".to_owned(),
        "pipe:1".to_owned(),
        "-nostats".to_owned(),
    ]);
    let mut child = Command::new(&options.ffmpeg)
        .current_dir(frames_directory)
        .args(&arguments)
        .arg(output)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", options.ffmpeg.display()))?;

    let stdout = child
        .stdout
        .take()
        .expect("ffmpeg stdout was configured as piped");
    let progress_cancel = Arc::clone(&cancel);
    let progress_reader = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut last_percent = 0_u8;
        for line in reader.lines().map_while(|line| line.ok()) {
            if progress_cancel.load(Ordering::Acquire) {
                break;
            }
            if let Some(value) = line.strip_prefix("out_time_us=") {
                if let Ok(microseconds) = value.parse::<f64>() {
                    let percent = (microseconds / 1_000_000.0 / duration_seconds * 100.0)
                        .clamp(0.0, 100.0) as u8;
                    if percent > last_percent || percent == 100 {
                        last_percent = percent;
                        eprint!("\rEncoding {progress_name}: {percent}%");
                    }
                }
            }
        }
    });

    let status = loop {
        if cancel.load(Ordering::Acquire) {
            child
                .kill()
                .context("failed to stop ffmpeg after cancellation")?;
            let _ = child.wait();
            let _ = progress_reader.join();
            bail!("conversion cancelled");
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = progress_reader.join();
    eprintln!();

    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    if options.output_format == OutputFormat::Webp {
        normalize_webp_duration(output, expected_webp_duration_ms)?;
    }
    Ok(())
}

fn append_webp_arguments(arguments: &mut Vec<String>, quality: u8, threads: &str) {
    let lossless = quality == 100;
    let encoder_quality = if lossless { 75 } else { quality };
    let compression_level = if lossless { 6 } else { 5 };
    let pixel_format = if lossless { "bgra" } else { "yuva420p" };
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
        threads.to_owned(),
        "-loop".to_owned(),
        "0".to_owned(),
        "-f".to_owned(),
        "webp".to_owned(),
    ]);
}

fn normalize_webp_duration(output: &Path, expected_duration_ms: u64) -> Result<()> {
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
        bail!(
            "FFmpeg produced a WebP duration of {total_duration_ms}ms; expected {expected_duration_ms}ms"
        );
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

fn encode_gif(
    frames_directory: &Path,
    options: &ConvertOptions,
    frame_count: usize,
    width: usize,
    height: usize,
    output: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let fps = options.fps.to_string();
    let quality = options.quality.max(1).to_string();
    let width = width.to_string();
    let height = height.to_string();
    let mut command = Command::new("gifski");
    command
        .current_dir(frames_directory)
        .args([
            "--fps",
            &fps,
            "--quality",
            &quality,
            "--repeat",
            "0",
            "--width",
            &width,
            "--height",
            &height,
            "--no-sort",
            "--output",
        ])
        .arg(output);
    for frame in 0..frame_count {
        command.arg(format!("{frame:05}.png"));
    }

    let mut child = command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start gifski; install it or make it available on PATH")?;
    let status = wait_for_child(&mut child, cancel, "gifski")?;
    if !status.success() {
        bail!("gifski exited with {status}");
    }
    Ok(())
}

fn wait_for_child(
    child: &mut std::process::Child,
    cancel: Arc<AtomicBool>,
    name: &str,
) -> Result<std::process::ExitStatus> {
    loop {
        if cancel.load(Ordering::Acquire) {
            child
                .kill()
                .with_context(|| format!("failed to stop {name} after cancellation"))?;
            let _ = child.wait();
            bail!("conversion cancelled");
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{append_webp_arguments, normalize_webp_duration};

    #[test]
    fn webp_uses_ffmpeg_animation_encoder_in_lossless_mode_at_quality_100() {
        let mut arguments = Vec::new();
        append_webp_arguments(&mut arguments, 100, "8");

        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-c:v", "libwebp_anim"])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-lossless", "1"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-compression_level", "6"])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-quality", "75"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-pix_fmt", "bgra"])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-loop", "0"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-f", "webp"]));
    }

    #[test]
    fn webp_uses_requested_lossy_quality_and_yuva_pixel_format() {
        let mut arguments = Vec::new();
        append_webp_arguments(&mut arguments, 0, "2");

        assert!(arguments.windows(2).any(|pair| pair == ["-lossless", "0"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-quality", "0"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-compression_level", "5"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-pix_fmt", "yuva420p"])
        );

        let mut quality_one_arguments = Vec::new();
        append_webp_arguments(&mut quality_one_arguments, 1, "2");
        assert!(
            quality_one_arguments
                .windows(2)
                .any(|pair| pair == ["-quality", "1"])
        );
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
}
