//! Per-user Windows startup. All commands use argument arrays rather than a shell.
use anyhow::Result;
#[cfg(windows)]
use anyhow::{Context, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

pub fn stop(c: &Connection) -> Result<Value> {
    c.execute("INSERT INTO runtime_state(key,value) VALUES('stop_requested','1') ON CONFLICT(key) DO UPDATE SET value='1'",[])?;
    Ok(json!({"stop_requested":true}))
}

#[cfg(windows)]
fn task_name(data: &Path) -> String {
    // Stable across process restarts and multiple independent data directories.
    let mut hash = 14695981039346656037u64;
    for b in data.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("RustInTime-{hash:016x}")
}

#[cfg(windows)]
fn schtasks(args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = std::process::Command::new("schtasks.exe")
        .args(args)
        .output()
        .context("run Windows Task Scheduler")?;
    ensure!(
        output.status.success(),
        "Task Scheduler: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(windows)]
pub fn install(data: &Path, config: &Path) -> Result<Value> {
    let exe = std::env::current_exe()?;
    let identity = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .context("read current Windows user identity")?;
    ensure!(
        identity.status.success(),
        "could not identify current Windows user"
    );
    // The account name may use a legacy Windows code page; the SID itself
    // is ASCII, so decoding the unused name must not make startup fail.
    let identity_text = String::from_utf8_lossy(&identity.stdout);
    let sid = identity_text
        .trim()
        .rsplit(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    ensure!(
        sid.starts_with("S-1-")
            && sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S'),
        "Windows user SID is missing from whoami output"
    );
    let xml_path = data.join("startup-task.xml");
    std::fs::write(&xml_path, task_xml(&exe, data, config, sid))?;
    let name = task_name(data);
    schtasks(&[
        "/Create".as_ref(),
        "/TN".as_ref(),
        name.as_ref(),
        "/XML".as_ref(),
        xml_path.as_os_str(),
        "/F".as_ref(),
    ])?;
    schtasks(&["/Run".as_ref(), "/TN".as_ref(), name.as_ref()])?;
    Ok(json!({"installed":name,"executable":exe,"data_dir":data}))
}

#[cfg(windows)]
pub fn uninstall(data: &Path, c: &Connection) -> Result<Value> {
    stop(c)?;
    let name = task_name(data);
    schtasks(&[
        "/Delete".as_ref(),
        "/TN".as_ref(),
        name.as_ref(),
        "/F".as_ref(),
    ])?;
    Ok(json!({"uninstalled":name,"stop_requested":true}))
}

#[cfg(windows)]
pub fn restart(data: &Path, _config: &Path, c: &Connection) -> Result<Value> {
    stop(c)?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("tracker.lock"))?;
    loop {
        if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
            fs2::FileExt::unlock(&lock)?;
            break;
        }
        ensure!(
            std::time::Instant::now() < until,
            "worker did not stop within ten seconds"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let name = task_name(data);
    schtasks(&["/Run".as_ref(), "/TN".as_ref(), name.as_ref()])?;
    Ok(json!({"restarted":name}))
}

#[cfg(not(windows))]
pub fn install(_data: &Path, _config: &Path) -> Result<Value> {
    anyhow::bail!(
        "startup installation requires Windows; use rit daemon for transcript tracking on this host"
    )
}
#[cfg(not(windows))]
pub fn uninstall(_data: &Path, _c: &Connection) -> Result<Value> {
    anyhow::bail!("startup management requires Windows")
}
#[cfg(not(windows))]
pub fn restart(_data: &Path, _config: &Path, _c: &Connection) -> Result<Value> {
    anyhow::bail!("startup management requires Windows")
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Quote a Windows command-line argument according to CommandLineToArgvW rules.
fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in s.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

pub fn task_xml(exe: &Path, data: &Path, config: &Path, user_sid: &str) -> String {
    let args = format!(
        "--data-dir {} --config {} daemon",
        quote(&data.to_string_lossy()),
        quote(&config.to_string_lossy())
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
 <RegistrationInfo><Description>Rust in Time project-aware time tracking</Description></RegistrationInfo>
 <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
 <Principals><Principal id="CurrentUser"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
 <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
 <Actions Context="CurrentUser"><Exec><Command>{}</Command><Arguments>{}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>
</Task>"#,
        xml(&exe.to_string_lossy()),
        xml(&args),
        xml(&data.to_string_lossy()),
        sid = xml(user_sid)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_handles_spaces_xml_and_trailing_backslashes() {
        let task = task_xml(
            Path::new(r"C:\Program Files\rit.exe"),
            Path::new("C:\\A&B\\"),
            Path::new(r"C:\User\config.yaml"),
            "S-1-5-21-42",
        );
        assert!(task.contains("C:\\Program Files\\rit.exe"));
        assert!(task.contains("A&amp;B"));
        assert!(task.contains("B\\\\&quot;"));
        assert!(task.contains("InteractiveToken"));
        assert_eq!(task.matches("<UserId>S-1-5-21-42</UserId>").count(), 2);
        assert!(task.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }
}
