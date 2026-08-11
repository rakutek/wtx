use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn wtx_home() -> PathBuf {
    let d = dirs::home_dir().unwrap_or_default().join(".wtx");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn lima_dir(name: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".lima")
        .join(name)
}

/// シェルに渡す文字列を安全にクォートする。
pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 表示幅（全角=2）で右埋めする。`{:<n}` は文字数で数えるため列がずれることがある。
pub fn pad(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

pub fn limactl(args: &[&str]) -> Result<()> {
    let st = Command::new("limactl").args(args).status()?;
    if !st.success() {
        return Err(anyhow!("limactl {} failed", args.join(" ")));
    }
    Ok(())
}

/// 出力を継承せずに実行する limactl。TUIのバックグラウンド操作から呼んでも
/// 画面を汚さない。失敗時は stderr の最後の非空行をエラーとして返す。
pub fn limactl_capture(args: &[&str]) -> std::result::Result<(), String> {
    let out = Command::new("limactl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("limactl failed")
        .to_string())
}

pub fn limactl_out(args: &[&str]) -> String {
    Command::new("limactl")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn lima_status(name: &str) -> String {
    limactl_out(&["list", name, "--format", "{{.Status}}"])
}

pub fn lima_status_checked(name: &str) -> Result<String> {
    let out = Command::new("limactl")
        .args(["list", "--format", "{{.Name}}\t{{.Status}}"])
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let detail = err
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("limactl list failed");
        return Err(anyhow!("limactl list: {detail}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let (instance, status) = line.split_once('\t')?;
            (instance == name).then(|| status.to_string())
        })
        .unwrap_or_default())
}

pub fn git_out(dir: &Path, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn git_run(dir: &Path, args: &[&str]) -> Result<()> {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !st.success() {
        return Err(anyhow!("git {} failed", args.join(" ")));
    }
    Ok(())
}

pub fn git_config_global(key: &str, fallback: &str) -> String {
    let out = Command::new("git")
        .args(["config", "--global", key])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}
