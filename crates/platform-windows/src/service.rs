//! Windows service (LocalSystem, session 0) supervising the server in the
//! active console session.
//!
//! Session 0 has no access to the interactive desktop, so the service never
//! captures by itself: it launches `remothing --config <path>` as SYSTEM inside
//! the active console session (desktop `winsta0\default`) and relaunches it
//! whenever the console moves to another session or the child exits. Running
//! the child as SYSTEM is what lets it follow the secure desktop (logon screen,
//! UAC) and call SendSAS.

use std::cell::Cell;
use std::ffi::{c_void, OsStr};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{s, w, Owned, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    SetHandleInformation, ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED,
    ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_SPECIFIC_ERROR, HANDLE, HANDLE_FLAG_INHERIT, NO_ERROR,
    WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, IsWellKnownSid, SecurityImpersonation,
    SetTokenInformation, TokenPrimary, TokenSessionId, TokenUser, WinLocalSystemSid,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE,
    REG_DWORD, REG_OPTION_NON_VOLATILE, RRF_RT_REG_DWORD,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW,
    SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW, SC_ACTION, SC_ACTION_RESTART,
    SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
    SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOP, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcess,
    OpenProcessToken, QueryFullProcessImageNameW, SetEvent, TerminateProcess,
    WaitForMultipleObjects, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

const SERVICE_NAME: PCWSTR = w!("remothing");
const DISPLAY_NAME: PCWSTR = w!("remothing desktop streaming");
const DESCRIPTION: PCWSTR = w!(
    "Streams the interactive desktop to remothing browser clients. Runs the remothing server \
     inside the active console session and follows session switches."
);
const POLICY_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
const SAS_VALUE: PCWSTR = w!("SoftwareSASGeneration");

const SERVICE_LOG: &str = "remothing-service.log";
const SESSION_LOG: &str = "remothing-session.log";
const SESSION_LOG_PREV: &str = "remothing-session.log.1";
const PID_FILE: &str = "remothing-session.pid";
/// The service log is appended across restarts; beyond this it starts over.
const SERVICE_LOG_MAX: u64 = 1 << 20;

/// Session ids that never host an interactive console (0 is the services
/// session, u32::MAX means "console is between sessions").
const NO_SESSION: [u32; 2] = [0, u32::MAX];
/// Session notifications arrive in bursts (disconnect, connect, logon...);
/// act once the burst is quiet.
const DEBOUNCE_MS: u32 = 500;
/// Safety net in case a session notification is missed.
const POLL_MS: u32 = 2000;
/// A child that ran at least this long is not part of a crash loop.
const STABLE_RUN: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const CHILD_EXIT_WAIT_MS: u32 = 5000;
const SCM_WAIT: Duration = Duration::from_secs(30);

/// `remothing service <sub>`: install | uninstall | start | stop | run.
pub fn command(sub: &str, config: &Path) -> Result<()> {
    match sub {
        "install" => install(config),
        "uninstall" => uninstall(),
        "start" => start(),
        "stop" => stop(),
        "run" => run(config),
        other => bail!("unknown service command {other:?} (install|uninstall|start|stop|run)"),
    }
}

/// Secure attention sequence (Ctrl+Alt+Del) through sas.dll.
///
/// Windows only honours SendSAS from a service (or LocalSystem) process and only
/// when the SoftwareSASGeneration policy allows services; otherwise the call is
/// silently ignored, so both conditions are checked up front to report a real
/// error instead of a no-op.
pub fn send_sas() -> Result<()> {
    type SendSasFn = unsafe extern "system" fn(BOOL);
    // The module stays loaded for the process lifetime because the cached
    // pointer refers into it.
    static SEND_SAS: LazyLock<Result<SendSasFn, String>> = LazyLock::new(|| {
        // SAFETY: plain library load of a system DLL.
        let module = unsafe { LoadLibraryW(w!("sas.dll")) }.map_err(|e| format!("load sas.dll: {e}"))?;
        // SAFETY: valid module handle and NUL-terminated name.
        let proc = unsafe { GetProcAddress(module, s!("SendSAS")) }.ok_or("sas.dll has no SendSAS export")?;
        // SAFETY: SendSAS is `VOID WINAPI SendSAS(BOOL AsUser)`.
        Ok(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, SendSasFn>(proc) })
    });

    if !is_local_system()? {
        bail!(
            "Ctrl+Alt+Del needs the server to run as LocalSystem (under the remothing service); \
             a user process cannot generate the secure attention sequence"
        );
    }
    let policy = sas_policy();
    // 1 = services, 3 = services and ease-of-access applications.
    if !matches!(policy, Some(1 | 3)) {
        bail!(
            "SoftwareSASGeneration policy does not allow services to send Ctrl+Alt+Del \
             (value {policy:?}); run `remothing service install` as administrator to enable it"
        );
    }
    let send = (*SEND_SAS).clone().map_err(|e| anyhow!(e))?;
    // SAFETY: FALSE = the caller is a service, not a uiAccess user application.
    unsafe { send(BOOL::from(false)) };
    Ok(())
}

