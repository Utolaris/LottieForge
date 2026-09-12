# TGSConvert 代码审计报告

- **日期**：2026-09-12
- **范围**：`tgs-convert/src`（6 个文件 / 2023 行 Rust）、`build.zig`、`build.rs`、`.github/workflows/ci.yml`、`tests/fixtures`
- **审计维度**：安全漏洞 / 过度耦合 / 过度防御性编程
- **基线验证**：`cargo clippy --workspace --all-targets -- -D warnings` 通过；`cargo test` 18 passed；`git ls-files` 33 个文件，无构建产物、无 `.DS_Store` 入库

结论：**代码风格是干净的（clippy 零告警、纯函数测试覆盖良好），问题集中在三个层面——凭据处理、架构分层、防御位置错配。** 没有发现可直接远程利用的漏洞；最高优先级问题是凭据回显。

---

## 修复进度（2026-09-12 更新）

| 编号 | 状态 | 落地方式 |
|---|---|---|
| V1 | ✅ 已修复 | `redacted_error()` 在错误出口逐层脱敏；`TelegramDownloadOptions` 手写 `Debug` |
| V2 | ✅ 已修复 | `sanitize_file_stem()` 净化 `file_unique_id`；`download_one()` 增加落点目录断言 |
| V3 | ✅ 已修复 | 客户端加 15s 连接 / 120s 请求超时；429 按 `Retry-After` 退避重试 2 次 |
| V4 | ✅ 已修复 | `MAX_FRAMES` / `MAX_DIMENSION` 两个上限，在写出任何帧之前失败 |
| C1 | ✅ 已修复 | `main.rs` 改用 clap `Subcommand`，删掉 4 份重复 CLI 与 `nth(1)` 分发 |
| C2 | ✅ 已修复 | 新建 `formats.rs` 统一持有格式知识，`ffmpeg.rs` 退化为只执行进程 |
| C3 | ✅ 已修复 | 引入 `EncodeSettings`，`ConvertOptions` 不再穿透到编码器 |
| C4 | ✅ 已修复 | `FrameWorker` + `LoadedAnimation::new_renderer`，删掉全仓唯一 clippy 抑制 |
| C5 | ✅ 已修复 | `timeline()` 一处同时算出帧数与时长 |
| C6 | ✅ 已修复 | 抽 `cancel.rs`；父目录助手统一为 `parent_directory()` |
| C7 | ✅ 已修复 | 纯像素变换拆到 `transform.rs` |
| D1 | ✅ 已修复 | `output_parent` 去掉永不失败的 `Result` |
| D2 | ✅ 已修复 | 边界常量单点定义，clap 与 `validate()` 共用 |
| D3 | ✅ 已修复 | 删掉不可能的溢出守卫，额度换成真实上限 |
| D4 | ✅ 已修复 | WebP 时长偏差降级为警告，不再丢弃成品文件 |
| D5 | ✅ 已修复 | `expect` 换成 `poisoned.into_inner()` / `context` |
| D6 | ✅ 已修复 | `RenderProgress` 双原子 + CAS 简化为单原子百分比判断 |
| T1 | ✅ 已修复 | CI 新增 `lint-and-test` job |
| T2 | ✅ 已修复 | 新增 `tests/cli.rs`，5 个 `.tgs` 与 JSON 夹具全部投入使用 |
| D7 | ⬜ 未处理 | NUL 字节全量扫描——有实际理由（rlottie 是 C API），改动收益低，保留 |

第一批改动落在 `telegram.rs`、`lib.rs`、`render.rs`、`options.rs`。

## 第二批：架构收敛与工程化

新增 `src/formats.rs`、`src/transform.rs`、`src/cancel.rs`，`ffmpeg.rs` 从 482 行降到 191 行，`main.rs` 从 249 行降到 216 行（并删掉约 120 行重复内容）。无新增依赖。

校验：`cargo fmt --all --check` 通过、`cargo clippy --workspace --all-targets -- -D warnings` 通过、`cargo test` **33 passed**（单元 29 + 集成 4，原为 18 个单元测试且无集成测试）。

### C1 的一处意外：clap 4.6 不支持「必填位置参数 + 可选子命令」

原计划用 `subcommand_negates_reqs = true` 让顶层 `<INPUT>` 在出现子命令时不再必填，这也是 clap 文档给出的写法。但实测在 clap 4.6.5 下**不生效**：

```
$ tgs-convert mov file.tgs -o out.mov --fps 5
error: the following required argument was not provided: input

Usage: tgs-convert [OPTIONS] <INPUT>
       tgs-convert [OPTIONS] [INPUT] <COMMAND>
```

对照 `mov --fps 5`（子命令自身缺参）报的是子命令的用法，说明 `mov` 确实被当作子命令解析了；但只要子命令解析成功，父级仍会校验自己的 `input`——即 `has_subcmd` 在父级校验时并未生效。交换字段顺序、去掉 `args_conflicts_with_subcommands` 都不影响。

**采用的结构**：顶层 `DefaultConversion.input` 为 `Option<PathBuf>`（仅此一处可选，换到 `into_conversion()` 里检查），子命令里的 `ConversionArgs.input` 保持必填，因此 `tgs-convert mov --fps 5` 这类误用仍由 clap 给出带用法的标准报错。共享选项抽到 `SharedConversionOptions`，三个结构体之间没有重复字段。

同时用 `propagate_version = true` 修掉了原来的 `tgs-convert gif 0.1.0`（版本号里带空格）：现在输出 `tgs-convert-gif 0.1.0`。

### C2 的收益：枚举穷尽性回来了

