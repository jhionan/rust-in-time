use anyhow::Result;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::domain::Signal;

#[derive(Debug, Clone, Default)]
pub struct ProcessSample {
    pub pid: u32,
    pub start_time: u64,
    pub binary_name: String,
    /// Monotonic total CPU use in centiseconds, summed across cores.
    pub cpu_centiseconds: u64,
    pub cwd: Option<String>,
}

#[derive(Default)]
pub struct Platform {
    system: System,
}

impl Platform {
    pub fn processes(&mut self) -> Vec<ProcessSample> {
        // sysinfo reads the Windows PEB, including WOW64 process parameters.
        // Refresh cwd each time: programs can chdir during a long-lived session.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_cwd(UpdateKind::Always),
        );
        self.system
            .processes()
            .iter()
            .map(|(pid, process)| ProcessSample {
                pid: pid.as_u32(),
                start_time: process.start_time(),
                binary_name: if cfg!(windows) {
                    process.name().to_string_lossy().to_lowercase()
                } else {
                    process.name().to_string_lossy().into_owned()
                },
                cpu_centiseconds: process.accumulated_cpu_time() / 10,
                // An executable's parent is never evidence of a working directory.
                cwd: process
                    .cwd()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_string_lossy().into_owned()),
            })
            .collect()
    }

    pub fn foreground(&self) -> Result<Option<Signal>> {
        native::foreground()
    }

    pub fn idle_seconds(&self) -> Result<Option<u64>> {
        native::idle_seconds()
    }
}

pub fn debug(signal: &str) -> Result<serde_json::Value> {
    let mut platform = Platform::default();
    match signal {
        "idle" => Ok(
            serde_json::json!({ "supported": cfg!(windows), "idle_seconds": platform.idle_seconds()? }),
        ),
        "windows" | "frontmost" | "focus" => Ok(
            serde_json::json!({ "supported": cfg!(windows), "foreground": platform.foreground()? }),
        ),
        "processes" | "agents" => {
            let processes: Vec<_> = platform.processes().into_iter().map(|p| serde_json::json!({"pid":p.pid,"binary_name":p.binary_name,"cwd":p.cwd,"cpu_centiseconds":p.cpu_centiseconds})).collect();
            Ok(serde_json::json!({ "processes": processes }))
        }
        _ => anyhow::bail!(
            "unknown debug signal {signal:?}; use windows, frontmost, idle, or processes"
        ),
    }
}

#[cfg(not(windows))]
mod native {
    use super::*;

    pub fn foreground() -> Result<Option<Signal>> {
        Ok(None)
    }
    pub fn idle_seconds() -> Result<Option<u64>> {
        Ok(None)
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::{
                SystemInformation::GetTickCount64,
                Threading::{
                    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                    QueryFullProcessImageNameW,
                },
            },
            UI::{
                Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
                WindowsAndMessaging::{
                    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
                    GetWindowThreadProcessId,
                },
            },
        },
        core::PWSTR,
    };

    struct ProcessHandle(HANDLE);
    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            // SAFETY: the handle was obtained from OpenProcess and is owned here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub fn foreground() -> Result<Option<Signal>> {
        // SAFETY: all output buffers below are allocated to their advertised
        // capacity; the foreground window and process may disappear at any time,
        // in which case API failures are returned to the worker for diagnostics.
        unsafe {
            let window = GetForegroundWindow();
            if window.0.is_null() {
                return Ok(None);
            }
            let mut pid = 0;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if pid == 0 {
                return Ok(None);
            }
            let handle = ProcessHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
            let mut executable = vec![0u16; 32768];
            let mut capacity = executable.len() as u32;
            QueryFullProcessImageNameW(
                handle.0,
                PROCESS_NAME_WIN32,
                PWSTR(executable.as_mut_ptr()),
                &mut capacity,
            )?;
            let full_name = String::from_utf16_lossy(&executable[..capacity as usize]);
            let binary = full_name
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&full_name)
                .to_lowercase();
            let title_len = GetWindowTextLengthW(window).max(0) as usize;
            let mut title = vec![0; title_len.saturating_add(1)];
            let written = GetWindowTextW(window, &mut title).max(0) as usize;
            Ok(Some(Signal {
                source: "focus".into(),
                bundle_id: binary.clone(),
                binary_name: binary,
                window_title: String::from_utf16_lossy(&title[..written]),
                ..Signal::default()
            }))
        }
    }

    pub fn idle_seconds() -> Result<Option<u64>> {
        let mut input = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: LASTINPUTINFO has its correct size and remains valid for the call.
        unsafe {
            if !GetLastInputInfo(&mut input).as_bool() {
                return Err(windows::core::Error::from_thread().into());
            }
            // LASTINPUTINFO stores a wrapping 32-bit tick count, even when the
            // system has been running for more than 49 days.
            let elapsed = (GetTickCount64() as u32).wrapping_sub(input.dwTime);
            Ok(Some(u64::from(elapsed) / 1000))
        }
    }
}