// ---------------------------------------------------------------------------
// SCM management commands (admin)

fn install(config: &Path) -> Result<()> {
    let config = std::path::absolute(config).context("resolve config path")?;
    let exe = std::env::current_exe().context("locate current executable")?;
    let binary = command_line(&[
        exe.as_os_str(),
        OsStr::new("--config"),
        config.as_os_str(),
        OsStr::new("service"),
        OsStr::new("run"),
    ]);

    let scm = open_scm(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    // SAFETY: valid SCM handle; all strings are NUL-terminated and outlive the call.
    let created = unsafe {
        CreateServiceW(
            *scm,
            SERVICE_NAME,
            DISPLAY_NAME,
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(binary.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(), // LocalSystem
            PCWSTR::null(),
        )
    };
    let (svc, updated) = match created {
        // SAFETY: freshly created handle owned here.
        Ok(h) => (unsafe { Owned::new(h) }, false),
        Err(e) if e.code() == ERROR_SERVICE_EXISTS.to_hresult() => {
            // Reinstall updates the existing registration (new exe or config path).
            let svc = open_service(&scm, SERVICE_ALL_ACCESS)?;
            // SAFETY: valid service handle; strings NUL-terminated.
            unsafe {
                ChangeServiceConfigW(
                    *svc,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(binary.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    w!("LocalSystem"),
                    w!(""),
                    DISPLAY_NAME,
                )
            }
            .map_err(|e| scm_error(e, "update service configuration"))?;
            (svc, true)
        }
        Err(e) => return Err(scm_error(e, "create service")),
    };

    let description = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(DESCRIPTION.0 as *mut u16) };
    // SAFETY: the struct and its string outlive the call; SCM only reads them.
    unsafe {
        ChangeServiceConfig2W(*svc, SERVICE_CONFIG_DESCRIPTION, Some(&description as *const _ as *const c_void))
    }
    .context("set service description")?;

    let mut actions = [
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 2_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 10_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 60_000 },
    ];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: struct and action array outlive the call.
    unsafe {
        ChangeServiceConfig2W(*svc, SERVICE_CONFIG_FAILURE_ACTIONS, Some(&failure as *const _ as *const c_void))
    }
    .context("set service failure actions")?;
    // Also restart when the service stops itself with an error, not only on crashes.
    let flag = SERVICE_FAILURE_ACTIONS_FLAG { fFailureActionsOnNonCrashFailures: BOOL::from(true) };
    // SAFETY: struct outlives the call.
    unsafe {
        ChangeServiceConfig2W(*svc, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, Some(&flag as *const _ as *const c_void))
    }
    .context("set service failure-actions flag")?;

    enable_software_sas()?;

    println!(
        "service remothing {} (LocalSystem, automatic start)\n  binary: {}\n  config: {}\n\
         SoftwareSASGeneration policy set to 1 (services may send Ctrl+Alt+Del)\n\
         start it now with: remothing --config \"{}\" service start",
        if updated { "updated" } else { "installed" },
        exe.display(),
        config.display(),
        config.display(),
    );
    Ok(())
}

fn uninstall() -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, SERVICE_ALL_ACCESS)?;
    stop_and_wait(&svc)?;
    // SAFETY: valid service handle with DELETE access (part of SERVICE_ALL_ACCESS).
    unsafe { DeleteService(*svc) }.map_err(|e| scm_error(e, "delete service"))?;
    println!(
        "service remothing removed (the SoftwareSASGeneration policy is left as is; \
         clear it in HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System if no longer wanted)"
    );
    Ok(())
}

