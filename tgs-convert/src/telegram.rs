use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::Deserialize;

const TOKEN_SERVICE: &str = "TGSConvert";
const TOKEN_ACCOUNT: &str = "tgs-convert";
const API_BASE: &str = "https://api.telegram.org";

/// Stands in for a bot token in any message that could reach a log.
const REDACTED: &str = "<redacted>";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RATE_LIMIT_RETRIES: u32 = 2;
const MAX_RETRY_AFTER_SECONDS: u64 = 60;
const MAX_FILE_STEM_LENGTH: usize = 64;

#[derive(Clone)]
pub struct TelegramDownloadOptions {
    pub link_or_name: String,
    pub output_directory: PathBuf,
    pub threads: usize,
    pub token: String,
}

impl std::fmt::Debug for TelegramDownloadOptions {
    /// Hand-written so the bot token can never be printed by an accidental
    /// `{:?}` on the options struct.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TelegramDownloadOptions")
            .field("link_or_name", &self.link_or_name)
            .field("output_directory", &self.output_directory)
            .field("threads", &self.threads)
            .field("token", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct TelegramDownloadReport {
    pub set_name: String,
    pub title: String,
    pub files: usize,
    pub output_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StickerSet {
    name: String,
    title: String,
    #[serde(default)]
    sticker_type: String,
    stickers: Vec<Sticker>,
}

#[derive(Clone, Debug, Deserialize)]
struct Sticker {
    file_id: String,
    file_unique_id: String,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    is_animated: bool,
    #[serde(default)]
    is_video: bool,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    file_path: String,
}

#[derive(Clone, Debug)]
struct DownloadItem {
    file_id: String,
    unique_id: String,
    emoji: Option<String>,
    is_animated: bool,
    is_video: bool,
}

pub fn download_sticker_set(options: &TelegramDownloadOptions) -> Result<TelegramDownloadReport> {
    if options.threads == 0 {
        bail!("--threads must be at least 1");
    }

    let requested_name = parse_sticker_set_name(&options.link_or_name)?;
    fs::create_dir_all(&options.output_directory).with_context(|| {
        format!(
            "failed to create output directory {}",
            options.output_directory.display()
        )
    })?;

    // Installed before the first request so Ctrl-C also interrupts metadata
    // fetches and the rate-limit back-off.
    let cancel = Arc::new(AtomicBool::new(false));
    install_cancel_handler(Arc::clone(&cancel))?;

    let client = Client::builder()
        .user_agent("tgs-convert Telegram sticker downloader")
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to create Telegram HTTP client")?;
    let set = get_sticker_set(&client, &options.token, &requested_name, &cancel)?;
    if set.stickers.is_empty() {
        bail!("Telegram sticker set {} is empty", set.name);
    }

    let include_emoji = set.sticker_type == "custom_emoji";
    let items = set
        .stickers
        .into_iter()
        .map(|sticker| DownloadItem {
            file_id: sticker.file_id,
            unique_id: sticker.file_unique_id,
            emoji: include_emoji.then_some(sticker.emoji).flatten(),
            is_animated: sticker.is_animated,
            is_video: sticker.is_video,
        })
        .collect::<Vec<_>>();
    let file_count = items.len();
    let workers = options.threads.min(file_count);
    let next_index = AtomicUsize::new(0);
    let completed = AtomicUsize::new(0);
    let first_error = Mutex::new(None::<String>);

    eprintln!(
        "Downloading {} sticker(s) from {} with {} worker(s)",
        file_count, set.name, workers
    );

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let client = client.clone();
            let output_directory = &options.output_directory;
            let token = &options.token;
            let items = &items;
            let next_index = &next_index;
            let completed = &completed;
            let cancel = Arc::clone(&cancel);
            let first_error = &first_error;
            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let index = next_index.fetch_add(1, Ordering::AcqRel);
                    if index >= items.len() {
                        break;
                    }

                    if let Err(error) =
                        download_one(&client, token, &items[index], output_directory, &cancel)
                    {
                        cancel.store(true, Ordering::Release);
                        let mut slot = first_error.lock().expect("download error mutex poisoned");
                        if slot.is_none() {
                            *slot = Some(format!("sticker {}: {error:#}", index + 1));
                        }
                        break;
                    }

                    let done = completed.fetch_add(1, Ordering::AcqRel) + 1;
                    eprintln!("Downloaded {done}/{file_count}");
                }
            });
        }
    });

    if let Some(error) = first_error
        .lock()
        .expect("download error mutex poisoned")
        .take()
    {
        return Err(anyhow!(error));
    }
    if cancel.load(Ordering::Acquire) {
        bail!("download cancelled");
    }

    Ok(TelegramDownloadReport {
        set_name: set.name,
        title: set.title,
        files: file_count,
        output_directory: options.output_directory.clone(),
    })
}

