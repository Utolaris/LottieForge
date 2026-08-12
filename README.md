# TGSConvert

一个把 Telegram TGS 贴纸（Lottie 动画）转换为透明视频/动画格式的 Rust CLI。

## 功能

- TGS / Lottie JSON / gzip JSON 输入
- 输出格式：
  - WebM（VP9 + alpha，`tgs-convert`，默认命令）
  - MOV（Apple ProRes 4444 + alpha，`tgs-convert mov`）
  - WebP（动画无损 + alpha，`tgs-convert webp`）
  - GIF（FFmpeg 编码器，仅支持 1/2/4/5/10/20/25/50 FPS，`tgs-convert gif`）
- Telegram 贴纸包下载（`tgs-convert telegram-download`，支持 `t.me/addstickers` 与 `t.me/addemoji`）
- 并行渲染：多个独立 rlottie worker 分帧渲染，帧序列写入临时目录
- 参数：`--fps`（GIF 上限 50）、`--quality`、`--width/--height`、`--play-speed`、`--rotation`、`--flip-horizontal/--flip-vertical`、`--threads`、`--ffmpeg`

## 依赖

运行时：

- FFmpeg 9（必须为包含 `libwebp` 的 full 构建；WebM / MOV / WebP / GIF 编码）

Homebrew 的普通 `ffmpeg` formula 不包含 libwebp，必须安装 `ffmpeg-full`
并把它的 keg-only bin 目录加入 PATH：

```sh
brew install ffmpeg-full
export PATH="/opt/homebrew/opt/ffmpeg-full/bin:$PATH"
```

可用 `ffmpeg -hide_banner -encoders | grep libwebp_anim` 确认 FFmpeg
包含动画 WebP 编码器。

构建时：

- Rust（stable，含 `x86_64-pc-windows-gnu` target）
- CMake 与 git（rlottie-sys 构建 vendored rlottie）
- zig（Windows x86-64 交叉编译）
- llvm-ar 或 mingw-w64（归档器；zig 0.16 的 `zig ar` 有缺陷时的替代）

## 构建

macOS 原生：

```sh
cd tgs-convert
cargo build --release
```

Windows x86-64（64 位）交叉编译，使用 zig 作为 C/C++ 工具链与链接器：

```sh
zig build
# 产物：zig-out/bin/tgs-convert.exe
```

## 用法

```sh
# TGS -> WebM
tgs-convert 测试.tgs --output out.webm --fps 60 --quality 100 --threads 8

# TGS -> ProRes 4444 MOV
tgs-convert mov 测试.tgs --output out.mov --fps 240 --quality 100

# TGS -> WebP
tgs-convert webp 测试.tgs --output out.webp --fps 60 --quality 100

# TGS -> GIF（50 FPS 上限，帧延迟为 10ms 整数倍）
tgs-convert gif 测试.tgs --output out.gif --fps 50 --quality 100

# 下载完整 Telegram 贴纸包
tgs-convert telegram-download https://t.me/addstickers/SomePack --output-dir ./packs/SomePack
```

## 说明

- WebP 由 FFmpeg 的 `libwebp_anim` 编码；质量 100 使用无损模式、effort 75 与压缩级别 6，低于 100 使用对应数值的有损质量、`yuva420p` 与压缩级别 5，以避开 `libwebp_anim` 在级别 6 的严重性能退化，均保留 alpha。FFmpeg 可合并相邻相同帧，CLI 会校正末帧最多 1ms 的取整差，使总时长与帧数/帧率对应的毫秒时间线一致。
- GIF 帧延迟以 10ms 为单位，因此帧率被限制在 50 FPS 以内且必须能整除 1000ms。
- Telegram bot token 不再内嵌进二进制，改从操作系统凭据库读取：
  - macOS：系统钥匙串（Keychain）。存入方式：
    `security add-generic-password -s TGSConvert -a tgs-convert -w '<token>'`
  - Windows：Windows Credential Locker
    （`Windows.Security.Credentials.PasswordVault`，资源名 `TGSConvert`，
    用户名 `tgs-convert`）。
  - 临时单次使用：`tgs-convert telegram-download <链接> --token '<token>'`。

## CI

`.github/workflows/ci.yml` 在 macOS runner 上用 zig 交叉编译 x86-64 二进制，并在 Windows runner 上安装 FFmpeg 后实际执行 WebM、MOV、WebP 与 GIF 转换验证。

## 下载

正式版二进制发布在 GitHub Releases：macOS（Apple Silicon）与 Windows x86-64（`tgs-convert.exe`）各一份。