fn start() -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, SERVICE_START | SERVICE_QUERY_STATUS)?;
    // SAFETY: valid service handle; no arguments.
    unsafe { StartServiceW(*svc, None) }.map_err(|e| scm_error(e, "start service"))?;
    wait_state(&svc, SERVICE_RUNNING)?;
    println!("service remothing running");
    Ok(())
}

fn stop() -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    stop_and_wait(&svc)?;
    println!("service remothing stopped");
    Ok(())
}

fn stop_and_wait(svc: &Owned<SC_HANDLE>) -> Result<()> {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: valid service handle and output struct.
    match unsafe { ControlService(**svc, SERVICE_CONTROL_STOP, &mut status) } {
        Ok(()) => wait_state(svc, SERVICE_STOPPED),
        Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => Ok(()),
        Err(e) => Err(scm_error(e, "stop service")),
    }
}

fn wait_state(svc: &Owned<SC_HANDLE>, want: SERVICE_STATUS_CURRENT_STATE) -> Result<()> {
    let deadline = Instant::now() + SCM_WAIT;
    loop {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: valid service handle and output struct.
        unsafe { QueryServiceStatus(**svc, &mut status) }.context("query service status")?;
        if status.dwCurrentState == want {
            return Ok(());
        }
        if want == SERVICE_RUNNING && status.dwCurrentState == SERVICE_STOPPED {
            bail!(
                "service stopped during start (exit code {}, service code {}); see {SERVICE_LOG} next to the config",
                status.dwWin32ExitCode,
                status.dwServiceSpecificExitCode
            );
        }
        if Instant::now() >= deadline {
            bail!("service did not reach state {} within {}s (now {})", want.0, SCM_WAIT.as_secs(), status.dwCurrentState.0);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn open_scm(access: u32) -> Result<Owned<SC_HANDLE>> {
    // SAFETY: local machine, default database.
    let h = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map_err(|e| scm_error(e, "open service control manager"))?;
    // SAFETY: handle owned here.
    Ok(unsafe { Owned::new(h) })
}

fn open_service(scm: &Owned<SC_HANDLE>, access: u32) -> Result<Owned<SC_HANDLE>> {
    // SAFETY: valid SCM handle, static service name.
    match unsafe { OpenServiceW(**scm, SERVICE_NAME, access) } {
        // SAFETY: handle owned here.
        Ok(h) => Ok(unsafe { Owned::new(h) }),
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            bail!("service remothing is not installed (run `remothing service install` as administrator)")
        }
        Err(e) => Err(scm_error(e, "open service")),
    }
}

fn scm_error(e: windows::core::Error, what: &str) -> anyhow::Error {
    if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
        anyhow!("{what}: access denied; run this command from an elevated (Administrator) prompt")
    } else {
        anyhow::Error::new(e).context(what.to_string())
    }
}

/// Lets services call SendSAS (policy "Disable or enable software Secure
/// Attention Sequence" = Services).
fn enable_software_sas() -> Result<()> {
    let mut key = HKEY::default();
    // SAFETY: static key path; output handle owned below.
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            POLICY_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    }
    .ok()
    .map_err(|e| scm_error(e, "open SoftwareSASGeneration policy key"))?;
    // SAFETY: handle returned by RegCreateKeyExW.
    let key = unsafe { Owned::new(key) };
    // SAFETY: valid key; data is a 4-byte DWORD.
    unsafe { RegSetValueExW(*key, SAS_VALUE, None, REG_DWORD, Some(&1u32.to_le_bytes())) }
        .ok()
        .map_err(|e| scm_error(e, "set SoftwareSASGeneration policy"))
}

fn sas_policy() -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: output buffer is a DWORD of the declared size.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            POLICY_KEY,
            SAS_VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    }
    .ok()
    .ok()
    .map(|()| value)
}

