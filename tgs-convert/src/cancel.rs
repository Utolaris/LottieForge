use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, anyhow};

/// Installs the Ctrl-C handler and returns the flag it sets.
///
/// `ctrlc` accepts a single handler per process, so this is shared by the
/// conversion and the Telegram download paths.
pub fn install(message: &'static str) -> Result<Arc<AtomicBool>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::Release);
        eprintln!("\n{message}");
    })
    .map_err(|error| anyhow!("failed to install Ctrl-C handler: {error}"))?;
    Ok(cancel)
}
