use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn wtx_home() -> PathBuf {
    let d = dirs::home_dir().unwrap_or_default().join(".wtx");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn lima_dir(name: &str) -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".lima").join(name)
}

/// シェルに渡す文字列を安全にクォートする。
pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn limactl(args: &[&str]) -> Result<()> {
    let st = Command::new("limactl").args(args).status()?;
    if !st.success() {
        return Err(anyhow!("limactl {} failed", args.join(" ")));
    }
    Ok(())
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