pub(crate) fn is_local_system() -> Result<bool> {
    let mut token = HANDLE::default();
    // SAFETY: pseudo handle of the current process; output handle owned below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.context("open process token")?;
    // SAFETY: handle owned here.
    let token = unsafe { Owned::new(token) };
    // TOKEN_USER is followed by the SID it points to; u64 keeps the buffer aligned.
    let mut buf = [0u64; 16];
    let mut len = 0u32;
    // SAFETY: buffer size passed matches the buffer.
    unsafe {
        GetTokenInformation(*token, TokenUser, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut len)
    }
    .context("query token user")?;
    // SAFETY: GetTokenInformation(TokenUser) filled a TOKEN_USER at the buffer start.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    // SAFETY: SID pointer refers into `buf`, alive here.
    Ok(unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) }.as_bool())
}

// ---------------------------------------------------------------------------
// Service runtime (`service run`, started by the SCM)

/// Kernel event handle shared between the SCM handler thread and the
/// supervisor thread; events are thread-safe by design.
struct Event(HANDLE);
// SAFETY: event handles may be signalled and waited on from any thread.
unsafe impl Send for Event {}
// SAFETY: see above.
unsafe impl Sync for Event {}

struct Shared {
    config: PathBuf,
    /// Manual reset: stays signalled once stop/shutdown arrives.
    stop: Event,
    /// Auto reset: one wake-up per notification burst.
    session: Event,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static LOG: Mutex<Option<File>> = Mutex::new(None);

fn run(config: &Path) -> Result<()> {
    let config = std::path::absolute(config).context("resolve config path")?;
    // SAFETY: unnamed events with default security; handles live for the process.
    let stop = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.context("create stop event")?;
    // SAFETY: as above.
    let session = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.context("create session event")?;
    if SHARED.set(Shared { config, stop: Event(stop), session: Event(session) }).is_err() {
        bail!("service dispatcher already started");
    }
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(SERVICE_NAME.0 as *mut u16), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR::null(), lpServiceProc: None },
    ];
    // SAFETY: NULL-terminated table; blocks until the service stops.
    match unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.to_hresult() => bail!(
            "`service run` is the entry point used by the Windows service manager and does nothing \
             from a console.\n  install and start the service (elevated): remothing service install && remothing service start\n  \
             or run the server directly in this session: remothing [--config PATH]"
        ),
        Err(e) => Err(anyhow::Error::new(e).context("StartServiceCtrlDispatcherW")),
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let Some(shared) = SHARED.get() else { return };
    let dir = config_dir(&shared.config);
    open_service_log(&dir);

    // SAFETY: static name; handler is a plain function, no context.
    let handle = match unsafe { RegisterServiceCtrlHandlerExW(SERVICE_NAME, Some(control_handler), None) } {
        Ok(h) => h,
        Err(e) => {
            log(format_args!("RegisterServiceCtrlHandlerExW failed: {e}"));
            return;
        }
    };
    let status = Status { handle, checkpoint: Cell::new(0) };
    status.set(SERVICE_START_PENDING, NO_ERROR.0, 0);
    log(format_args!("service starting (config {})", shared.config.display()));

    let result = supervise(shared, &dir, &status);
    match &result {
        Ok(()) => log(format_args!("service stopped")),
        Err(e) => log(format_args!("service failed: {e:#}")),
    }
    let (code, specific) = if result.is_ok() { (NO_ERROR.0, 0) } else { (ERROR_SERVICE_SPECIFIC_ERROR.0, 1) };
    status.set(SERVICE_STOPPED, code, specific);
}

