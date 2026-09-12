//! Integration tests that exercise the real bundled fixtures.
//!
//! The unit tests inside `src` only cover pure functions; these load actual
//! `.tgs` and Lottie JSON files and, when FFmpeg is present, run the whole
//! convert pipeline end to end.

use std::{fs, path::PathBuf, process::Command};

use tgs_convert::load_animation;

fn fixture_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a workspace parent")
        .join("tests/fixtures")
}

fn fixtures_with_extension(extension: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(fixture_directory())
        .expect("the fixture directory is readable")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(extension))
        .collect();
    paths.sort();
    paths
}

#[test]
fn every_tgs_fixture_loads() {
    let fixtures = fixtures_with_extension("tgs");
    assert!(
        fixtures.len() >= 5,
        "expected the bundled TGS fixtures, found {}",
        fixtures.len()
    );

    for path in fixtures {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let animation = load_animation(&path)
            .unwrap_or_else(|error| panic!("{name} failed to load: {error:#}"));
        assert!(animation.metadata.width > 0, "{name} has an empty width");
        assert!(animation.metadata.height > 0, "{name} has an empty height");
        assert!(
            animation.metadata.duration_seconds > 0.0,
            "{name} has a non-positive duration"
        );
    }
}

#[test]
fn every_json_fixture_loads() {
    let fixtures = fixtures_with_extension("json");
    assert!(!fixtures.is_empty(), "expected the bundled JSON fixtures");

    for path in fixtures {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let animation = load_animation(&path)
            .unwrap_or_else(|error| panic!("{name} failed to load: {error:#}"));
        assert!(animation.metadata.width > 0, "{name} has an empty width");
        assert!(
            animation.metadata.duration_seconds > 0.0,
            "{name} has a non-positive duration"
        );
    }
}

#[test]
fn gzip_compressed_lottie_loads_like_plain_json() {
    let source = fixture_directory().join("sample.lottie.json");
    let plain = load_animation(&source).expect("the plain JSON fixture loads");

    let temporary = tempfile::tempdir().expect("a temporary directory");
    let compressed = temporary.path().join("compressed.tgs");
    let encoded = {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(&fs::read(&source).expect("the fixture is readable"))
            .expect("gzip encoding succeeds");
        encoder.finish().expect("gzip stream finishes")
    };
    fs::write(&compressed, encoded).expect("the temporary file is writable");

    let decoded = load_animation(&compressed).expect("the gzip fixture loads");
    assert_eq!(decoded.metadata.width, plain.metadata.width);
    assert_eq!(decoded.metadata.height, plain.metadata.height);
    assert!(
        (decoded.metadata.duration_seconds - plain.metadata.duration_seconds).abs() < 1e-6,
        "gzip and plain JSON disagree on duration"
    );
}

/// Runs the built CLI end to end. Skipped when FFmpeg is not installed, so the
/// suite stays usable on machines that only have the Rust toolchain.
#[test]
fn cli_converts_a_fixture_to_every_format_when_ffmpeg_is_available() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg was not found on PATH");
        return;
    }

    let input = fixtures_with_extension("tgs")
        .into_iter()
        .next()
        .expect("at least one TGS fixture");
    let temporary = tempfile::tempdir().expect("a temporary directory");

    for (subcommand, extension) in [
        (None, "webm"),
        (Some("mov"), "mov"),
        (Some("webp"), "webp"),
        (Some("gif"), "gif"),
    ] {
        let output = temporary.path().join(format!("out.{extension}"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_tgs-convert"));
        if let Some(subcommand) = subcommand {
            command.arg(subcommand);
        }
        let status = command
            .arg(&input)
            .args(["--output"])
            .arg(&output)
            .args(["--fps", "5", "--threads", "2"])
            .status()
            .expect("the CLI runs");

        assert!(
            status.success(),
            "converting to {extension} failed with {status:?}"
        );
        assert!(
            output.is_file() && fs::metadata(&output).unwrap().len() > 0,
            "no output was written for {extension}"
        );
    }

    // The temporary frame directory must not outlive the run.
    let leftovers: Vec<_> = fs::read_dir(temporary.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("tgs-frames-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "frame directory leaked: {leftovers:?}"
    );
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
