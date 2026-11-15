// SPDX-License-Identifier: GPL-3.0-only

//! Windows service: keeps one session agent running as SYSTEM inside the
//! active console session (vdservice equivalent).
//!
//! The session agent must live in the user session to reach its desktop,
//! clipboard and input, and must be SYSTEM to follow the input desktop onto
//! the logon and UAC secure desktops.

use std::sync::OnceLock;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSGetActiveConsoleSessionId, WTSSESSION_NOTIFICATION,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, RegisterServiceCtrlHandlerExW, SC_MANAGER_ALL_ACCESS,
    SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, CreateProcessAsUserW, GetCurrentProcess, OpenEventW,
    OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows_sys::core::PWSTR;

use crate::win::{Event, OwnedHandle, wide};

pub const SERVICE_NAME: &str = "ameleo-display-agent";
const DISPLAY_NAME: &str = "Ameleo Display Agent";
const DESCRIPTION: &str = "Guest agent for QEMU D-Bus displays.";
/// Signaled by the service to stop the session agent.
pub const STOP_EVENT: &str = r"Global\ameleo-display-agent-stop";

/// The session agent removes what it added to the guest before exiting.
const AGENT_STOP_TIMEOUT_MS: u32 = 10_000;
const RESTART_DELAY: Duration = Duration::from_secs(3);
const MAX_RESTARTS: u32 = 10;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const WTS_CONSOLE_CONNECT: u32 = 1;
const NO_SESSION: u32 = 0xFFFF_FFFF;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;

enum Control {
    Stop,
    ConsoleConnect(u32),
}

struct Globals {
    status: SERVICE_STATUS_HANDLE,
    tx: std_mpsc::Sender<Control>,
    wake: Event,
}

// The SCM callbacks are plain functions.
unsafe impl Send for Globals {}
unsafe impl Sync for Globals {}

static GLOBALS: OnceLock<Globals> = OnceLock::new();
static CONTROL_RX: OnceLock<std::sync::Mutex<Option<std_mpsc::Receiver<Control>>>> =
    OnceLock::new();

/// Entry point when started by the SCM.
pub fn run_dispatcher() -> Result<()> {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        bail!(
            "StartServiceCtrlDispatcher failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let Some(g) = GLOBALS.get() else { return };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOP_PENDING {
            AGENT_STOP_TIMEOUT_MS + 1000
        } else {
            0
        },
    };
    unsafe { SetServiceStatus(g.status, &status) };
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let (tx, rx) = std_mpsc::channel();
    let name = wide(SERVICE_NAME);
    let status = unsafe {
        RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), std::ptr::null())
    };
    if status.is_null() {
        tracing::error!("RegisterServiceCtrlHandlerEx failed");
        return;
    }
    let Ok(wake) = Event::new(false) else { return };
    let _ = GLOBALS.set(Globals { status, tx, wake });
    let _ = CONTROL_RX.set(std::sync::Mutex::new(Some(rx)));
    set_state(SERVICE_RUNNING);
    tracing::info!("service started");

    let rx = CONTROL_RX.get().unwrap().lock().unwrap().take().unwrap();
    if let Err(e) = supervise(&rx) {
        tracing::error!("service failed: {e:#}");
    }
    set_state(SERVICE_STOPPED);
    tracing::info!("service stopped");
}

unsafe extern "system" fn control_handler(
    control: u32,
    event_type: u32,
    event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    let Some(g) = GLOBALS.get() else { return 0 };
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_state(SERVICE_STOP_PENDING);
            let _ = g.tx.send(Control::Stop);
            g.wake.set();
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            if event_data.is_null() {
                return 0;
            }
            let id = unsafe { &*(event_data as *const WTSSESSION_NOTIFICATION) }.dwSessionId;
            if event_type == WTS_CONSOLE_CONNECT {
                let _ = g.tx.send(Control::ConsoleConnect(id));
                g.wake.set();
            }
        }
        SERVICE_CONTROL_INTERROGATE => {}
        _ => return 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
    0
}