unsafe extern "system" fn control_handler(control: u32, _event: u32, _data: *mut c_void, _ctx: *mut c_void) -> u32 {
    let Some(shared) = SHARED.get() else { return ERROR_CALL_NOT_IMPLEMENTED.0 };
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            // SAFETY: event handle valid for the process lifetime.
            let _ = unsafe { SetEvent(shared.stop.0) };
            NO_ERROR.0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            // SAFETY: as above.
            let _ = unsafe { SetEvent(shared.session.0) };
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

struct Status {
    handle: SERVICE_STATUS_HANDLE,
    checkpoint: Cell<u32>,
}

impl Status {
    fn set(&self, state: SERVICE_STATUS_CURRENT_STATE, exit: u32, specific: u32) {
        let accepted = if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
        } else {
            0
        };
        let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
        let checkpoint = if pending { self.checkpoint.get() + 1 } else { 0 };
        self.checkpoint.set(checkpoint);
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: exit,
            dwServiceSpecificExitCode: specific,
            dwCheckPoint: checkpoint,
            dwWaitHint: if pending { CHILD_EXIT_WAIT_MS + 5000 } else { 0 },
        };
        // SAFETY: handle from RegisterServiceCtrlHandlerExW.
        if let Err(e) = unsafe { SetServiceStatus(self.handle, &status) } {
            log(format_args!("SetServiceStatus({}) failed: {e}", state.0));
        }
    }
}

struct Child {
    process: Owned<HANDLE>,
    pid: u32,
    session: u32,
    started: Instant,
}

impl Child {
    fn terminate(self) {
        // SAFETY: process handle owned by self, opened with full access by CreateProcessAsUserW.
        unsafe {
            let _ = TerminateProcess(*self.process, 1);
            WaitForSingleObject(*self.process, CHILD_EXIT_WAIT_MS);
        }
        log(format_args!("terminated server pid {} (session {})", self.pid, self.session));
    }

    fn exit_code(&self) -> u32 {
        let mut code = 0;
        // SAFETY: valid process handle.
        let _ = unsafe { GetExitCodeProcess(*self.process, &mut code) };
        code
    }
}

fn active_session() -> Option<u32> {
    // SAFETY: no preconditions.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (!NO_SESSION.contains(&id)).then_some(id)
}

fn backoff(failures: u32) -> Duration {
    // A clean-looking exit still waits a second so a child that exits
    // immediately cannot spin the supervisor.
    if failures == 0 {
        return Duration::from_secs(1);
    }
    Duration::from_secs(1u64 << failures.min(6)).min(MAX_BACKOFF)
}

/// Waits on `handles`; returns the signalled index, or None on timeout.
fn wait_any(handles: &[HANDLE], timeout_ms: u32) -> Result<Option<usize>> {
    // SAFETY: all handles are valid for the duration of the wait.
    let r: WAIT_EVENT = unsafe { WaitForMultipleObjects(handles, false, timeout_ms) };
    if r == WAIT_TIMEOUT {
        return Ok(None);
    }
    let index = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    if index < handles.len() {
        Ok(Some(index))
    } else {
        Err(anyhow::Error::new(windows::core::Error::from_win32()).context("WaitForMultipleObjects"))
    }
}

/// Returns true when stop was requested while waiting for the burst to settle.
fn debounce(shared: &Shared) -> Result<bool> {
    loop {
        match wait_any(&[shared.stop.0, shared.session.0], DEBOUNCE_MS)? {
            Some(0) => return Ok(true),
            Some(_) => continue,
            None => return Ok(false),
        }
    }
}

