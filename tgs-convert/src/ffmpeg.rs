use std::{
    io::{BufRead, BufReader},
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

use crate::formats::{EncodePlan, Progress, normalize_webp_duration};

/// Runs the FFmpeg invocation a format asked for.
///
/// This module knows nothing about individual formats — that lives in
/// `formats`. It only knows how to start FFmpeg, report progress and honour
/// cancellation.
pub fn encode(
    frames_directory: &Path,
    program: &Path,
    plan: &EncodePlan,
    output: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    match plan {
        EncodePlan::Gif { arguments } => {
            encode_gif(frames_directory, program, arguments, output, cancel)
        }
        EncodePlan::Single {
            arguments,
            progress,
        } => encode_with_progress(
            frames_directory,
            program,
            arguments,
            progress,
            output,
            cancel,
        ),
        EncodePlan::Webp {
            arguments,
            progress,
            expected_duration_ms,
        } => {
            encode_with_progress(
                frames_directory,
                program,
                arguments,
                progress,
                output,
                Arc::clone(&cancel),
            )?;
            normalize_webp_duration(output, *expected_duration_ms)
        }
    }
}

fn encode_with_progress(
    frames_directory: &Path,
    program: &Path,
    arguments: &[String],
    progress: &Progress,
    output: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let mut full_arguments = arguments.to_vec();
    full_arguments.extend([
        "-progress".to_owned(),
        "pipe:1".to_owned(),
        "-nostats".to_owned(),
    ]);

    let mut child = spawn(
        frames_directory,
        program,
        &full_arguments,
        output,
        Stdio::piped(),
    )?;
    let stdout = child.stdout.take().context("FFmpeg stdout was not piped")?;
    let progress_cancel = Arc::clone(&cancel);
    let duration_seconds = progress.duration_seconds;
    let progress_name = progress.label;
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
    Ok(())
}

fn encode_gif(
    frames_directory: &Path,
    program: &Path,
    arguments: &[String],
    output: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let mut child = spawn(
        frames_directory,
        program,
        arguments,
        output,
        Stdio::inherit(),
    )?;
    let status = wait_for_child(&mut child, cancel, "ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    Ok(())
}

fn spawn(
    frames_directory: &Path,
    program: &Path,
    arguments: &[String],
    output: &Path,
    stdout: Stdio,
) -> Result<std::process::Child> {
    Command::new(program)
        .current_dir(frames_directory)
        .args(arguments)
        .arg(output)
        .stdout(stdout)
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", program.display()))
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