struct Supervisor {
    stop_event: OwnedHandle,
    agent: Option<OwnedHandle>,
    session: u32,
    restarts: u32,
    window_start: Instant,
    /// No agent running: when to try again. The wait never sleeps, so stop
    /// and console changes are handled at once.
    next_launch: Instant,
}

fn supervise(rx: &std_mpsc::Receiver<Control>) -> Result<()> {
    let wake = &GLOBALS.get().unwrap().wake;
    let name = wide(STOP_EVENT);
    let stop_event =
        OwnedHandle::new(unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) })
            .context("creating the agent stop event")?;
    let mut s = Supervisor {
        stop_event,
        agent: None,
        session: NO_SESSION,
        restarts: 0,
        window_start: Instant::now(),
        next_launch: Instant::now(),
    };
    s.launch();

    loop {
        let mut handles: Vec<HANDLE> = vec![wake.raw()];
        if let Some(a) = &s.agent {
            handles.push(a.raw());
        }
        let timeout = if s.agent.is_some() {
            u32::MAX
        } else {
            s.next_launch
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(u32::MAX as u128 - 1) as u32
        };
        let r =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
        if r == WAIT_OBJECT_0 {
            for c in rx.try_iter() {
                match c {
                    Control::Stop => {
                        s.stop_agent();
                        return Ok(());
                    }
                    Control::ConsoleConnect(id) => {
                        tracing::info!("console connected to session {id}");
                        s.stop_agent();
                        s.restarts = 0;
                        s.launch();
                    }
                }
            }
        } else if r == WAIT_OBJECT_0 + 1 {
            tracing::warn!("session agent exited");
            s.agent = None;
            if s.window_start.elapsed() > RESTART_WINDOW {
                s.window_start = Instant::now();
                s.restarts = 0;
            }
            s.restarts += 1;
            let delay = if s.restarts > MAX_RESTARTS {
                tracing::error!("session agent keeps crashing, pausing restarts");
                s.restarts = 0;
                s.window_start = Instant::now();
                RESTART_WINDOW
            } else {
                RESTART_DELAY
            };
            s.next_launch = Instant::now() + delay;
        } else if r == WAIT_TIMEOUT {
            s.launch();
        }
    }
}

impl Supervisor {
    /// Starts the agent in the session attached to the console now. On
    /// failure (no console session yet, session going away) it is retried
    /// after `RESTART_DELAY`.
    fn launch(&mut self) {
        if self.agent.is_some() {
            return;
        }
        self.next_launch = Instant::now() + RESTART_DELAY;
        self.session = unsafe { WTSGetActiveConsoleSessionId() };
        if self.session == NO_SESSION {
            return; // no console session yet
        }
        unsafe { windows_sys::Win32::System::Threading::ResetEvent(self.stop_event.raw()) };
        match launch_in_session(self.session) {
            Ok(h) => {
                tracing::info!("session agent started in session {}", self.session);
                self.agent = Some(h);
            }
            Err(e) => tracing::warn!(
                "cannot start the session agent in session {}: {e}",
                self.session
            ),
        }
    }

    fn stop_agent(&mut self) {
        let Some(agent) = self.agent.take() else {
            return;
        };
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.stop_event.raw());
            if WaitForSingleObject(agent.raw(), AGENT_STOP_TIMEOUT_MS) != WAIT_OBJECT_0 {
                tracing::warn!("session agent did not stop, terminating it");
                TerminateProcess(agent.raw(), 1);
            }
        }
    }
}