fn supervise(shared: &Shared, dir: &Path, status: &Status) -> Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    let pid_file = dir.join(PID_FILE);
    // A previous service instance that crashed cannot have cleaned up its child,
    // which would still hold the listening port.
    kill_stale_child(&pid_file, &exe);
    status.set(SERVICE_RUNNING, NO_ERROR.0, 0);

    let mut child: Option<Child> = None;
    let mut failures = 0u32;
    let mut not_before = Instant::now();
    loop {
        if child.is_none() && Instant::now() >= not_before {
            if let Some(session) = active_session() {
                match launch(&exe, &shared.config, dir, session) {
                    Ok(c) => {
                        log(format_args!("started server pid {} in session {session}", c.pid));
                        let _ = fs::write(&pid_file, c.pid.to_string());
                        child = Some(c);
                    }
                    Err(e) => {
                        failures += 1;
                        let delay = backoff(failures);
                        log(format_args!("launch in session {session} failed: {e:#}; retry in {}s", delay.as_secs()));
                        not_before = Instant::now() + delay;
                    }
                }
            }
        }

        let timeout = match &child {
            Some(_) => POLL_MS,
            None => {
                let left = not_before.saturating_duration_since(Instant::now()).as_millis() as u32;
                if left == 0 { POLL_MS } else { left.min(POLL_MS) }
            }
        };
        let handles_buf;
        let handles: &[HANDLE] = match &child {
            Some(c) => {
                handles_buf = [shared.stop.0, shared.session.0, *c.process];
                &handles_buf
            }
            None => &[shared.stop.0, shared.session.0][..],
        };
        match wait_any(handles, timeout)? {
            Some(0) => break,
            Some(1) => {
                if debounce(shared)? {
                    break;
                }
                follow_session(&mut child, &mut not_before);
            }
            Some(_) => {
                let Some(c) = child.take() else { continue };
                let ran = c.started.elapsed();
                failures = if ran >= STABLE_RUN { 0 } else { failures + 1 };
                let delay = backoff(failures);
                log(format_args!(
                    "server pid {} exited with code {:#x} after {}s; restart in {}s",
                    c.pid,
                    c.exit_code(),
                    ran.as_secs(),
                    delay.as_secs()
                ));
                let _ = fs::remove_file(&pid_file);
                not_before = Instant::now() + delay;
            }
            None => follow_session(&mut child, &mut not_before),
        }
    }

    status.set(SERVICE_STOP_PENDING, NO_ERROR.0, 0);
    if let Some(c) = child.take() {
        c.terminate();
    }
    let _ = fs::remove_file(&pid_file);
    Ok(())
}

/// Drops a child that no longer runs in the console session so the next loop
/// iteration relaunches it there. Logon/unlock inside the same session keep the
/// child: it runs as SYSTEM on winsta0 and follows desktop switches itself.
fn follow_session(child: &mut Option<Child>, not_before: &mut Instant) {
    let active = active_session();
    match child {
        Some(c) if active != Some(c.session) => {
            log(format_args!("console moved from session {} to {active:?}", c.session));
            if let Some(c) = child.take() {
                c.terminate();
            }
            *not_before = Instant::now();
        }
        // A new session deserves an immediate attempt even during crash back-off.
        None if active.is_some() => *not_before = (*not_before).min(Instant::now() + Duration::from_millis(DEBOUNCE_MS as u64)),
        _ => {}
    }
}