`ffmpeg.rs:100` 原先的 `unreachable!("GIF is encoded by encode_gif before FFmpeg arguments are built")` 之所以存在，是因为 GIF 用提前 return 绕开了那个 `match`——代价是编译器不再检查穷尽性，新增格式会静默走到死分支。现在 `OutputFormat::plan()` 是穷尽的，`encode()` 的 `match` 也是穷尽的，新增格式不给参数就编译不过；`formats::tests::every_format_produces_a_plan` 会遍历全部变体。

### 端到端回归

```
子命令 mov / webp / gif   → 均成功
裸形态（默认 WebM）        → 成功
文件名为 mov 的输入        → 成功（修复前会被误判为子命令）
--help                    → 列出全部子命令（修复前一个都不显示）
--version / gif --version → tgs-convert 0.1.0 / tgs-convert-gif 0.1.0
顶层选项 + 子命令混用      → 报错而非静默忽略
```

集成测试 `tests/cli.rs` 覆盖：5 个 `.tgs` 与 2 个 JSON 夹具全部可加载；gzip 与明文 JSON 解析结果一致；有 FFmpeg 时跑通 4 种格式并断言临时帧目录无残留。

### V1 端到端验证

用等价最小程序复现过之后，又临时把 `API_BASE` 指向不可达主机、跑真实的 `telegram-download` 路径（事后已还原）：

```
error: failed to fetch Telegram sticker set HotCherry: Telegram request getStickerSet failed:
error sending request for url (https://nonexistent-host.invalid/bot<redacted>/getStickerSet?name=HotCherry):
client error (Connect): tls handshake eof
```

三点确认：token 变成 `<redacted>`；**排障信息完整保留**（`client error (Connect)`、`tls handshake eof`、method 与 `name=HotCherry` 查询串都还在）；非敏感上下文（`failed to fetch … HotCherry`）照旧。修复做法是遍历 `source` 链逐层替换，而不是只 `to_string()` 一层——否则会丢掉 `tls handshake eof` 这类关键线索。

### 新增测试（5 个）

- `unique_id_cannot_escape_the_output_directory` —— `../../etc/passwd` → `etcpasswd.tgs`，且路径只有 1 个分量
- `sanitize_file_stem_keeps_the_telegram_alphabet` —— 真实 `AgADGAADwDZPEw` 原样保留；空串与纯目录分隔符回落为 `sticker`；超长截断到 64
- `bot_tokens_are_removed_from_error_text`
- `redacted_error_keeps_the_cause_chain_without_the_token` —— 三层 source 链，断言脱敏后仍含每层原因
- `options_debug_redacts_the_token` —— `{:?}` 不再吐 token

### V4 端到端验证

用真实 CLI 跑恶意输入（`sample.lottie.json` 改 `fr=1, op=1e9`）：

```
=== 攻击 1：自报 10 亿秒时长 ===
error: this animation needs 60000000000 frames at 60 fps, which exceeds the
       20000 frame limit; raise --play-speed or lower --fps

=== 攻击 2：--width 99999 ===
error: output size 99999x99999 exceeds the 4096px per-side limit; lower --width / --height

=== 对照：正常转换仍工作 ===
Wrote /tmp/ok.webm (512x512, 8 frames, 1.500s)
```

上限生效，且临时帧目录无残留（`TempDir` 的 Drop 已清理）。

**额度选择**：帧上限取 `MAX_FRAMES = 20_000`（60 fps 下约 5.5 分钟，240 fps 上限下约 1.4 分钟），足以覆盖真实贴纸（通常 3 秒 / 180 帧）与较长 Lottie；分辨率上限取 `MAX_DIMENSION = 4096`。

**新增测试（2 个）**：`output_frame_count_rejects_animations_beyond_the_frame_limit`（含 `--play-speed 0.1` 反向拉长时长的用例）、`output_size_is_capped_on_either_side`（含"按宽高比缩放出的另一侧同样受限"用例）。

**遗留**：D3 中 `scale_dimension` 的 `scaled > usize::MAX` 守卫仍在（防止 `as usize` 静默截断，虽然在新上限下已几乎不可达）；`duration / play_speed` 的重复计算（C5）未动，`ConversionReport` 的时长与真实帧数仍各自计算。

**一处小瑕疵**：攻击 1 会先打印 `Rendering … 8 worker(s)` 再报错——该提示位于 `lib.rs` 中 `render_sequence` 之前。仅影响观感，未改动。

### 顺带修掉的一个 UX 问题

`install_cancel_handler` 原先在 `get_sticker_set` **之后**才安装，导致元数据请求期间 Ctrl-C 完全无效。现已提前到第一次请求之前，且新增的退避等待按 200ms 切片检查取消标志——否则引入重试反而会造成最长 60s 的不可中断等待。

---

## 一、问题清单（按优先级）