/// Starts `<exe> session` as SYSTEM in `session`.
fn launch_in_session(session: u32) -> std::io::Result<OwnedHandle> {
    unsafe {
        let mut own: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let own = OwnedHandle(own);
        let mut dup: HANDLE = std::ptr::null_mut();
        if DuplicateTokenEx(
            own.raw(),
            MAXIMUM_ALLOWED,
            std::ptr::null(),
            SecurityIdentification,
            TokenPrimary,
            &mut dup,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let dup = OwnedHandle(dup);
        if SetTokenInformation(
            dup.raw(),
            TokenSessionId,
            (&session as *const u32).cast(),
            4,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let exe = std::env::current_exe()?;
        let app = wide(&exe);
        let mut cmd = wide(format!("\"{}\" session", exe.display()));
        let mut desktop = wide(r"winsta0\default");
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessAsUserW(
            dup.raw(),
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        drop(OwnedHandle(pi.hThread));
        Ok(OwnedHandle(pi.hProcess))
    }
}

/// In the session agent: exit when the service asks.
pub fn watch_stop_event() {
    let name = wide(STOP_EVENT);
    let h = unsafe { OpenEventW(SYNCHRONIZE, 0, name.as_ptr()) };
    if h.is_null() {
        return; // not started by the service
    }
    let h = OwnedHandle(h);
    std::thread::spawn(move || {
        unsafe { WaitForSingleObject(h.raw(), u32::MAX) };
        tracing::info!("stop requested by the service");
        stop_now();
    });
}

/// Cleanup run before the session agent exits (what the clients added to
/// the guest).
static ON_STOP: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub fn on_stop(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_STOP.set(Box::new(f));
}

/// Runs the cleanup once and exits.
fn stop_now() -> ! {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(f) = ON_STOP.get() {
            f();
        }
    });
    std::process::exit(0);
}

/// Console mode: Ctrl+C / closing the window cleans up like a service stop.
pub fn watch_console_close() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handler(_: u32) -> i32 {
        tracing::info!("console closed");
        stop_now()
    }
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}

pub fn install() -> Result<()> {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if scm.is_null() {
            bail!(
                "OpenSCManager failed (run as administrator): {}",
                std::io::Error::last_os_error()
            );
        }
        let exe = std::env::current_exe()?;
        let bin = wide(format!("\"{}\" service", exe.display()));
        let name = wide(SERVICE_NAME);
        let display = wide(DISPLAY_NAME);
        let svc = CreateServiceW(
            scm,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            bin.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        );
        if svc.is_null() {
            let e = std::io::Error::last_os_error();
            CloseServiceHandle(scm);
            // Installed already: `install` again updates the rest (camera
            // source).
            if e.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_SERVICE_EXISTS as i32)
            {
                bail!("CreateService failed: {e}");
            }
            println!("service {SERVICE_NAME} already installed");
        } else {
            let mut desc = wide(DESCRIPTION);
            let info = SERVICE_DESCRIPTIONW {
                lpDescription: desc.as_mut_ptr(),
            };
            ChangeServiceConfig2W(
                svc,
                SERVICE_CONFIG_DESCRIPTION,
                (&info as *const SERVICE_DESCRIPTIONW).cast(),
            );
            CloseServiceHandle(svc);
            CloseServiceHandle(scm);
        }
    }
    crate::camera::install();
    println!("service {SERVICE_NAME} installed, start it with: sc.exe start {SERVICE_NAME}");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if scm.is_null() {
            bail!(
                "OpenSCManager failed (run as administrator): {}",
                std::io::Error::last_os_error()
            );
        }
        let name = wide(SERVICE_NAME);
        let svc = OpenServiceW(scm, name.as_ptr(), SERVICE_ALL_ACCESS);
        if svc.is_null() {
            let e = std::io::Error::last_os_error();
            CloseServiceHandle(scm);
            bail!("OpenService failed: {e}");
        }
        let ok = DeleteService(svc);
        let e = std::io::Error::last_os_error();
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        if ok == 0 {
            bail!("DeleteService failed: {e}");
        }
    }
    crate::camera::uninstall();
    println!("service {SERVICE_NAME} removed (stop it first if it is running)");
    Ok(())
}
