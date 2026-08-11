# tgs-convert

tgs-convert is a standalone Rust CLI for converting Telegram TGS stickers and
Lottie JSON to transparent VP9 WebM, Apple ProRes 4444 MOV, animated WebP, or
animated GIF. It replaces the desktop project's TGS-to-WebM chain:

1. decompress a TGS when required;
2. render transparent RGBA PNG frames with vendored rlottie;
3. encode with FFmpeg libvpx-vp9/yuva420p, prores_ks/yuva444p10le, or
   libwebp_anim, or with gifski.

It does not depend on .NET, Avalonia, Python, or a GUI. FFmpeg 9.0 or newer,
built with libwebp support, is an explicit runtime dependency for WebM, MOV,
and WebP output. Verify animated WebP support with:

    ffmpeg -hide_banner -encoders | grep libwebp_anim

## Build

    cd tgs-convert
    cargo build --release

The Telegram rlottie feature vendors the renderer, so a C++ toolchain and CMake
are needed for the initial Cargo build.

On macOS, install Homebrew GCC once. It supplies the compatibility C++ library
name required by the current rlottie-sys build script:

    brew install gcc

The included Cargo configuration disables an upstream 32-bit ARM NEON assembly
path that does not link on Apple Silicon. Rendering remains CPU-parallel through
the worker pool.

## Usage

    ./target/release/tgs-convert ../测试.tgs \
      --output ../测试-rust.webm \
      --fps 240 \
      --quality 100 \
      --threads 8

The input may be .tgs, plain Lottie JSON, or gzip-compressed JSON. When
--output is omitted, the CLI writes alongside the input with a .webm extension.

## Desktop-option parity

| Desktop conversion option | CLI flag | Default |
| --- | --- | --- |
| Frame rate | --fps 1..240 | 60 (GIF: 50) |
| Width / height | --width, --height | animation size |
| Quality | --quality 0..100 | 100 |
| Playback speed | --play-speed 0.1..10 | 1.0 |
| Rotation | --rotation | 0 |
| Horizontal / vertical flip | --flip-horizontal, --flip-vertical | off |
| FFmpeg location (WebM, MOV, WebP) | --ffmpeg | ffmpeg |

Quality uses the same mappings as WebmConverter: quality 100 selects VP9 CRF
15 and cpu-used 0. VP9 output uses yuva420p, the four-plane format required to
preserve transparency.

## ProRes 4444 MOV

Use the mov subcommand for an alpha-preserving Apple ProRes 4444 file. It takes
the same conversion flags as the default WebM command and defaults to a .mov
output beside the input:

    ./target/release/tgs-convert mov ../测试.tgs \
      --output ../测试-prores4444.mov \
      --fps 240 \
      --quality 100 \
      --threads 8

The encoder is FFmpeg prores_ks with profile 4444, alpha_bits 16, and
yuva444p10le input. This retains alpha rather than compositing transparent
pixels against black. The shared --quality range maps to ProRes per-macroblock
bitrate limits while the 4444 profile and alpha precision remain fixed.

## Animated WebP

Use the webp subcommand to produce a lossless looping animated WebP with alpha:

    ./target/release/tgs-convert webp ../测试.tgs \
      --output ../测试.webp \
      --fps 60 \
      --quality 100 \
      --threads 8

WebP is encoded from the RGBA PNG sequence by the selected FFmpeg executable's
libwebp_anim encoder, so semi-transparent pixels remain transparent. Quality
100 selects lossless mode with effort 75 and compression level 6, matching the
former img2webp behavior. Lower values select lossy mode at the requested
quality, use `yuva420p`, and cap compression at level 5 to avoid libwebp_anim's
severe level-6 performance cliff. FFmpeg may coalesce
adjacent identical frames. After encoding, the CLI verifies the WebP frame
durations and corrects at most one millisecond on the final frame so the total
duration matches the integer-millisecond `frame_count / fps` timeline, instead
of using the desktop project's defective WebP timer. The output loops
indefinitely.

## Animated GIF

Use the gif subcommand to encode the same RGBA frame sequence through gifski:

    ./target/release/tgs-convert gif ../测试.tgs \
      --output ../测试.gif \
      --fps 50 \
      --quality 100 \
      --threads 8

GIF frame delays are stored in 10ms units. Therefore GIF accepts only 1, 2, 4,
5, 10, 20, 25, or 50 FPS; 50 FPS is the default and maximum. GIF supports one
transparent palette entry, so fully transparent pixels remain transparent but
semi-transparent edges are quantized to binary transparency by the format.

## Frame sampling note

The output duration and frame count follow the requested --fps exactly.
rlottie exposes integral source-frame rendering, however, so requesting an
output fps above the animation's own frame rate repeats the nearest source
frame. This differs from Skottie sub-frame property interpolation while keeping
the requested WebM timebase and duration correct.

## Concurrency and cleanup

--threads creates that many independent rlottie workers, capped at the output
frame count. Each worker owns an animation instance and a reusable surface,
then writes a disjoint contiguous frame range into a temporary directory. This
avoids shared renderer state and lets CPU rendering and PNG writing proceed
concurrently. FFmpeg receives the same thread count.

The frame directory is automatically removed on success, failure, or Ctrl-C.
Ctrl-C also stops the FFmpeg process if encoding has started.

## Telegram sticker-pack download

The CLI can download every file in a Telegram sticker or custom-emoji pack.
Give it an addstickers or addemoji link (or just the pack name):

    ./target/release/tgs-convert telegram-download \
      https://t.me/addstickers/HotCherry \
      --output-dir ../HotCherry \
      --threads 8

It queries the pack metadata, then fetches file metadata and downloads sticker
files concurrently. Files retain their Telegram extension (typically .tgs,
.webp, or .webm) and use the same unique-ID naming convention as the desktop
project; custom emoji files are prefixed with their emoji where filesystem-safe.
When --output-dir is omitted, the set name becomes the output directory.

The bot token is not embedded in the binary. It is read from the macOS
Keychain (service `TGSConvert`, account `tgs-convert`) or from the Windows
Credential Locker (`Windows.Security.Credentials.PasswordVault`, same
resource/user names). A one-shot token can be passed on the command line with
`--token <token>` for a single run.