| 编号 | 等级 | 类别 | 问题 | 位置 |
|---|---|---|---|---|
| V1 | 中高 | 凭据泄露 | bot token 拼进 URL，经 `reqwest` 错误对象回显到 stderr | `telegram.rs:325,361` + `main.rs:134` |
| V2 | 中 | 路径穿越 | 下载文件名直接使用服务端返回的 `file_unique_id`，未净化 | `telegram.rs:395-403` |
| V3 | 中 | 可用性 | HTTP 客户端无超时、无 429 重试，服务端挂起即永久阻塞 | `telegram.rs:90-93, 371` |
| V4 | 中 | 资源耗尽 | 帧数与分辨率无上限，恶意 Lottie 可写满磁盘 | `render.rs:43-54`、`options.rs:58-89` |
| V5 | 低 | 凭据暴露 | `--token` 走命令行（`ps` 可见）；`TelegramDownloadOptions` 派生 `Debug` 未脱敏 | `main.rs:126-127`、`telegram.rs:19-25` |
| C1 | 高 | 过度耦合 | `main.rs` 4 份重复 CLI 结构 + 手写 `nth(1)` 子命令分发 | `main.rs:12-180` |
| C2 | 高 | 过度耦合 | `OutputFormat` 的格式知识分裂在 `options.rs` 与 `ffmpeg.rs`，含 `unreachable!()` 死分支 | `options.rs:13-38`、`ffmpeg.rs:44-103` |
| C3 | 中 | 过度耦合 | `ConvertOptions`（13 字段）作为 god DTO 被穿透传递，`ffmpeg::encode` 只用其中 5 个 | `ffmpeg.rs:18-24` |
| C4 | 中 | 过度耦合 | `render_worker` 10 个参数并 `#[allow(clippy::too_many_arguments)]` | `render.rs:157-169` |
| C5 | 中 | 重复逻辑 | `duration / play_speed` 在 `lib.rs` 与 `render.rs` 各实现一遍 | `lib.rs:55`、`render.rs:44` |
| C6 | 低 | 重复逻辑 | Ctrl-C handler 与「取父目录」助手在多个模块重复 | `lib.rs:98-111`、`telegram.rs:419-425`、`render.rs:63-67` |
| C7 | 低 | 职责过载 | `render.rs` 一个模块混了 JSON 加载、像素变换、PNG 编码、进度上报 | `render.rs` 全文 |
| D1 | 中 | 过度防御 | `output_parent()` 声明 `Result` 但永不失败，`?` 是噪声 | `lib.rs:106-111` |
| D2 | 中 | 过度防御 | `validate()` 与 clap `range` 重复校验，错误文案两套 | `options.rs:58-89`、`main.rs:33,45` |
| D3 | 中 | 防御错位 | 守住了不可能的 `usize::MAX` 溢出，漏掉了现实的帧数爆炸 | `lib.rs:149`、`render.rs:50` |
| D4 | 中 | 过度防御 | WebP 时长偏差 >1ms 直接 `bail!`，丢弃已经编码成功的文件 | `ffmpeg.rs:292-296` |
| D5 | 低 | 风格不一致 | `expect("... poisoned")` 与 anyhow 错误处理混用 | `render.rs:129,146`、`telegram.rs:148,164` |
| D6 | 低 | 过度防御 | `RenderProgress` 用双原子 + CAS 节流一行进度输出 | `render.rs:365-400` |
| T1 | 中 | 测试/CI | CI 从不执行 `cargo test` / `clippy` / `fmt`，`zig build test` 也无人调用 | `ci.yml` |
| T2 | 低 | 仓库卫生 | 7 个 fixture 中 6 个（约 346 KB）无任何引用 | `tests/fixtures` |

---

## 二、漏洞详情

### V1（中高）Telegram bot token 回显到 stderr —— 已实测复现

`telegram_api()` 把 token 拼进 URL 路径：

```rust
// telegram.rs:325
let url = format!("{API_BASE}/bot{token}/{method}");
```

`reqwest` 的 `Error::Display` 会把请求 URL 一并打印（`reqwest-0.12.28/src/error.rs:268`：`write!(f, " for url ({url})")`），而请求失败时错误对象确实带上了 url（`async_impl/client.rs:3071`：`error::request(e).with_url(self.url.clone())`）。调用方只加了 `with_context`，**保留了 source 链**：

```rust
// telegram.rs:326-332
client.get(url).query(&query).send()
    .with_context(|| format!("Telegram API request {method} failed"))?   // source 未被丢弃
```

最后 `main.rs:134` 用 `{:#}` 打印完整因果链，token 原样落到 stderr。

**实测输出**（用等价最小复现程序，假 token）：

```
error: Telegram API request getMe failed: error sending request for url
(https://nonexistent-host.invalid/bot7654321:AAHfakeTokenValueDoNotUse/getMe): client error (Connect): tls handshake eof
```

同一个泄露点还有 3 处：`telegram.rs:361`（文件下载 URL）、`telegram.rs:332`（`error_for_status()` 造的 Status 错误同样带 url）、`telegram.rs:150`（把 `{error:#}` 塞进 `anyhow!(error)`，等于二次固化）。

**影响**：CI 日志、粘贴给他人排障的终端输出、issue 里的报错截图，都会带出可直接调用 Bot API 的完整凭据。任何网络抖动（DNS、TLS、代理、超时）都会触发。

**修复**：在错误出口处强制脱敏，而不是依赖调用方纪律。

```rust
/// 建议放在 telegram.rs
fn redact(text: &str, token: &str) -> String {
    text.replace(token, "<redacted>")
}

fn telegram_api<T>(client: &Client, token: &str, method: &str, query: [(&str, &str); 1]) -> Result<T>
where T: for<'de> Deserialize<'de>,
{
    let url = format!("{API_BASE}/bot{token}/{method}");
    let response = client
        .get(url)
        .query(&query)
        .send()
        // 关键：不要用 with_context 追加 source，而是自己拼一条不含 token 的消息
        .map_err(|error| anyhow!("Telegram API request {method} failed: {}", redact(&error.to_string(), token)))?
        .error_for_status()
        .map_err(|error| anyhow!("Telegram API request {method} returned an HTTP error: {}", redact(&error.to_string(), token)))?;
    // .json() 同理
    ...
}
```

更彻底的做法是引入 `struct Secret(String)`，手写 `Debug`/`Display` 输出 `***`，只提供 `expose()` 取值，这样「不小心 `{:?}` 一下」也不会泄露。同时给 `TelegramDownloadOptions` 手写 `Debug`（当前 `#[derive(Debug)]` 会把 token 打进任何调试输出）。

补充：建议在 CI 的 Windows 步骤后加一条日志脱敏断言，或至少在 README 里提醒「排障时不要直接贴报错」。

### V2（中）下载文件名未净化 —— 与已有的 emoji 净化不对称