/// Resolves the Telegram bot token used by sticker downloads.
///
/// Priority: an explicit `--token` passed for a single run, then the OS
/// credential store (macOS Keychain, Windows PasswordVault). The token is never
/// embedded in the binary.
pub fn resolve_bot_token(cli_token: Option<&str>) -> Result<String> {
    if let Some(token) = cli_token {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
    }

    #[cfg(target_os = "macos")]
    {
        macos_keychain_token()
    }
    #[cfg(target_os = "windows")]
    {
        windows_password_vault_token()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        bail!("no --token provided and this platform has no supported credential store");
    }
}

#[cfg(target_os = "macos")]
fn macos_keychain_token() -> Result<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            TOKEN_SERVICE,
            "-a",
            TOKEN_ACCOUNT,
            "-w",
        ])
        .output()
        .context("failed to run security(1); the macOS Keychain is required")?;
    if !output.status.success() {
        bail!(
            "Telegram bot token not found in macOS Keychain (service {TOKEN_SERVICE}, account {TOKEN_ACCOUNT}); \
             store it with: security add-generic-password -s {TOKEN_SERVICE} -a {TOKEN_ACCOUNT} -w <token>, \
             or pass --token for a single run"
        );
    }
    let token =
        String::from_utf8(output.stdout).context("macOS Keychain returned a non-UTF8 token")?;
    let token = token.trim();
    if token.is_empty() {
        bail!("Telegram bot token in macOS Keychain is empty");
    }
    Ok(token.to_owned())
}

#[cfg(target_os = "windows")]
fn windows_password_vault_token() -> Result<String> {
    use windows::{Security::Credentials::PasswordVault, core::HSTRING};

    let vault = PasswordVault::new().context("failed to open Windows PasswordVault")?;
    let resource = HSTRING::from(TOKEN_SERVICE);
    let credentials = vault
        .FindAllByResource(&resource)
        .context("failed to query Windows PasswordVault")?;
    let iterator = credentials
        .First()
        .context("failed to enumerate Windows PasswordVault")?;
    while iterator
        .HasCurrent()
        .context("failed to enumerate Windows PasswordVault")?
    {
        let credential = iterator
            .Current()
            .context("failed to read Windows PasswordVault credential")?;
        credential
            .RetrievePassword()
            .context("failed to retrieve password from Windows PasswordVault credential")?;
        let password = credential
            .Password()
            .context("failed to read password from Windows PasswordVault credential")?
            .to_string();
        if !password.is_empty() {
            return Ok(password);
        }
        iterator
            .MoveNext()
            .context("failed to enumerate Windows PasswordVault")?;
    }
    bail!(
        "Telegram bot token not found in Windows Credential Locker (PasswordVault resource {TOKEN_SERVICE}, \
         user {TOKEN_ACCOUNT}); store it with PowerShell or pass --token for a single run"
    );
}

