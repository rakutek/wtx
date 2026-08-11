use crate::util::{lima_dir, shq, wtx_home};
use anyhow::{anyhow, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Connection arguments that bypass Lima's control master.
/// The provisioning command `usermod -aG docker` does not affect sessions established
/// through an existing master, so always create a new connection. See VERIFICATION.md.
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

/// Send a script to Bash in the VM through stdin, avoiding shell-quoting issues.
pub fn vm_script(name: &str, script: &str, extra_stdin: Option<&[u8]>) -> Result<()> {
    vm_script_with_output(name, script, extra_stdin, true)
}

/// Allow callers to suppress remote stdout so it cannot corrupt a JSON receipt.
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
        // `bash -s` reads complete lines, so without a trailing newline the following
        // stdin data would be appended to the last command line.
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

/// Run a command in the VM and capture stdout for use by wtx itself.
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

/// Run a command in the VM and pass its exit status through unchanged, as required by the
/// orchestrator contract. With `tty=true`, force SSH to allocate a PTY so interactive TUIs
/// such as Codex and Claude remain connected directly.
pub fn exec(name: &str, workdir: Option<&str>, cmd: &[String], tty: bool) -> Result<()> {
    let quoted: Vec<String> = cmd.iter().map(|s| shq(s)).collect();
    let mut remote = quoted.join(" ");
    if let Some(d) = workdir {
        remote = format!("cd {} && {}", shq(d), remote);
    }
    let mut args = ssh_base(name);
    if tty {
        // `-tt` forces allocation even when wtx itself runs under another PTY runner.
        // SSH forwards window resizing and signals, so no custom PTY implementation is needed.
        args.push("-tt".into());
    }
    args.push(format!("lima-{name}"));
    args.push("--".into());
    args.push(remote);
    let st = Command::new("ssh").args(&args).status()?;
    std::process::exit(st.code().unwrap_or(1));
}

/// Check whether dockerd accepts commands without causing side effects.
pub fn docker_ready(name: &str) -> bool {
    capture(name, "docker info >/dev/null 2>&1 && printf '%s' ready")
        .is_ok_and(|out| out == "ready")
}

/// Wait for dockerd readiness in a newly started VM. The orchestrator controls the timeout.
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

fn forward_socket_path(name: &str, bound_port: u16) -> PathBuf {
    wtx_home().join(format!("{name}-{bound_port}.sock"))
}

/// Match only `<name>-<port>.sock` so prefix-related VM names such as `api` and `api-dev`
/// cannot be mistaken for one another.
fn forward_socket_port(name: &str, path: &Path) -> Option<u16> {
    path.file_name()?
        .to_str()?
        .strip_prefix(&format!("{name}-"))?
        .strip_suffix(".sock")?
        .parse()
        .ok()
}

fn close_control_socket(name: &str, socket: &Path) {
    let _ = Command::new("ssh")
        .args([
            "-S",
            &socket.to_string_lossy(),
            "-O",
            "exit",
            &format!("lima-{name}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(socket);
}

/// forward: host HOST -> VM GUEST (`ssh -L`)
/// bridge: VM GUEST -> host HOST (`ssh -R`). CLI arguments are always ordered HOST:GUEST.
pub fn forward(name: &str, spec: &str, reverse: bool) -> Result<()> {
    forward_impl(name, spec, reverse, false)
}

fn forward_impl(name: &str, spec: &str, reverse: bool, quiet: bool) -> Result<()> {
    let spec = parse_port_spec(spec)?;
    let bound_port = if reverse { spec.guest } else { spec.host };
    let sock = forward_socket_path(name, bound_port);
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

/// Check whether a forward's SSH master is alive with `-O check`.
/// Stopping a VM normally terminates the master and removes its socket, but an abnormal exit
/// can leave one behind.
pub fn master_alive(name: &str, host_port: u16) -> bool {
    let sock = forward_socket_path(name, host_port);
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
        .is_ok_and(|s| s.success())
}

/// Idempotently re-create a dead forward without writing to stdout, as required by `wtx env`.
pub fn ensure_forward(name: &str, host: u16, guest: u16) -> Result<()> {
    if master_alive(name, host) {
        return Ok(());
    }
    drop_forward(name, host); // Remove a stale socket, if present.
    forward_impl(name, &format!("{host}:{guest}"), false, true)
}

/// Stop a forward without output. Do nothing when its socket does not exist.
pub fn drop_forward(name: &str, host_port: u16) {
    close_control_socket(name, &forward_socket_path(name, host_port));
}

pub fn unforward(name: &str, port: &str) -> Result<()> {
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow!("invalid bound port: {port}"))?;
    if port == 0 {
        return Err(anyhow!("port must be between 1 and 65535"));
    }
    close_control_socket(name, &forward_socket_path(name, port));
    println!("stopped");
    Ok(())
}

pub fn close_all_forwards(name: &str) {
    if let Ok(rd) = std::fs::read_dir(wtx_home()) {
        for e in rd.flatten() {
            let p = e.path();
            if forward_socket_port(name, &p).is_some() {
                close_control_socket(name, &p);
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

    #[test]
    fn forward_socket_requires_an_exact_vm_name_and_numeric_port() {
        assert_eq!(
            forward_socket_port("api", Path::new("api-42000.sock")),
            Some(42000)
        );
        assert_eq!(
            forward_socket_port("api-dev", Path::new("api-dev-42000.sock")),
            Some(42000)
        );
        assert_eq!(
            forward_socket_port("api", Path::new("api-dev-42000.sock")),
            None
        );
        assert_eq!(forward_socket_port("api", Path::new("api-bad.sock")), None);
    }
}