```rust
// telegram.rs:395-403
fn filename(item: &DownloadItem, extension: &str) -> String {
    let emoji = item.emoji.as_deref().map(clean_emoji).unwrap_or_default();
    let stem = if emoji.is_empty() {
        item.unique_id.clone()          // ← 未经任何净化
    } else {
        format!("{emoji}_{}", item.unique_id)   // ← 同样未经净化
    };
    format!("{stem}.{extension}")
}
```

`unique_id` 来自 `sticker.file_unique_id`（`telegram.rs:54`），是**服务端返回的字符串**，代码对 emoji 做了 `clean_emoji()` 去掉分隔符和控制字符，却唯独漏了这个字段。随后：

```rust
// telegram.rs:359-360
let destination = output_directory.join(filename);
let partial_destination = destination.with_extension(format!("{extension}.part"));
```

`Path::join` 遇到 `..` 会原样保留。若响应里的 `file_unique_id` 为 `../../.ssh/authorized_keys`，`File::create` 就会在目标目录之外写入（`..` 指向的父目录存在，因此可穿越）。

**利用前提**：需要控制 Telegram API 响应——即中间人（需受信证书）或 Telegram 侧被攻陷。因此不是「可直接远程利用」，而是**缺失的纵深防御**：当前安全性完全依赖上游响应的合法性，且与代码里已有的 emoji 净化思路自相矛盾。

**修复**：把净化抽成一个函数，对**所有**来自远端的路径片段统一应用；并加一道「落点必须在目标目录内」的断言。

```rust
fn sanitize_stem(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(64)
        .collect();
    if cleaned.is_empty() { "sticker".to_owned() } else { cleaned }
}

fn filename(item: &DownloadItem, extension: &str) -> String {
    let emoji = item.emoji.as_deref().map(clean_emoji).unwrap_or_default();
    let unique = sanitize_stem(&item.unique_id);
    let stem = if emoji.is_empty() { unique } else { format!("{emoji}_{unique}") };
    format!("{stem}.{extension}")
}
```

```rust
// download_one() 里的兜底断言
if destination.parent() != Some(output_directory) {
    bail!("refusing to write outside the output directory: {}", destination.display());
}
```

### V3（中）HTTP 无超时、无 429 退避

```rust
// telegram.rs:90-93
let client = Client::builder()
    .user_agent("tgs-convert Telegram sticker downloader")
    .build()
```

没有 `.timeout()` / `.connect_timeout()`。`io::copy(&mut response, &mut partial)`（`telegram.rs:371`）是阻塞读取，取消检查只发生在文件与文件之间（`telegram.rs:136`）——**对端建立连接后不再发数据，进程会永久挂住，Ctrl-C 也要等到这一轮 I/O 返回才生效**（实际上取消逻辑根本不在读循环里）。

另外 Telegram 对 Bot API 有速率限制，超限返回 429 且带 `retry_after`；当前实现直接失败。

**修复**：

```rust
let client = Client::builder()
    .user_agent("tgs-convert Telegram sticker downloader")
    .connect_timeout(Duration::from_secs(15))
    .timeout(Duration::from_secs(120))     // 整体上限，必要时改为读超时
    .build()?;
```

并在 `telegram_api()` 中对 429 解析 `retry_after` 做有限次退避重试（建议最多 2 次，尊重 `retry_after`）。

### V4（中）帧数与分辨率无上限

```rust
// render.rs:43-54
let frames = (duration * f64::from(self.fps)).ceil();
if !frames.is_finite() || frames > usize::MAX as f64 {   // 上界约 1.8e19，形同虚设
    bail!("requested output frame count is out of range");
}
```

`duration` 来自 Lottie 自报的 `(op - ip) / fr`（`render.rs:215-222`），`.tgs` 是**从 Telegram 下载的不可信文件**。构造 `{"fr":1,"ip":0,"op":1000000000}` 即得 10 亿秒时长，`--fps 60` 下 `frame_count ≈ 6e10`，渲染循环会持续写出 512×512 的 PNG（每帧约 1 MB）直到写满磁盘。`--width/--height` 同样无上界，`Surface::new(Size::new(w, h))` 会直接分配。

**修复**：把上限放在 `validate()` 里，作为输入契约的一部分。

```rust
// options.rs，常量集中定义，供 clap 与 lib 共用
pub const MAX_FPS: u32 = 240;
pub const MAX_DIMENSION: usize = 4096;
pub const MAX_FRAMES: usize = 100_000;   // 约 55 分钟 @30fps，按需要调整

// convert() 中在展开帧数前检查
let timeline = timeline(animation.metadata.duration_seconds, options.play_speed, options.fps)?;
if timeline.frames > MAX_FRAMES {
    bail!("animation needs {} frames, exceeding the {} frame limit; raise --play-speed or lower --fps",
          timeline.frames, MAX_FRAMES);
}
```

### V5（低）token 的命令行暴露与 Debug 未脱敏

- `--token` 走 argv，同机其他用户可通过 `ps` 看到（README 已注明是「临时单次使用」，可接受，但建议补充 `TGS_TOKEN` 环境变量或 `--token-stdin` 作为更安全的替代）。
- `TelegramDownloadOptions` 派生 `Debug` 且持有 `token: String`，任何一次 `dbg!`/日志都会打印凭据。建议手写 `Debug` 输出 `<redacted>`。

### V6（低）`--ffmpeg` 接受任意可执行路径

`Command::new(&options.ffmpeg)`（`ffmpeg.rs:109,326`）会执行用户指定的任意程序。这是有意的可配置项（README 说明用于指向 `ffmpeg-full`），风险自担，无需改动。仅提示：若未来出现「配置文件/环境变量传入 ffmpeg 路径」的场景，需要校验来源。

---

## 三、过度耦合

### C1（高）`main.rs` 的 4 份重复 CLI 与手写分发

`Cli`、`MovCli`、`WebpCli`、`GifCli`（`main.rs:12-104`）四个结构体除 `name`/`about` 属性外**完全相同**，都只是 `#[command(flatten)] ConversionArgs`。分发靠手动比较 argv：

