use crate::util::{lima_dir, shq, wtx_home};
use anyhow::{anyhow, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Lima の control master を迂回した接続引数。
/// provision の `usermod -aG docker` は master 確立後の既存セッションに効かないため、
/// 常に新規接続する（VERIFICATION.md 参照）。
fn ssh_base(name: &str) -> Vec<String> {
    vec![
        "-F".into(),
        lima_dir(name)
            .join("ssh.config")
            .to_string_lossy()
            .into_owned(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPath=none".into(),
    ]
}

/// VM 内の bash に stdin 経由でスクリプトを流す（クォート事故が起きない）。
pub fn vm_script(name: &str, script: &str, extra_stdin: Option<&[u8]>) -> Result<()> {
    vm_script_with_output(name, script, extra_stdin, true)
}

/// JSON receipt を壊さないよう、必要な呼び出しではリモート stdout を抑止できる。
pub fn vm_script_with_output(
    name: &str,
    script: &str,
    extra_stdin: Option<&[u8]>,
    inherit_stdout: bool,
) -> Result<()> {
    let mut args = ssh_base(name);
    args.push(format!("lima-{name}"));
    args.push("--".into());
    args.push("bash".into());
    args.push("-s".into());

    let mut command = Command::new("ssh");
    command.args(&args).stdin(Stdio::piped());
    if !inherit_stdout {
        command.stdout(Stdio::null());
    }
    let mut child = command.spawn()?;
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
    let out = Command::new("ssh")
        .args(&args)
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(anyhow!("remote command failed: {remote}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// VM 内でコマンドを実行する。終了コードはそのまま素通しする（オーケストレータ連携の契約）。
/// tty=true では ssh に PTY を強制割り当てし、Codex/Claude 等の対話TUIをそのまま接続する。
pub fn exec(name: &str, workdir: Option<&str>, cmd: &[String], tty: bool) -> Result<()> {
    let quoted: Vec<String> = cmd.iter().map(|s| shq(s)).collect();
    let mut remote = quoted.join(" ");
    if let Some(d) = workdir {
        remote = format!("cd {} && {}", shq(d), remote);
    }
    let mut args = ssh_base(name);
    if tty {
        // -tt は、wtx 自身が別のPTYランナーから起動された場合にも割り当てを強制する。
        // ssh が window resize と signal を中継するため、独自PTY実装は持たない。
        args.push("-tt".into());
    }
    args.push(format!("lima-{name}"));
    args.push("--".into());
    args.push(remote);
    let st = Command::new("ssh").args(&args).status()?;
    std::process::exit(st.code().unwrap_or(1));
}

/// dockerd がコマンドを受け付けられるかを副作用なしで確認する。
pub fn docker_ready(name: &str) -> bool {
    capture(name, "docker info >/dev/null 2>&1 && printf '%s' ready")
        .map(|out| out == "ready")
        .unwrap_or(false)
}

/// 起動直後のVMで dockerd ready を待つ。タイムアウトはオーケストレータ側で調整可能。
pub fn wait_docker_ready(name: &str, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        if docker_ready(name) {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(anyhow!(
                "timed out after {}s waiting for docker in {name}",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PortSpec {
    host: u16,
    guest: u16,
}

fn parse_port_spec(spec: &str) -> Result<PortSpec> {
    let (host, guest) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("port spec must be HOST:GUEST"))?;
    let host = host
        .parse::<u16>()
        .map_err(|_| anyhow!("invalid host port in {spec}"))?;
    let guest = guest
        .parse::<u16>()
        .map_err(|_| anyhow!("invalid guest port in {spec}"))?;
    if host == 0 || guest == 0 {
        return Err(anyhow!("ports must be between 1 and 65535"));
    }
    Ok(PortSpec { host, guest })
}

/// forward: ホスト HOST → VM GUEST (ssh -L)
/// bridge: VM GUEST → ホスト HOST (ssh -R)。CLIの並びは常に HOST:GUEST。
pub fn forward(name: &str, spec: &str, reverse: bool) -> Result<()> {
    forward_impl(name, spec, reverse, false)
}

fn forward_impl(name: &str, spec: &str, reverse: bool, quiet: bool) -> Result<()> {
    let spec = parse_port_spec(spec)?;
    let bound_port = if reverse { spec.guest } else { spec.host };
    let sock = wtx_home().join(format!("{name}-{bound_port}.sock"));
    let (flag, value) = if reverse {
        ("-R", format!("{}:127.0.0.1:{}", spec.guest, spec.host))
    } else {
        ("-L", format!("{}:localhost:{}", spec.host, spec.guest))
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
    if !quiet {
        let kind = if reverse { "bridge" } else { "forward" };
        println!(
            "{kind} {}:{} active (stop: wtx unforward --name {name} {bound_port})",
            spec.host, spec.guest
        );
    }
    Ok(())
}

/// forward の ssh マスターが生きているか（`-O check`）。
/// VM停止でマスターは自然終了しソケットも消えるのが通常だが、異常終了で残ることはある。
pub fn master_alive(name: &str, host_port: u16) -> bool {
    let sock = wtx_home().join(format!("{name}-{host_port}.sock"));
    if !sock.exists() {
        return false;
    }
    Command::new("ssh")
        .args([
            "-S",
            &sock.to_string_lossy(),
            "-O",
            "check",
            &format!("lima-{name}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 死んでいれば張り直す冪等 forward。`sim env` の再armに使うため stdout を汚さない。
pub fn ensure_forward(name: &str, host: u16, guest: u16) -> Result<()> {
    if master_alive(name, host) {
        return Ok(());
    }
    drop_forward(name, host); // 残骸ソケットの掃除（無ければ何もしない）
    forward_impl(name, &format!("{host}:{guest}"), false, true)
}

/// forward を畳む（出力なし）。ソケットが無ければ何もしない。
pub fn drop_forward(name: &str, host_port: u16) {
    let sock = wtx_home().join(format!("{name}-{host_port}.sock"));
    let _ = Command::new("ssh")
        .args([
            "-S",
            &sock.to_string_lossy(),
            "-O",
            "exit",
            &format!("lima-{name}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&sock);
}

pub fn unforward(name: &str, port: &str) -> Result<()> {
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow!("invalid bound port: {port}"))?;
    if port == 0 {
        return Err(anyhow!("port must be between 1 and 65535"));
    }
    let sock = wtx_home().join(format!("{name}-{port}.sock"));
    let _ = Command::new("ssh")
        .args([
            "-S",
            &sock.to_string_lossy(),
            "-O",
            "exit",
            &format!("lima-{name}"),
        ])
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
            let fname = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if fname.starts_with(&format!("{name}-")) && fname.ends_with(".sock") {
                let _ = Command::new("ssh")
                    .args([
                        "-S",
                        &p.to_string_lossy(),
                        "-O",
                        "exit",
                        &format!("lima-{name}"),
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_spec_is_always_host_then_guest() {
        assert_eq!(
            parse_port_spec("8080:3000").unwrap(),
            PortSpec {
                host: 8080,
                guest: 3000,
            }
        );
    }

    #[test]
    fn port_spec_rejects_paths_and_zero() {
        assert!(parse_port_spec("../../x:3000").is_err());
        assert!(parse_port_spec("0:3000").is_err());
    }
}