pub fn parse_sticker_set_name(link_or_name: &str) -> Result<String> {
    let trimmed = link_or_name.trim();
    let candidate = if let Some((_, rest)) = trimmed.split_once("://") {
        let without_query = rest.split(['?', '#']).next().unwrap_or_default();
        let mut parts = without_query.split('/').filter(|part| !part.is_empty());
        let host = parts.next().unwrap_or_default().to_ascii_lowercase();
        let kind = parts.next().unwrap_or_default().to_ascii_lowercase();
        let name = parts.next().unwrap_or_default();
        if (host != "t.me" && host != "www.t.me") || (kind != "addstickers" && kind != "addemoji") {
            bail!("Telegram link must use t.me/addstickers/<name> or t.me/addemoji/<name>");
        }
        if parts.next().is_some() || name.is_empty() {
            bail!("Telegram link must contain exactly one sticker-set name");
        }
        name
    } else {
        trimmed
    };

    if candidate.is_empty()
        || candidate.len() > 64
        || !candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("invalid Telegram sticker-set name");
    }
    Ok(candidate.to_owned())
}

fn get_sticker_set(
    client: &Client,
    token: &str,
    name: &str,
    cancel: &AtomicBool,
) -> Result<StickerSet> {
    telegram_api(client, token, "getStickerSet", &[("name", name)], cancel)
        .with_context(|| format!("failed to fetch Telegram sticker set {name}"))
}

fn get_file(
    client: &Client,
    token: &str,
    file_id: &str,
    cancel: &AtomicBool,
) -> Result<TelegramFile> {
    telegram_api(client, token, "getFile", &[("file_id", file_id)], cancel)
        .context("failed to fetch Telegram file metadata")
}

fn telegram_api<T>(
    client: &Client,
    token: &str,
    method: &str,
    query: &[(&str, &str)],
    cancel: &AtomicBool,
) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let url = format!("{API_BASE}/bot{token}/{method}");
    let response = send_with_retry(client, &url, query, token, method, cancel)?
        .error_for_status()
        .map_err(|error| {
            anyhow!(
                "Telegram API request {method} returned an HTTP error: {}",
                redacted_error(&error, token)
            )
        })?;
    let response = response.json::<ApiResponse<T>>().map_err(|error| {
        anyhow!(
            "Telegram API request {method} returned invalid JSON: {}",
            redacted_error(&error, token)
        )
    })?;
    if !response.ok {
        bail!(
            "Telegram API request {method} was rejected: {}",
            response
                .description
                .unwrap_or_else(|| "no description".to_owned())
        );
    }
    response
        .result
        .ok_or_else(|| anyhow!("Telegram API request {method} returned no result"))
}

/// Sends one GET request, retrying a bounded number of times when Telegram
/// reports rate limiting.
fn send_with_retry(
    client: &Client,
    url: &str,
    query: &[(&str, &str)],
    token: &str,
    method: &str,
    cancel: &AtomicBool,
) -> Result<reqwest::blocking::Response> {
    let mut attempt = 0_u32;
    loop {
        let response = client.get(url).query(query).send().map_err(|error| {
            anyhow!(
                "Telegram request {method} failed: {}",
                redacted_error(&error, token)
            )
        })?;

        if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(response);
        }

        let retry_after = retry_after_seconds(&response);
        if attempt >= MAX_RATE_LIMIT_RETRIES {
            bail!(
                "Telegram rate limited {method}; gave up after {MAX_RATE_LIMIT_RETRIES} retries \
                 (the server asked to wait {retry_after}s)"
            );
        }
        attempt += 1;
        eprintln!(
            "Telegram rate limited {method}; retrying in {retry_after}s \
             ({attempt}/{MAX_RATE_LIMIT_RETRIES})"
        );
        if !sleep_until_cancelled(Duration::from_secs(retry_after), cancel) {
            bail!("download cancelled");
        }
    }
}

fn retry_after_seconds(response: &reqwest::blocking::Response) -> u64 {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .min(MAX_RETRY_AFTER_SECONDS)
}