```rust
// main.rs:140-156
if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("telegram-download")) {
    return run_telegram_download();
}
if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("mov")) {
    return run_mov();
}
...
```

已实测的三个后果：

1. **子命令在帮助里完全不可见。** `tgs-convert --help` 的 Usage 只有 `tgs-convert [OPTIONS] <INPUT>`，`mov` / `webp` / `gif` / `telegram-download` 一个字都没提，只能靠 README。
2. **文件名与子命令名冲突即不可转换。** 实测：把输入文件命名为 `mov`，执行 `tgs-convert mov --output out.webm` 得到
   `error: the following required arguments were not provided: <INPUT>`。同名文件 `webp` / `gif` / `telegram-download` 同理。
3. **版本号带空格。** `tgs-convert gif --version` 输出 `tgs-convert gif 0.1.0`。

**修复**：交给 clap。`subcommand_negates_reqs` + `args_conflicts_with_subcommands` 正好表达「带子命令时顶层必填参数不再必填」，可以完整保留 `tgs-convert x.tgs`（默认 WebM）的旧行为。

```rust
#[derive(Debug, Parser)]
#[command(
    name = "tgs-convert",
    version,
    about = "Parallel TGS/Lottie JSON to transparent VP9 WebM converter",
    subcommand_negates_reqs = true,          // 出现子命令时忽略顶层必填
    args_conflicts_with_subcommands = true,
)]
struct Cli {
    #[command(flatten)]
    conversion: ConversionArgs,             // 默认形态：TGS -> WebM
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// TGS -> transparent ProRes 4444 MOV
    Mov(ConversionArgs),
    /// TGS -> transparent animated WebP
    Webp(ConversionArgs),
    /// TGS -> animated GIF (1/2/4/5/10/20/25/50 fps)
    Gif(ConversionArgs),
    /// Download every sticker from a Telegram pack
    TelegramDownload(TelegramArgs),
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None                                  => run_conversion(cli.conversion, OutputFormat::WebmVp9),
        Some(Command::Mov(a))                 => run_conversion(a, OutputFormat::MovProres4444),
        Some(Command::Webp(a))                => run_conversion(a, OutputFormat::Webp),
        Some(Command::Gif(a))                 => run_conversion(a, OutputFormat::Gif),
        Some(Command::TelegramDownload(a))    => run_telegram_download(a),
    }
}
```

净效果：删掉 3 个结构体、3 个 `run_*` 函数和整段 `nth(1)` 分发（约 120 行），`--help` 自动列出子命令，文件名冲突消失。`TelegramArgs` 也可顺势把 `link_or_name` 从 `String` 保留不变，但去掉 `resolve_bot_token(Option<&str>)` 里的空串再判断（clap 可保证非空）。

### C2（高）格式知识分裂在两个模块

「一种输出格式」的知识现在散落三处：

| 知识 | 位置 |
|---|---|
| 扩展名、描述、默认帧率 | `options.rs:13-38` |
| 编码器与参数 | `ffmpeg.rs:44-103` |
| 子命令名与 about 文案 | `main.rs:73-104` |

`ffmpeg.rs:100-102` 还留了一个死分支：

```rust
OutputFormat::Gif => {
    unreachable!("GIF is encoded by encode_gif before FFmpeg arguments are built")
}
```

这个 `unreachable!()` 本身就是耦合的症状：GIF 的特殊性（需要 `palettegen`/`paletteuse`、需要独立参数通道）从 `encode()` 的早期分支里「绕开」了这个 match，导致枚举穷尽性检查失效，编译器再也帮不上忙——**新增格式时不会报错，只会静默走到 `unreachable!()` 或忘记加参数。**

**修复**：把「格式 → 编码计划」收敛成一处纯函数，`ffmpeg.rs` 退化为「拿参数、起进程、显示进度」。

```rust
// options.rs（或新建 formats.rs）
pub struct EncodeSettings<'a> {
    pub program: &'a Path,
    pub fps: u32,
    pub frame_count: usize,
    pub quality: u8,
    pub threads: usize,
}

/// 编码计划：一条命令行 + 是否需要进度解析，或一条独立的 GIF 计划
pub enum EncodePlan {
    Ffmpeg { arguments: Vec<String>, progress_label: &'static str },
    Gif    { arguments: Vec<String> },
}

impl OutputFormat {
    pub fn plan(self, settings: &EncodeSettings<'_>) -> EncodePlan {
        match self {
            Self::WebmVp9       => EncodePlan::Ffmpeg { arguments: vp9_arguments(settings), progress_label: "VP9 alpha WebM" },
            Self::MovProres4444 => EncodePlan::Ffmpeg { arguments: prores_arguments(settings), progress_label: "ProRes 4444 alpha MOV" },
            Self::Webp          => EncodePlan::Ffmpeg { arguments: webp_arguments(settings), progress_label: "animated WebP" },
            Self::Gif           => EncodePlan::Gif { arguments: gif_arguments(settings) },
        }
    }
}
```

`match` 变成穷尽的、无死分支的，GIF 走 `EncodePlan::Gif` 分支而不是「提前 return 绕过」。加一种格式时编译器会在 `plan()` 和 `file_extension()` 处同时报错，这就是想要的效果。

### C3（中）`ConvertOptions` 是穿透各层的 god DTO

```rust
// ffmpeg.rs:18-24 —— 收下 13 个字段的 options，只用到 5 个
pub fn encode(
    frames_directory: &Path,
    options: &ConvertOptions,     // ffmpeg / fps / quality / threads / output_format
    frame_count: usize,
    output: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()>
```

`render` 侧已经做对了——`render_sequence` 拿的是专用的 `RenderSettings`（`render.rs:30-40`）。`ffmpeg` 侧没有对等处理，于是：

