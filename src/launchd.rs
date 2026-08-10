//! launchd ソケットアクティベーション。常駐プロセスを持たず、
//! VM からの pull が来た瞬間に wtx が起動し、アイドルで終了する。
use crate::mirror::mirror_config;
use crate::util::wtx_home;
use anyhow::{anyhow, Result};
use std::ffi::CString;
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::Command;

extern "C" {
    fn launch_activate_socket(name: *const libc::c_char, fds: *mut *mut libc::c_int, cnt: *mut libc::size_t) -> libc::c_int;
}

const LABEL: &str = "com.wtx.mirror";

pub fn plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub fn installed() -> bool {
    plist_path().exists()
}

/// launchd から渡されたソケットを受け取る。launchd 管理下でなければ None
/// （呼び出し側は通常の bind にフォールバックする）。
pub fn activated_listener(name: &str) -> Option<TcpListener> {
    let cname = CString::new(name).ok()?;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut cnt: libc::size_t = 0;
    unsafe {
        if launch_activate_socket(cname.as_ptr(), &mut fds, &mut cnt) != 0 || cnt == 0 {
            return None;
        }
        let fd = *fds;
        libc::free(fds as *mut libc::c_void);
        Some(TcpListener::from_raw_fd(fd))
    }
}

pub fn install() -> Result<()> {
    let self_exe = std::env::current_exe()?;
    let mut sockets = String::new();
    for e in mirror_config() {
        sockets.push_str(&format!(
            "    <key>{}</key>\n    <dict>\n      <key>SockNodeName</key><string>127.0.0.1</string>\n      <key>SockServiceName</key><string>{}</string>\n      <key>SockType</key><string>stream</string>\n    </dict>\n",
            e.registry, e.port
        ));
    }
    let log = wtx_home().join("mirror.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string><string>mirror</string><string>serve</string>
  </array>
  <key>Sockets</key>
  <dict>
{sockets}  </dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = self_exe.display(),
        sockets = sockets,
        log = log.display()
    );

    // 既存の常駐プロセスが同じポートを掴んでいると bootstrap が失敗する
    if let Ok(pid) = std::fs::read_to_string(wtx_home().join("mirror.pid")) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let _ = std::fs::remove_file(wtx_home().join("mirror.pid"));
    }
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .status();

    let path = plist_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, plist)?;
    let out = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()])
        .output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "launchctl bootstrap: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    println!("mirror: launchd オンデマンド起動を登録しました（常駐プロセスなし）");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .status();
    match std::fs::remove_file(plist_path()) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    println!("mirror: launchd 登録を解除しました");
    Ok(())
}
