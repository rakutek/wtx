use crate::util::{lima_dir, shq, wtx_home};
use anyhow::{anyhow, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// Lima の control master を迂回した接続引数。
/// provision の `usermod -aG docker` は master 確立後の既存セッションに効かないため、
/// 常に新規接続する（VERIFICATION.md 参照）。
fn ssh_base(name: &str) -> Vec<String> {
    vec![
        "-F".into(),
        lima_dir(name).join("ssh.config").to_string_lossy().into_owned(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPath=none".into(),
    ]
}

/// VM 内の bash に stdin 経由でスクリプトを流す（クォート事故が起きない）。
pub fn vm_script(name: &str, script: &str, extra_stdin: Option<&[u8]>) -> Result<()> {
    let mut args = ssh_base(name);
    args.push(format!("lima-{name}"));
    args.push("--".into());
    args.push("bash".into());
    args.push("-s".into());

    let mut child = Command::new("ssh").args(&args).stdin(Stdio::piped()).spawn()?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| anyhow!("no stdin"))?;
        stdin.write_all(script.as_bytes())?;
        // bash -s は行単位で読むため、スクリプト末尾に改行がないと
        // 後続の stdin データがコマンド行に連結されてしまう
        if !script.ends_with('\n') {
            stdin.write_all(b"\n")?;
        }
        if let Some(b) = extra_stdin {
            stdin.write_all(b)?;
        }
    }
    let st = child.wait()?;
    if !st.success() {
        return Err(anyhow!("remote script failed (exit {:?})", st.code()));
    }
    Ok(())
}

/// VM 内でコマンドを実行し、標準出力を取り込む（wtx 自身が結果を使う用途）。
pub fn capture(name: &str, remote: &str) -> Result<String> {
    let mut args = ssh_base(name);
    args.push(format!("lima-{name}"));
    args.push("--".into());
    args.push(remote.to_string());
    let out = Command::new("ssh").args(&args).stderr(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(anyhow!("remote command failed: {remote}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// VM 内でコマンドを実行する。終了コードはそのまま素通しする（オーケストレータ連携の契約）。
pub fn exec(name: &str, workdir: Option<&str>, cmd: &[String]) -> Result<()> {
    let quoted: Vec<String> = cmd.iter().map(|s| shq(s)).collect();
    let mut remote = quoted.join(" ");
    if let Some(d) = workdir {
        remote = format!("cd {} && {}", shq(d), remote);
    }
    let mut args = ssh_base(name);
    args.push(format!("lima-{name}"));
    args.push("--".into());
    args.push(remote);
    let st = Command::new("ssh").args(&args).status()?;
    std::process::exit(st.code().unwrap_or(1));
}

pub fn shell(name: &str) -> Result<()> {
    let mut args = ssh_base(name);
    args.push("-t".into());
    args.push(format!("lima-{name}"));
    let st = Command::new("ssh").args(&args).status()?;
    if !st.success() {
        return Err(anyhow!("shell exited with {:?}", st.code()));
    }
    Ok(())
}

/// forward: ホスト A → VM B (ssh -L) / bridge: VM A → ホスト B (ssh -R)
pub fn forward(name: &str, spec: &str, reverse: bool) -> Result<()> {
    let (a, b) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("port spec must be A:B"))?;
    let sock = wtx_home().join(format!("{name}-{a}.sock"));
    let (flag, value) = if reverse {
        ("-R", format!("{a}:127.0.0.1:{b}"))
    } else {
        ("-L", format!("{a}:localhost:{b}"))
    };
    let mut args = ssh_base(name);
    args.extend([
        "-f".into(),
        "-N".into(),
        "-M".into(),
        "-S".into(),
        sock.to_string_lossy().into_owned(),
        flag.into(),
        value,
        format!("lima-{name}"),
    ]);
    let st = Command::new("ssh").args(&args).status()?;
    if !st.success() {
        return Err(anyhow!("ssh {flag} failed"));
    }
    let kind = if reverse { "bridge" } else { "forward" };
    println!("{kind} {spec} active (stop: wtx unforward {name} {a})");
    Ok(())
}

pub fn unforward(name: &str, port: &str) -> Result<()> {
    let sock = wtx_home().join(format!("{name}-{port}.sock"));
    let _ = Command::new("ssh")
        .args(["-S", &sock.to_string_lossy(), "-O", "exit", &format!("lima-{name}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&sock);
    println!("stopped");
    Ok(())
}

pub fn close_all_forwards(name: &str) {
    if let Ok(rd) = std::fs::read_dir(wtx_home()) {
        for e in rd.flatten() {
            let p = e.path();
            let fname = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if fname.starts_with(&format!("{name}-")) && fname.ends_with(".sock") {
                let _ = Command::new("ssh")
                    .args(["-S", &p.to_string_lossy(), "-O", "exit", &format!("lima-{name}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}