- `ffmpeg` 模块被迫了解 `ConvertOptions` 的全部形状（改任一无关字段都要重新编译它）；
- 「quality 数值 → 编码器参数」的**策略**留在 DTO 上（`options.rs:92-135` 的 `vp9_crf` / `vp9_cpu_used` / `prores_bits_per_mb`），使配置对象承担了编码决策。

**修复**：照 `RenderSettings` 的先例派生 `EncodeSettings`（见 C2 代码），`ffmpeg.rs` 只依赖它和 `OutputFormat`。

### C4（中）`render_worker` 10 参数 + clippy 抑制

```rust
// render.rs:157-169
#[allow(clippy::too_many_arguments)]
fn render_worker(
    json: &[u8], resource_path: &Path, worker_id: usize, frame_start: usize, frame_end: usize,
    metadata: AnimationMetadata, settings: RenderSettings, output_directory: &Path,
    cancel: &AtomicBool, progress: &RenderProgress,
) -> Result<()>
```

`#[allow]` 是症状不是原因。根因是 `LoadedAnimation` **没有真正封装**：它把 `json` / `resource_path` 声明为私有字段，但 `render_sequence` 与 `render_worker`（同模块，可以访问私有字段）直接把字段拆开往线程里搬，于是 worker 只好逐个接参数。全仓唯一的 `#[allow]` 出现在这里（clippy 零告警是刻意维持的），说明这是唯一越线的地方。

**修复**：给 `LoadedAnimation` 一个构造渲染器的方法，把字段所有权锁回类型内部：

```rust
impl LoadedAnimation {
    /// 每个 worker 需要独立的 rlottie 实例；cache_key 必须每 worker 唯一。
    fn new_renderer(&self, worker_id: usize) -> Result<Animation> {
        let cache_key = format!("tgs-convert-{}-{worker_id}", std::process::id());
        Animation::from_data(self.json.to_vec(), cache_key, &self.resource_path)
            .ok_or_else(|| anyhow!("rlottie could not initialize renderer worker {worker_id}"))
    }
}

struct WorkerContext<'a> {
    animation: &'a LoadedAnimation,
    settings: RenderSettings,
    output_directory: &'a Path,
    cancel: &'a AtomicBool,
    progress: &'a RenderProgress,
}

impl WorkerContext<'_> {
    fn render(&self, worker_id: usize, frames: Range<usize>) -> Result<()> { ... }
}
```

参数从 10 个降到 3 个，`#[allow]` 可以删掉。

### C5（中）时间线公式重复实现

```rust
// lib.rs:55
let duration_seconds = animation.metadata.duration_seconds / options.play_speed;

// render.rs:44
let duration = duration_seconds / self.play_speed;
```

同一个域规则写了两遍，而且**两处的用途不同**：`lib.rs` 的结果进 `ConversionReport`，`render.rs` 的结果决定实际帧数。任何一处改动（例如将来改成「按帧对齐」）都会让「报告里的时长」与「真实产出时长」静默分叉。

**修复**：抽一个 `Timeline`，一次算清两个值：

```rust
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
    if !frames.is_finite() || frames > MAX_FRAMES as f64 {
        bail!("animation needs {frames} frames, exceeding the {MAX_FRAMES} frame limit");
    }
    Ok(Timeline { frames: frames as usize, duration_seconds })
}
```

顺带把 D3（上限校验错位）一并修掉。

### C6（低）重复的辅助函数

- Ctrl-C handler：`lib.rs:98-104` 与 `telegram.rs:419-425` 是同一段逻辑的两个副本（仅提示文案不同）。抽到 `cancel.rs`，`install_cancel_handler(msg: &'static str) -> Result<Arc<AtomicBool>>`。
- 「取父目录，空则 `.`」：`lib.rs:106-111` 与 `render.rs:63-67` 各写一遍。

### C7（低）`render.rs` 职责过载

单文件 454 行混了 4 件事：输入解码（gzip/JSON、`read_lottie_json`）、纯像素变换（flip/rotate/bilinear/unpremultiply）、PNG 编码、进度上报。其中纯像素变换部分恰好是测试覆盖最好的部分（`render.rs:402-454`）。建议拆出 `transform.rs`（纯函数，易测）与 `render.rs`（线程与 I/O），顺带让 C4 的改动只落在 `render.rs`。

---

## 四、过度防御性编程

这一节的核心观察：**防御写在了不会出问题的地方，真正会出问题的地方反而没有防御。**

### D3（中）防御错位 —— 最典型的一例

两处守卫拦的是数学上不可能发生的情形：

```rust
// lib.rs:147-151 —— usize×usize 用 u128 计算，最大 (2^64-1)^2 < u128::MAX，乘积不会溢出
let scaled = (source as u128 * target_other as u128 + source_other as u128 / 2) / source_other as u128;
if scaled > usize::MAX as u128 { bail!("requested output size is out of range"); }

// render.rs:50 —— 上界 1.8e19 帧，要写到宇宙毁灭
if !frames.is_finite() || frames > usize::MAX as f64 { bail!("requested output frame count is out of range"); }
```

而真正可被不可信输入触发的资源耗尽（V4）没有任何上限。**建议：把这两处的「理论溢出」守卫删掉或降级为 `debug_assert`，把额度花在 `MAX_FRAMES` / `MAX_DIMENSION` 上。**

### D1（中）永不失败的 `Result`

```rust
// lib.rs:106-111
fn output_parent(output: &Path) -> Result<&Path> {
    Ok(output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(".")))
}
```

函数体只有一个 `Ok(...)`，调用处却是 `output_parent(&options.output)?`（`lib.rs:31`）。`Result` 与 `?` 都在误导读者「这里可能失败」。同理 `absolute_output_path`（`lib.rs:113-121`）确实会失败（`current_dir()`），对比之下更显突兀。

