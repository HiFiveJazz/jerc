//! Opt-in, content-free timing for intermittent mail-worker stalls.
//!
//! Set JERC_PERF_LOG to a file path. Never log addresses, passwords,
//! mailbox names, Message-IDs, or mail bodies from this module.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static RUN_ID: OnceLock<String> = OnceLock::new();

fn run_id() -> &'static str {
    RUN_ID.get_or_init(|| {
        format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        )
    })
}

fn output() -> Option<&'static Mutex<File>> {
    LOG.get_or_init(|| {
        let path = std::env::var_os("JERC_PERF_LOG")?;
        if path.is_empty() {
            return None;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path).ok().map(Mutex::new)
    })
    .as_ref()
}

/// The stage argument must be a fixed, non-user-derived label.
pub(crate) fn record(stage: &'static str, queued: Option<Duration>, elapsed: Duration, ok: bool) {
    let Some(output) = output() else {
        return;
    };
    let Ok(mut file) = output.lock() else {
        return;
    };
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let queue_ms = queued
        .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1000.0))
        .unwrap_or_else(|| "-".to_owned());
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("main");
    let _ = writeln!(
        file,
        "ts_ms={epoch_ms} run_id={} worker={thread_name} stage={stage} queue_ms={queue_ms} duration_ms={:.3} ok={ok}",
        run_id(),
        elapsed.as_secs_f64() * 1000.0,
    );
}

/// Correlate UI enqueue/commit and worker execution without logging mailbox
/// names, subjects, message IDs, or other private data.
pub(crate) fn record_request(
    stage: &'static str,
    request_id: u64,
    queued: Option<Duration>,
    elapsed: Duration,
    ok: bool,
) {
    let Some(output) = output() else {
        return;
    };
    let Ok(mut file) = output.lock() else {
        return;
    };
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let queue_ms = queued
        .map(|d| format!("{:.3}", d.as_secs_f64() * 1000.0))
        .unwrap_or_else(|| "-".to_owned());
    let current = std::thread::current();
    let worker = current.name().unwrap_or("main");
    let _ = writeln!(
        file,
        "ts_ms={ts_ms} run_id={} worker={worker} stage={stage} request_id={request_id} queue_ms={queue_ms} duration_ms={:.3} ok={ok}",
        run_id(),
        elapsed.as_secs_f64() * 1000.0,
    );
}
