#[cfg(not(windows))]
fn main() {
    eprintln!("hotswitch-receiver-service only runs on Windows");
}

#[cfg(windows)]
mod windows_service_main {
    use std::ffi::OsStr;
    use std::fs::{create_dir_all, OpenOptions};
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::null;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
        TOKEN_ALL_ACCESS,
    };
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess,
        WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    const SERVICE_NAME: &str = "Hotswitch";
    const LOG_DIR: &str = r"C:\ProgramData\hotswitch";

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> windows_service::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    fn service_main(_args: Vec<std::ffi::OsString>) {
        if let Err(error) = run_service() {
            log_line(&format!("service error: {error:?}"));
        }
    }

    enum ServiceEvent {
        SessionChanged,
        Stop,
    }

    struct ChildProcess {
        handle: HANDLE,
        session_id: u32,
    }

    fn run_service() -> windows_service::Result<()> {
        let (tx, rx) = mpsc::channel();
        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop => {
                    let _ = tx.send(ServiceEvent::Stop);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::SessionChange(_) => {
                    let _ = tx.send(ServiceEvent::SessionChanged);
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;

        set_service_status(
            &status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SESSION_CHANGE,
        )?;

        let mut child = spawn_receiver_for_active_session();

        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(ServiceEvent::Stop) => break,
                Ok(ServiceEvent::SessionChanged) => {
                    stop_child(&mut child);
                    child = spawn_receiver_for_active_session();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        set_service_status(
            &status_handle,
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
        )?;
        stop_child(&mut child);
        set_service_status(
            &status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
        )
    }

    fn set_service_status(
        status_handle: &service_control_handler::ServiceStatusHandle,
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
    ) -> windows_service::Result<()> {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    }

    fn spawn_receiver_for_active_session() -> Option<ChildProcess> {
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == u32::MAX {
            log_line("no active console session");
            return None;
        }

        match spawn_receiver_in_session(session_id) {
            Ok(child) => {
                log_line(&format!("spawned receiver in session {session_id}"));
                Some(child)
            }
            Err(error) => {
                log_line(&format!(
                    "failed to spawn receiver in session {session_id}: {error}"
                ));
                None
            }
        }
    }

    fn spawn_receiver_in_session(session_id: u32) -> Result<ChildProcess, String> {
        let receiver_path =
            receiver_path().ok_or_else(|| "failed to resolve receiver path".to_string())?;
        let receiver_dir = receiver_path
            .parent()
            .ok_or_else(|| "failed to resolve receiver directory".to_string())?;

        let mut current_token = HANDLE::default();
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut current_token)
                .map_err(|error| format!("OpenProcessToken failed: {error}"))?;
        }

        let mut primary_token = HANDLE::default();
        let duplicate_result = unsafe {
            DuplicateTokenEx(
                current_token,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary_token,
            )
        };
        unsafe {
            let _ = CloseHandle(current_token);
        }
        duplicate_result.map_err(|error| format!("DuplicateTokenEx failed: {error}"))?;

        let set_session_result = unsafe {
            SetTokenInformation(
                primary_token,
                TokenSessionId,
                (&session_id as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        };
        if let Err(error) = set_session_result {
            unsafe {
                let _ = CloseHandle(primary_token);
            }
            return Err(format!("SetTokenInformation failed: {error}"));
        }

        let app = to_wide(receiver_path.as_os_str());
        let cwd = to_wide(receiver_dir.as_os_str());
        let mut desktop = to_wide(OsStr::new("winsta0\\default"));
        let mut startup = STARTUPINFOW::default();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.lpDesktop = PWSTR(desktop.as_mut_ptr());

        let mut process_info = PROCESS_INFORMATION::default();
        let create_result = unsafe {
            CreateProcessAsUserW(
                primary_token,
                PCWSTR(app.as_ptr()),
                PWSTR::null(),
                None,
                None,
                false,
                Default::default(),
                None,
                PCWSTR(cwd.as_ptr()),
                &startup,
                &mut process_info,
            )
        };

        unsafe {
            let _ = CloseHandle(primary_token);
        }

        create_result.map_err(|error| {
            format!(
                "CreateProcessAsUserW failed: {error} (last error {})",
                unsafe { GetLastError().0 }
            )
        })?;

        unsafe {
            let _ = CloseHandle(process_info.hThread);
        }

        Ok(ChildProcess {
            handle: process_info.hProcess,
            session_id,
        })
    }

    fn stop_child(child: &mut Option<ChildProcess>) {
        let Some(child_process) = child.take() else {
            return;
        };

        let wait_result = unsafe { WaitForSingleObject(child_process.handle, 0) };
        if wait_result != WAIT_OBJECT_0 {
            let _ = unsafe { TerminateProcess(child_process.handle, 0) };
            let _ = unsafe { WaitForSingleObject(child_process.handle, 5_000) };
        }

        let _ = unsafe { CloseHandle(child_process.handle) };
        log_line(&format!(
            "stopped receiver for session {}",
            child_process.session_id
        ));
    }

    fn receiver_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.with_file_name("hotswitch-receiver.exe"))
    }

    fn log_line(message: &str) {
        let log_dir = Path::new(LOG_DIR);
        let _ = create_dir_all(log_dir);
        let log_path = log_dir.join("service.log");
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
            let _ = writeln!(file, "{message}");
        }
    }

    fn to_wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    windows_service_main::run()
}