**修复**：`fn output_parent(output: &Path) -> &Path`，去掉 `?`。

### D2（中）三重校验、两套文案

| 位置 | 校验 | 失败表现 |
|---|---|---|
| `main.rs:33` | `value_parser!(u32).range(1..=240)` | clap 风格错误 + 退出码 2 |
| `options.rs:59-61` | `fps == 0 \|\| fps > 240` | `--fps must be in the range 1..=240` |
| `main.rs:45` | `value_parser!(u8).range(0..=100)` | clap 风格错误 |
| `options.rs:67-69` | `quality > 100` | `--quality must be in the range 0..=100` |
| `telegram.rs:78-80` | `threads == 0` | `--threads must be at least 1`（`options.rs:82-84` 已有一份） |

从库 API 角度，`validate()` 是必要的（`convert()` 是 public，不能假设调用方来自 clap）；但边界**常量**重复了，且同一约束有两套文案。建议常量单点定义、两边引用：

```rust
pub const FPS_RANGE: std::ops::RangeInclusive<u32> = 1..=240;
pub const QUALITY_RANGE: std::ops::RangeInclusive<u8> = 0..=100;
pub const MIN_THREADS: usize = 1;
```

clap 侧 `#[arg(long, value_parser = clap::value_parser!(u32).range(FPS_RANGE))]`，`validate()` 复用同一常量。`telegram.rs:78-80` 的 `threads == 0` 检查可以删除——`download_sticker_set` 是 public，但 `--threads` 的 clap 默认值保证非零，若要保留则应只留一处。

### D4（中）严格到把成功变成失败

```rust
// ffmpeg.rs:288-296
let correction = i128::from(expected_duration_ms) - i128::from(total_duration_ms);
if correction == 0 { return Ok(()); }
if correction.abs() > 1 {
    bail!("FFmpeg produced a WebP duration of {total_duration_ms}ms; expected {expected_duration_ms}ms");
}
```

为修掉末帧 ≤1ms 的取整差，这里手写了 110 行 RIFF 分块遍历（`ffmpeg.rs:196-306`），代价是：**一旦偏差超过 1ms，一个已经编码成功的 WebP 会被判为失败并整体报错**——用户丢掉的是可用产物，换来的是一句「时长不符」。对长动画（帧数多、逐帧毫秒取整累积），这个 1ms 阈值偏紧。

**建议**：把 `bail!` 降级为 `eprintln!("warning: WebP duration is {total}ms, expected {expected}ms; leaving as-is")` 后返回 `Ok`。同时考虑把「校验失败即失败」改成「校验失败只警告」——CLI 用户的期待是拿到文件，而不是拿到一条时序断言。

### D5（低）`expect` 与 anyhow 混用

```rust
render.rs:129   let mut slot = error.lock().expect("render error mutex poisoned");
render.rs:146   if let Some(e) = error.lock().expect("render error mutex poisoned").take()
telegram.rs:148 let mut slot = first_error.lock().expect("download error mutex poisoned");
ffmpeg.rs:121   let stdout = child.stdout.take().expect("ffmpeg stdout was configured as piped");
ffmpeg.rs:221   .expect("RIFF size is four bytes")
```

整个项目的设计是「把错误变成一行可读消息、退出码非零」（`main.rs:130-138`）。在同一批函数里混进会 panic 并打印 backtrace 的 `expect`，等于对用户开了两个出口。互斥锁中毒只在同线程已 panic 时发生，此时打印 backtrace 反而更难读。建议统一为 `context(...)?`，或者如果确信不可能，就写成 `unwrap_or_else(|e| e.into_inner())` 并去掉 panic。

### D6（低）双原子 + CAS 的进度节流

```rust
// render.rs:365-400
struct RenderProgress {
    completed: AtomicUsize,
    last_reported: AtomicUsize,
    total: usize,
    report_every: usize,
}
```

为限流一行 `eprint!` 用了两个原子量加一轮 `compare_exchange` 循环（逻辑正确，但维护成本明显高于收益）。`Mutex<usize>` 或「只在跨过整数百分点时打印」的单个原子量足够。低优先级，仅作简化建议。

### D7（低）NUL 字节全量扫描

```rust
// render.rs:59-61
if json.contains(&0) { bail!("the animation JSON contains a NUL byte"); }
```

对 200 KB 级的 JSON 做一次整段扫描，只为了在交给 rlottie 前挡掉 `CString::new` 会 panic 的情形。检查本身有理由（避免 C 侧 `expect` panic），但可以改成「扫描与解码合并」或缩短扫描长度。低优先级。

---

## 五、测试与 CI 缺口

### T1（中）CI 从不运行测试、clippy 或 fmt

`.github/workflows/ci.yml` 只有两个 job：`build-x64`（交叉编译）和 `test-x64`（Windows 上跑 4 种格式转换）。全流程**没有 `cargo test`、没有 `cargo clippy`、没有 `cargo fmt --check`**，而 `AGENTS.md` 明确要求这三条命令。`build.zig:55-58` 定义了 `zig build test`，CI 也从未调用。

后果：
- 18 个单元测试只在开发机跑（本地实测 18 passed），CI 是绿的但没测过它们；
- Windows job 只断言 `size -lt 1000`（`ci.yml:99` 等 4 处），`--fps 5` 的小样看不出时长/alpha 回归；
- `AGENTS.md` 的硬性约定无人执行。

**修复**：加一个 job：

```yaml
  lint-and-test:
    name: Lint and unit tests
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v6
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - run: brew install gcc            # build.rs 需要（rlottie-sys 的 libstdc++ 兼容名）
      - run: cargo fmt --all --check
        working-directory: tgs-convert
      - run: cargo clippy --workspace --all-targets -- -D warnings
        working-directory: tgs-convert
      - run: cargo test --workspace
        working-directory: tgs-convert
```