/// Sleeps in short slices so Ctrl-C is honoured during the back-off.
///
/// Returns `false` when the download was cancelled.
fn sleep_until_cancelled(duration: Duration, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

/// Removes the bot token from a message before it can reach a log.
///
/// `reqwest` appends the request URL to its error messages, and the token is
/// part of that URL's path, so **every** error derived from a request has to
/// pass through here — including the ones only reachable through `{:#}`.
fn sanitize_token(message: &str, token: &str) -> String {
    if token.is_empty() {
        return message.to_owned();
    }
    message.replace(token, REDACTED)
}

/// Renders `error` and its whole source chain the way anyhow's `{:#}` would,
/// with the token removed at every level.
fn redacted_error(error: &dyn std::error::Error, token: &str) -> String {
    let mut message = sanitize_token(&error.to_string(), token);
    let mut source = error.source();
    while let Some(current) = source {
        message.push_str(": ");
        message.push_str(&sanitize_token(&current.to_string(), token));
        source = current.source();
    }
    message
}

fn download_one(
    client: &Client,
    token: &str,
    item: &DownloadItem,
    output_directory: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    let metadata = get_file(client, token, &item.file_id, cancel)?;
    let extension =
        extension_from_path(&metadata.file_path).unwrap_or_else(|| fallback_extension(item));
    let filename = filename(item, extension);
    let destination = output_directory.join(filename);
    if destination.parent() != Some(output_directory) {
        bail!(
            "refusing to write outside the output directory: {}",
            destination.display()
        );
    }
    let partial_destination = destination.with_extension(format!("{extension}.part"));
    let url = format!("{API_BASE}/file/bot{token}/{}", metadata.file_path);

    let mut response = send_with_retry(client, &url, &[], token, "file download", cancel)?
        .error_for_status()
        .map_err(|error| {
            anyhow!(
                "Telegram file download returned an HTTP error: {}",
                redacted_error(&error, token)
            )
        })?;
    let mut partial = File::create(&partial_destination)
        .with_context(|| format!("failed to create {}", partial_destination.display()))?;
    io::copy(&mut response, &mut partial)
        .with_context(|| format!("failed to write {}", partial_destination.display()))?;
    drop(partial);
    fs::rename(&partial_destination, &destination)
        .with_context(|| format!("failed to finalize {}", destination.display()))?;
    Ok(())
}

fn extension_from_path(file_path: &str) -> Option<&str> {
    let extension = Path::new(file_path).extension()?.to_str()?;
    (extension.len() <= 10 && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then_some(extension)
}

fn fallback_extension(item: &DownloadItem) -> &'static str {
    if item.is_animated {
        "tgs"
    } else if item.is_video {
        "webm"
    } else {
        "webp"
    }
}

fn filename(item: &DownloadItem, extension: &str) -> String {
    let emoji = item.emoji.as_deref().map(clean_emoji).unwrap_or_default();
    let unique = sanitize_file_stem(&item.unique_id);
    let stem = if emoji.is_empty() {
        unique
    } else {
        format!("{emoji}_{unique}")
    };
    format!("{stem}.{extension}")
}

/// Telegram returns `file_unique_id` as an opaque string. Only characters that
/// are safe inside a file name are kept, so a hostile response cannot escape
/// the output directory with a `..` segment or an absolute path.
fn sanitize_file_stem(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(MAX_FILE_STEM_LENGTH)
        .collect();
    if cleaned.is_empty() {
        "sticker".to_owned()
    } else {
        cleaned
    }
}

fn clean_emoji(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            *character != '\u{fe0f}'
                && !character.is_control()
                && !matches!(
                    *character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
        .collect()
}

fn install_cancel_handler(cancel: Arc<AtomicBool>) -> Result<()> {
    ctrlc::set_handler(move || {
        cancel.store(true, Ordering::Release);
        eprintln!("\nCancellation requested; stopping active downloads.");
    })
    .map_err(|error| anyhow!("failed to install Ctrl-C handler: {error}"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        DownloadItem, TelegramDownloadOptions, clean_emoji, filename, parse_sticker_set_name,
        redacted_error, sanitize_file_stem, sanitize_token,
    };

    const SAMPLE_TOKEN: &str = "7654321:AAHfakeTokenValueDoNotUse";

    #[test]
    fn parses_sticker_and_emoji_links() {
        assert_eq!(
            parse_sticker_set_name("https://t.me/addstickers/HotCherry?startapp=x").unwrap(),
            "HotCherry"
        );
        assert_eq!(
            parse_sticker_set_name("https://t.me/addemoji/Custom_Emoji/").unwrap(),
            "Custom_Emoji"
        );
    }

    #[test]
    fn rejects_non_pack_links_and_unsafe_names() {
        assert!(parse_sticker_set_name("https://t.me/example").is_err());
        assert!(parse_sticker_set_name("../not-a-pack").is_err());
    }

    #[test]
    fn custom_emoji_filename_matches_desktop_pattern() {
        let item = DownloadItem {
            file_id: "unused".to_owned(),
            unique_id: "unique".to_owned(),
            emoji: Some("😀\u{fe0f}".to_owned()),
            is_animated: false,
            is_video: false,
        };
        assert_eq!(filename(&item, "tgs"), "😀_unique.tgs");
        assert_eq!(clean_emoji("a/b"), "ab");
    }

    #[test]
    fn unique_id_cannot_escape_the_output_directory() {
        let item = DownloadItem {
            file_id: "unused".to_owned(),
            unique_id: "../../etc/passwd".to_owned(),
            emoji: None,
            is_animated: true,
            is_video: false,
        };
        let name = filename(&item, "tgs");
        assert_eq!(name, "etcpasswd.tgs");
        assert_eq!(Path::new(&name).components().count(), 1);
    }

    #[test]
    fn sanitize_file_stem_keeps_the_telegram_alphabet() {
        assert_eq!(sanitize_file_stem("AgADGAADwDZPEw"), "AgADGAADwDZPEw");
        assert_eq!(sanitize_file_stem("a-b_c"), "a-b_c");
        assert_eq!(sanitize_file_stem(""), "sticker");
        assert_eq!(sanitize_file_stem("../.."), "sticker");
        assert_eq!(sanitize_file_stem("/etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_file_stem(&"a".repeat(200)).len(), 64);
    }

    #[test]
    fn bot_tokens_are_removed_from_error_text() {
        let message = format!(
            "error sending request for url (https://api.telegram.org/bot{SAMPLE_TOKEN}/getMe)"
        );
        let sanitized = sanitize_token(&message, SAMPLE_TOKEN);
        assert!(!sanitized.contains(SAMPLE_TOKEN));
        assert!(sanitized.contains("<redacted>"));
    }

    #[test]
    fn redacted_error_keeps_the_cause_chain_without_the_token() {
        let deepest = ChainError::new("tls handshake eof".to_owned(), None);
        let middle = ChainError::new("client error (Connect)".to_owned(), Some(deepest));
        let top = ChainError::new(
            format!(
                "error sending request for url (https://api.telegram.org/bot{SAMPLE_TOKEN}/getMe)"
            ),
            Some(middle),
        );

        let rendered = redacted_error(&top, SAMPLE_TOKEN);
        assert!(!rendered.contains(SAMPLE_TOKEN));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("client error (Connect)"));
        assert!(rendered.contains("tls handshake eof"));
    }

    #[test]
    fn options_debug_redacts_the_token() {
        let options = TelegramDownloadOptions {
            link_or_name: "HotCherry".to_owned(),
            output_directory: PathBuf::from("out"),
            threads: 2,
            token: SAMPLE_TOKEN.to_owned(),
        };
        let rendered = format!("{options:?}");
        assert!(!rendered.contains(SAMPLE_TOKEN));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("HotCherry"));
    }

    #[derive(Debug)]
    struct ChainError {
        message: String,
        source: Option<Box<ChainError>>,
    }

    impl ChainError {
        fn new(message: String, source: Option<Self>) -> Self {
            Self {
                message,
                source: source.map(Box::new),
            }
        }
    }

    impl std::fmt::Display for ChainError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.message)
        }
    }

    impl std::error::Error for ChainError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match &self.source {
                Some(inner) => Some(inner.as_ref()),
                None => None,
            }
        }
    }
}