fn launch(exe: &Path, config: &Path, dir: &Path, session: u32) -> Result<Child> {
    let token = session_token(session)?;

    let log_path = dir.join(SESSION_LOG);
    let _ = fs::rename(&log_path, dir.join(SESSION_LOG_PREV));
    let log_file = File::create(&log_path).with_context(|| format!("create {}", log_path.display()))?;
    let log_handle = HANDLE(log_file.as_raw_handle());
    // SAFETY: handle owned by `log_file`, alive until after CreateProcessAsUserW.
    unsafe { SetHandleInformation(log_handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .context("make log handle inheritable")?;

    let mut cmd = command_line(&[exe.as_os_str(), OsStr::new("--config"), config.as_os_str()]);
    let mut desktop = wide(OsStr::new(r"winsta0\default"));
    let cwd = wide(dir.as_os_str());
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: log_handle,
        hStdError: log_handle,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all buffers are NUL-terminated and alive for the call; the
    // command line buffer is mutable as CreateProcessAsUserW requires.
    unsafe {
        CreateProcessAsUserW(
            Some(*token),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW,
            None,
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut info,
        )
    }
    .with_context(|| format!("CreateProcessAsUserW in session {session}"))?;
    // SAFETY: handles returned by CreateProcessAsUserW, owned here.
    let (process, _thread) = unsafe { (Owned::new(info.hProcess), Owned::new(info.hThread)) };
    drop(log_file);
    Ok(Child { process, pid: info.dwProcessId, session, started: Instant::now() })
}

/// The service's own LocalSystem token, retargeted to `session`.
fn session_token(session: u32) -> Result<Owned<HANDLE>> {
    let mut own = HANDLE::default();
    // SAFETY: pseudo handle of the current process.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut own) }
        .context("open service token")?;
    // SAFETY: handle owned here.
    let own = unsafe { Owned::new(own) };
    let mut dup = HANDLE::default();
    // SAFETY: valid source token with TOKEN_DUPLICATE.
    unsafe { DuplicateTokenEx(*own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut dup) }
        .context("duplicate service token")?;
    // SAFETY: handle owned here.
    let dup = unsafe { Owned::new(dup) };
    // SAFETY: TokenSessionId takes a DWORD; needs SeTcbPrivilege, held by LocalSystem.
    unsafe {
        SetTokenInformation(*dup, TokenSessionId, &session as *const u32 as *const c_void, size_of::<u32>() as u32)
    }
    .with_context(|| format!("set token session {session}"))?;
    Ok(dup)
}

fn kill_stale_child(pid_file: &Path, exe: &Path) {
    let Some(pid) = fs::read_to_string(pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok()) else {
        return;
    };
    let _ = fs::remove_file(pid_file);
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    // SAFETY: plain open by pid; failure means it is gone.
    let Ok(h) = (unsafe { OpenProcess(access, false, pid) }) else { return };
    // SAFETY: handle owned here.
    let h = unsafe { Owned::new(h) };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buffer length passed in `len`.
    if unsafe { QueryFullProcessImageNameW(*h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.is_err() {
        return;
    }
    let image = PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize]));
    // The pid may have been reused by an unrelated process since.
    if !image.as_os_str().eq_ignore_ascii_case(exe.as_os_str()) {
        return;
    }
    // SAFETY: valid handle with terminate/synchronize access.
    unsafe {
        let _ = TerminateProcess(*h, 1);
        WaitForSingleObject(*h, CHILD_EXIT_WAIT_MS);
    }
    log(format_args!("terminated stale server pid {pid} left by a previous service instance"));
}

fn config_dir(config: &Path) -> PathBuf {
    let dir = config.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let _ = fs::create_dir_all(&dir);
    dir
}

fn open_service_log(dir: &Path) {
    let path = dir.join(SERVICE_LOG);
    let truncate = fs::metadata(&path).map(|m| m.len() > SERVICE_LOG_MAX).unwrap_or(false);
    let file = OpenOptions::new().create(true).append(!truncate).write(true).truncate(truncate).open(&path).ok();
    if let Ok(mut slot) = LOG.lock() {
        *slot = file;
    }
}

fn log(args: fmt::Arguments) {
    let Ok(mut slot) = LOG.lock() else { return };
    let Some(file) = slot.as_mut() else { return };
    // SAFETY: no preconditions.
    let t = unsafe { GetLocalTime() };
    let _ = writeln!(
        file,
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {args}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
}

// ---------------------------------------------------------------------------
// Strings

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// NUL-terminated command line quoted per the MSVC CRT rules, so paths with
/// spaces or trailing backslashes reach `std::env::args` unchanged.
fn command_line(args: &[&OsStr]) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUOTE: u16 = b'"' as u16;
    let mut out = Vec::new();
    for arg in args {
        if !out.is_empty() {
            out.push(b' ' as u16);
        }
        out.push(QUOTE);
        let mut slashes = 0usize;
        for c in arg.encode_wide() {
            if c == BACKSLASH {
                slashes += 1;
                continue;
            }
            let n = if c == QUOTE { slashes * 2 + 1 } else { slashes };
            out.extend(std::iter::repeat_n(BACKSLASH, n));
            out.push(c);
            slashes = 0;
        }
        out.extend(std::iter::repeat_n(BACKSLASH, slashes * 2));
        out.push(QUOTE);
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let mut w = command_line(&args);
        assert_eq!(w.pop(), Some(0));
        String::from_utf16(&w).unwrap()
    }

    #[test]
    fn quotes_per_crt_rules() {
        assert_eq!(line(&[r"C:\Program Files\r.exe", "--config"]), r#""C:\Program Files\r.exe" "--config""#);
        // Trailing backslashes are doubled so they do not escape the closing quote.
        assert_eq!(line(&[r"C:\dir\"]), r#""C:\dir\\""#);
        assert_eq!(line(&[r#"a\"b"#]), r#""a\\\"b""#);
    }
}