### T2（低）6 个 fixture 无引用

`tests/fixtures/` 下 7 个文件，只有 `sample.lottie.json` 被 `ci.yml:79` 使用：

```
tests/fixtures/AgADAQADwDZPEw.tgs   引用 0 次
tests/fixtures/AgADAgADwDZPEw.tgs   引用 0 次
tests/fixtures/AgADDQADwDZPEw.tgs   引用 0 次
tests/fixtures/AgADGAADwDZPEw.tgs   引用 0 次
tests/fixtures/AgADWgADwDZPEw.tgs   引用 0 次
tests/fixtures/Error 404.json       引用 0 次
tests/fixtures/sample.lottie.json   引用 1 次
```

约 346 KB 的仓库死重（`Error 404.json` 实际是合法的 Lottie：`{"v":"5.8.1","fr":60,"ip":0,"op":360,...}`，命名有误导性）。而且 `src/` 下**没有任何对真实 `.tgs` 的测试**——这些 fixture 正是现成的素材。

**建议**：新增集成测试 `tgs-convert/tests/cli.rs`，用它们覆盖真实输入路径（gzip 解码、非整数帧率、真实时长），并在 ffmpeg 不存在时 `eprintln!` + 跳过，避免 CI 依赖：

```rust
#[test]
fn loads_every_sample_tgs_fixture() {
    for entry in std::fs::read_dir("../tests/fixtures").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("tgs") {
            let animation = tgs_convert::render::load_animation(&path)
                .unwrap_or_else(|e| panic!("{} failed to load: {e:#}", path.display()));
            assert!(animation.metadata.width > 0 && animation.metadata.height > 0);
            assert!(animation.metadata.duration_seconds > 0.0);
        }
    }
}
```

（需要把 `render` 与 `load_animation` 从 `mod` 提升为 `pub mod`。）其余 5 个 `.tgs` 若确实不打算用，应从仓库移除而不是继续跟踪。

---

## 六、修改建议与落地顺序

### 第一批：凭据与不可信输入（安全相关，改动小、收益高）

1. **V1** 在 `telegram_api()` / `download_one()` 的错误出口脱敏 token；给 `TelegramDownloadOptions` 手写 `Debug`。*预计 30 行，无行为变化。*
2. **V2** 抽出 `sanitize_stem()` 并应用到 `unique_id`；`download_one()` 增加落点目录断言。*约 15 行。*
3. **V3** 给 `Client::builder()` 加超时；429 退避重试。*约 20 行。*
4. **V4 / D3** 定义 `MAX_FRAMES` / `MAX_DIMENSION`，在 `validate()` 与 `timeline()` 中生效；删除两处不可能的溢出守卫。*约 25 行。*

### 第二批：架构收敛（消除 C1–C5）

5. **C5** 先引入 `Timeline`，把 `play_speed` 除法收敛到一处（这是 V4 的载体，先做）。
6. **C1** 用 clap `Subcommand` + `subcommand_negates_reqs` 重写 `main.rs`，删掉 120 行重复代码与 `nth(1)` 分发。
7. **C2 + C3** 引入 `EncodePlan` / `EncodeSettings`，让 `match` 重新变成穷尽的，删掉 `unreachable!()`。
8. **C4** 给 `LoadedAnimation` 加 `new_renderer()`，把 `render_worker` 参数收敛进 `WorkerContext`，删掉 `#[allow(clippy::too_many_arguments)]`。
9. **C6 / C7** 抽出 `cancel.rs`、`transform.rs`，顺手清掉重复的父目录助手。

### 第三批：工程化收尾

10. **T1** CI 增加 lint + test job（同时让 `AGENTS.md` 的约定真正生效）。
11. **T2** 新增 `tests/cli.rs` 用掉现有 fixture；确认不用的 5 个 `.tgs` 与 `Error 404.json` 删掉或改名。
12. **D1 / D2 / D4 / D5** 去噪：`output_parent` 去掉 `Result`；边界常量单点定义；WebP 时长偏差降级为警告；统一错误风格。
13. **D6 / D7** 可选简化。

---

## 七、做得好的地方（不在改动范围）

审计中确认无需调整的部分，改动时不要破坏：

- **凭据不进二进制**：token 走 macOS Keychain / Windows PasswordVault（`telegram.rs:186-274`），比内嵌是实质性提升；平台分发用 `#[cfg]` 且非支持平台有明确报错。
- **链接解析严格**：`parse_sticker_set_name`（`telegram.rs:276-304`）限制 `addstickers`/`addemoji` 双形态、拒绝多余路径段、名字限定 `[A-Za-z0-9_]{1,64}`，默认输出目录因此不会穿越。
- **像素处理顺序正确**：`transform_frame` 在**预乘 alpha** 空间做双线性插值，最后才 `unpremultiply`（`render.rs:232-255`），避免了透明边缘出现暗边——顺序错了画质会明显劣化。
- **原子落盘**：下载先写 `.part` 再 `rename`（`telegram.rs:360-375`），中断不会留下半截文件。
- **取消语义清晰**：`AtomicBool` 贯通渲染与编码，ffmpeg 子进程被显式 kill（`ffmpeg.rs:143-156`），临时帧目录靠 `TempDir` 的 Drop 清理。
- **构建期兼容处理有据可查**：`build.rs` 的 libstdc++ 桥接、`.cargo/config.toml` 的 `-DLOTTIE_DISABLE_ARM_NEON`、`vendor/rlottie` 的补丁都写了原因注释——vendor 补丁把上游的 `as_encoded_bytes` 换回 `as_bytes`（Windows 上 Unix-only），差异仅有 2 行，可控。
- **clippy `-D warnings` 零告警**，唯一一处抑制出现在 C4，且是明确可消除的技术债。仓库卫生干净（33 个跟踪文件，无构建产物）。
