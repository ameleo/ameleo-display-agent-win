// SPDX-License-Identifier: GPL-3.0-only

//! ameleo-display-agent: Windows guest agent speaking peer-to-peer D-Bus over the
//! virtio-serial port `fr.ameleo.agent.0`.
//!
//! ```text
//! ameleo-display-agent install     register the Windows service
//! ameleo-display-agent uninstall   remove it
//! ameleo-display-agent service     service entry point (started by the SCM)
//! ameleo-display-agent session     session agent (started by the service)
//! ameleo-display-agent console     session agent in the foreground, logs on stderr
//! ameleo-display-agent camera-decoder
//!                                  H.264 decoder of the client cameras, sandboxed
//!                                  (started by the session agent)
//! ameleo-display-agent clipboard-helper
//!                                  clipboard of files, as the user (started by the
//!                                  session agent)
//! ```

#[cfg(windows)]
mod camera;
#[cfg(windows)]
mod channel;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
#[cfg(windows)]
mod consent;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod feature;
#[cfg(windows)]
mod file_clipboard;
#[cfg(windows)]
mod filexfer;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod ifaces;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod user;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "install" => service::install(),
        "uninstall" => service::uninstall(),
        "service" => {
            let _log = init_logging("service", false);
            service::run_dispatcher()
        }
        "session" => run_session("session", false),
        "console" => run_session("console", true),
        "camera-test" => camera::run_test(),
        "camera-controls" => camera::run_controls(),
        "camera-decoder" => camera::sandbox::run_child(),
        "clipboard-helper" => {
            let _log = init_logging("clipboard-helper", false);
            file_clipboard::helper::run_helper()
        }
        _ => {
            eprintln!(
                "usage: ameleo-display-agent install|uninstall|service|session|console|camera-test|camera-controls"
            );
            std::process::exit(2);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("ameleo-display-agent only runs on Windows");
    std::process::exit(1);
}

/// Sets up logging for the process `role`: to stderr, or to a file of
/// [`log_dir`]. Logs stop when the guard is dropped.
#[cfg(windows)]
fn init_logging(role: &str, stderr: bool) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_appender::non_blocking::NonBlockingBuilder;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("AMELEO_DISPLAY_AGENT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    // Logs go through a background thread and are dropped when it lags: a
    // log line must never block the agent. A console in QuickEdit mode
    // stops every write while text is selected, and the clipboard threads,
    // which other apps wait on, write logs.
    let builder = || NonBlockingBuilder::default().lossy(true).thread_name("log");
    if stderr {
        let (writer, guard) = builder().finish(std::io::stderr());
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(enable_vt_colors())
            .with_writer(writer)
            .init();
        return guard;
    }
    let path = log_dir().join(format!("ameleo-display-agent-{role}.log"));
    // The user's temp directory when ProgramData is not writable.
    let open = |p: &std::path::Path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    };
    let file = open(&path)
        .or_else(|_| open(&std::env::temp_dir().join(format!("ameleo-display-agent-{role}.log"))));
    let (writer, guard) = match file {
        Ok(file) => builder().finish(Box::new(file) as Box<dyn std::io::Write + Send>),
        Err(_) => builder().finish(Box::new(std::io::stderr()) as Box<dyn std::io::Write + Send>),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .init();
    guard
}

/// Panics go to the log: the agent's stderr goes nowhere. Written at once
/// too, the log thread may not get to it before an abort.
#[cfg(windows)]
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let line = format!("panic in thread {}: {info}", thread.name().unwrap_or("?"));
        tracing::error!("{line}");
        let path = log_dir().join("ameleo-display-agent-panic.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{:?} {line}", std::time::SystemTime::now());
        }
        default(info);
    }));
}

/// `%ProgramData%\ameleo-display-agent`: readable by administrators. The temp
/// directory of SYSTEM (`C:\Windows\SystemTemp` on Windows 11) is not.
#[cfg(windows)]
fn log_dir() -> std::path::PathBuf {
    let dir = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\ProgramData".into())
        .join("ameleo-display-agent");
    match std::fs::create_dir_all(&dir) {
        Ok(()) => dir,
        Err(_) => std::env::temp_dir(),
    }
}

/// Turns on ANSI escape handling in the console. False (no colors) when
/// stderr is not a console or the console cannot do it (old conhost).
#[cfg(windows)]
fn enable_vt_colors() -> bool {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE,
        SetConsoleMode,
    };
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        let mut mode = 0;
        GetConsoleMode(h, &mut mode) != 0
            && (mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0)
    }
}

#[cfg(windows)]
fn run_session(role: &str, stderr: bool) -> anyhow::Result<()> {
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    let _log = init_logging(role, stderr);
    log_panics();
    tracing::info!(
        "ameleo-display-agent {} starting ({role})",
        env!("CARGO_PKG_VERSION")
    );
    // Physical pixels everywhere, the pointer mapping depends on it.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    service::watch_stop_event();
    if stderr {
        service::watch_console_close();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let res = rt.block_on(session::run());
    if let Err(e) = &res {
        tracing::error!("session agent failed: {e:#}");
    }
    res
}
