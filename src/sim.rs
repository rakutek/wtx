//! worktree 連動の iOS シミュレータ管理（設計は docs/DESIGN-sim.md）。
//! wtx が持つのはデバイスの寿命（VM連動）とポート配線・環境変数の出力まで。
//! 操作（tap 等）は orca emulator / simctl に任せ、契約は UDID と WTX_* 環境変数だけにする。
//! シミュレータはホスト側にしか存在できない（CoreSimulator は macOS の Xcode に属する）。
use crate::lima::{self, InstanceMeta, PortMap};
use crate::sshx;
use crate::util::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;

fn xcrun(args: &[&str]) -> Result<String> {
    let out = Command::new("xcrun").args(args).output().map_err(|_| {
        anyhow!("xcrun not found (simulator support needs Xcode command line tools)")
    })?;
    if !out.status.success() {
        return Err(anyhow!(
            "xcrun {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn simctl_json(kind: &str) -> Result<Value> {
    Ok(serde_json::from_str(&xcrun(&[
        "simctl", "list", "-j", kind,
    ])?)?)
}

/// ~/.wtx の全メタデータ（mirrors.json 等は workdir を持たないので自然に除外される）。
fn all_metas() -> Vec<(String, InstanceMeta)> {
    let Ok(rd) = std::fs::read_dir(wtx_home()) else {
        return vec![];
    };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                return None;
            }
            let name = p.file_stem()?.to_str()?.to_string();
            let meta = lima::load_meta(&name)?;
            if meta.workdir.is_empty() {
                return None;
            }
            Some((name, meta))
        })
        .collect()
}

/// カレントディレクトリを workdir に含むVM（メタデータ workdir の最長前方一致）。
/// 同率で複数マッチした場合は全候補を返す（呼び出し側が推測せずエラーにする）。
pub fn covering_cwd() -> Result<Vec<(String, InstanceMeta)>> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let cwd = cwd.to_string_lossy().into_owned();
    let mut best: Vec<(String, InstanceMeta)> = vec![];
    let mut best_len = 0usize;
    for (name, meta) in all_metas() {
        let wd = meta.workdir.clone();
        if cwd != wd && !cwd.starts_with(&format!("{wd}/")) {
            continue;
        }
        if wd.len() > best_len {
            best_len = wd.len();
            best = vec![(name, meta)];
        } else if wd.len() == best_len {
            best.push((name, meta));
        }
    }
    Ok(best)
}

/// NAME 省略時にカレントディレクトリから VM を解決する。
/// 同じ workdir を持つVMが複数ある場合は推測せず候補を列挙してエラーにする。
pub fn resolve(explicit: Option<&str>) -> Result<(String, InstanceMeta)> {
    if let Some(n) = explicit {
        let meta =
            lima::load_meta(n).ok_or_else(|| anyhow!("no metadata for {n} (is it a wtx VM?)"))?;
        return Ok((n.to_string(), meta));
    }
    let mut best = covering_cwd()?;
    match best.len() {
        0 => Err(anyhow!(
            "no wtx VM covers {} (run inside a worktree that has one, or pass NAME)",
            std::env::current_dir()?.display()
        )),
        1 => Ok(best.remove(0)),
        _ => Err(anyhow!(
            "multiple VMs cover this directory: {} (pass NAME)",
            best.iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn which() -> Result<()> {
    let (name, _) = resolve(None)?;
    println!("{name}");
    Ok(())
}

/// 最新の iOS runtime（isAvailable なもののうちバージョン最大）。
fn newest_ios_runtime() -> Result<Value> {
    let j = simctl_json("runtimes")?;
    j["runtimes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r["isAvailable"].as_bool() == Some(true)
                && r["identifier"]
                    .as_str()
                    .unwrap_or("")
                    .contains("SimRuntime.iOS")
        })
        .max_by_key(|r| {
            let mut it = r["version"]
                .as_str()
                .unwrap_or("0")
                .split('.')
                .map(|x| x.parse::<u64>().unwrap_or(0));
            (
                it.next().unwrap_or(0),
                it.next().unwrap_or(0),
                it.next().unwrap_or(0),
            )
        })
        .ok_or_else(|| anyhow!("no iOS simulator runtime installed (install one via Xcode)"))
}

/// 既定のデバイス種別: runtime が対応する iPhone のうち、カタログの minRuntimeVersion が
/// 最大のもの（= 最新機種）。supportedDeviceTypes の並び順には依存しない。
fn default_device(runtime: &Value) -> Result<(String, String)> {
    let cat = simctl_json("devicetypes")?;
    let minver: std::collections::HashMap<String, i64> = cat["devicetypes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            Some((
                t["identifier"].as_str()?.to_string(),
                t["minRuntimeVersion"].as_i64().unwrap_or(0),
            ))
        })
        .collect();
    let types = runtime["supportedDeviceTypes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let best = types
        .iter()
        .filter(|t| t["productFamily"].as_str() == Some("iPhone"))
        .max_by_key(|t| {
            t["identifier"]
                .as_str()
                .and_then(|i| minver.get(i))
                .copied()
                .unwrap_or(0)
        })
        .ok_or_else(|| anyhow!("no iPhone device type for the newest runtime"))?;
    Ok((
        best["identifier"].as_str().unwrap_or_default().to_string(),
        best["name"].as_str().unwrap_or_default().to_string(),
    ))
}

/// UDID → (state, runtime識別子)。デバイスが存在しなければ None。
fn device_state(udid: &str) -> Result<Option<(String, String)>> {
    let j = simctl_json("devices")?;
    if let Some(map) = j["devices"].as_object() {
        for (rt, devs) in map {
            for d in devs.as_array().cloned().unwrap_or_default() {
                if d["udid"].as_str() == Some(udid) {
                    return Ok(Some((
                        d["state"].as_str().unwrap_or("?").to_string(),
                        rt.clone(),
                    )));
                }
            }
        }
    }
    Ok(None)
}

/// `wtx ls` 用: UDID 集合の状態をまとめて引く。simctl が使えない環境では空を返す
/// （sim_udid を持つVMが無ければ呼び出し側が空リストを渡すので xcrun には触れない）。
pub fn states_for(udids: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if udids.is_empty() {
        return out;
    }
    let Ok(j) = simctl_json("devices") else {
        return out;
    };
    if let Some(map) = j["devices"].as_object() {
        for devs in map.values() {
            for d in devs.as_array().cloned().unwrap_or_default() {
                if let (Some(u), Some(s)) = (d["udid"].as_str(), d["state"].as_str()) {
                    if udids.iter().any(|x| x == u) {
                        out.insert(u.to_string(), s.to_string());
                    }
                }
            }
        }
    }
    out
}

/// デバイスが未作成（または消失）なら作る（冪等）。meta の保存は呼び出し側が行う。
pub fn ensure_device(name: &str, meta: &mut InstanceMeta, device: Option<&str>) -> Result<()> {
    ensure_device_with_output(name, meta, device, true)
}

pub fn ensure_device_with_output(
    name: &str,
    meta: &mut InstanceMeta,
    device: Option<&str>,
    print_progress: bool,
) -> Result<()> {
    if !meta.sim_udid.is_empty() {
        if device_state(&meta.sim_udid)?.is_some() {
            if print_progress {
                println!("simulator ready: wtx-{name} ({})", meta.sim_udid);
            }
            return Ok(());
        }
        eprintln!(
            "wtx: recorded simulator {} is gone; creating a new one",
            meta.sim_udid
        );
    }
    let rt = newest_ios_runtime()?;
    let rt_id = rt["identifier"].as_str().unwrap_or_default().to_string();
    let (dt_id, dt_name) = match device {
        Some(d) => (d.to_string(), d.to_string()),
        None => default_device(&rt)?,
    };
    // runtime を明示すると出力は UDID 1行になる（省略すると案内行が混ざる。実測）
    let out = xcrun(&["simctl", "create", &format!("wtx-{name}"), &dt_id, &rt_id])?;
    let udid = out.lines().last().unwrap_or_default().trim().to_string();
    meta.sim_udid = udid.clone();
    meta.sim_devicetype = dt_name.clone();
    if print_progress {
        println!("created simulator wtx-{name} ({dt_name}, {udid})");
    }
    Ok(())
}

/// `wtx up --from` 用: clone 元のデバイスを複製する（インストール済みアプリ・データごと）。
/// simctl clone は Shutdown が必須（実測: Booted は SimError 405）なので、
/// VM clone と同じく「止めて写して戻す」。
pub fn clone_device_with_output(
    src_udid: &str,
    new_vm: &str,
    print_progress: bool,
) -> Result<String> {
    let was_booted = matches!(device_state(src_udid)?, Some((s, _)) if s == "Booted");
    if was_booted {
        if print_progress {
            println!("shutting down the source simulator for a consistent copy...");
        }
        let _ = xcrun(&["simctl", "shutdown", src_udid]);
    }
    let out = xcrun(&["simctl", "clone", src_udid, &format!("wtx-{new_vm}")])?;
    let udid = out.lines().last().unwrap_or_default().trim().to_string();
    if was_booted {
        let _ = xcrun(&["simctl", "boot", src_udid]); // 復帰はバックグラウンドで進む
    }
    if print_progress {
        println!("cloned simulator -> wtx-{new_vm} ({udid})");
    }
    Ok(udid)
}

/// VM 削除時・`sim rm` 時のデバイス削除（ベストエフォート）。Booted でも落としてから消す。
pub fn delete_device(udid: &str) {
    if udid.is_empty() {
        return;
    }
    let _ = xcrun(&["simctl", "shutdown", udid]);
    match xcrun(&["simctl", "delete", udid]) {
        Ok(_) => println!("deleted simulator {udid}"),
        Err(e) => eprintln!("wtx: could not delete simulator {udid}: {e}"),
    }
}

pub fn up(name: Option<&str>, device: Option<&str>) -> Result<()> {
    let (name, mut meta) = resolve(name)?;
    ensure_device(&name, &mut meta, device)?;
    lima::save_meta(&name, &meta)
}

pub fn rm(name: Option<&str>) -> Result<()> {
    let (name, mut meta) = resolve(name)?;
    if meta.sim_udid.is_empty() {
        println!("no simulator recorded for {name}");
        return Ok(());
    }
    delete_device(&meta.sim_udid);
    meta.sim_udid.clear();
    meta.sim_devicetype.clear();
    lima::save_meta(&name, &meta)
}

/// ホストポートの払い出し。全VMのメタデータに記録済みの値を避け、実際に bind
/// できることも確かめる（名前ハッシュ方式は衝突に気付けないので採らない。DESIGN-sim.md）。
const PORT_LO: u16 = 42000;
const PORT_HI: u16 = 42999;

fn alloc_host_port(extra_used: &BTreeMap<String, PortMap>) -> Result<u16> {
    let mut used: std::collections::HashSet<u16> = all_metas()
        .iter()
        .flat_map(|(_, m)| m.ports.values().map(|p| p.host))
        .collect();
    used.extend(extra_used.values().map(|p| p.host));
    for p in PORT_LO..=PORT_HI {
        if used.contains(&p) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return Ok(p);
        }
    }
    Err(anyhow!("no free host port in {PORT_LO}-{PORT_HI}"))
}

/// `--from` 用: label:guest の定義を引き継ぎ、ホストポートは新規に払い出す。
pub fn inherit_ports(src: &BTreeMap<String, PortMap>) -> BTreeMap<String, PortMap> {
    let mut out = BTreeMap::new();
    for (label, p) in src {
        match alloc_host_port(&out) {
            Ok(host) => {
                out.insert(
                    label.clone(),
                    PortMap {
                        host,
                        guest: p.guest,
                    },
                );
            }
            Err(e) => eprintln!("wtx: warning: port {label} not inherited: {e}"),
        }
    }
    out
}

/// ラベル → 環境変数名（英数以外は `_`、大文字化）。
fn env_key(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub fn wire(name: Option<&str>, spec: &str) -> Result<()> {
    let (name, mut meta) = resolve(name)?;
    let (label, guest) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("spec must be LABEL:GUESTPORT (e.g. api:3000)"))?;
    let guest: u16 = guest
        .parse()
        .map_err(|_| anyhow!("bad guest port in {spec}"))?;
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!("label must be alphanumeric/-/_ : {label}"));
    }
    let host = match meta.ports.get(label) {
        Some(p) if p.guest == guest => p.host,
        Some(p) => {
            // guest 側の変更: 割り当て済みホストポートは維持し、古い forward だけ畳む
            sshx::drop_forward(&name, p.host);
            p.host
        }
        None => alloc_host_port(&meta.ports)?,
    };
    meta.ports
        .insert(label.to_string(), PortMap { host, guest });
    lima::save_meta(&name, &meta)?;
    sshx::ensure_forward(&name, host, guest)?;
    println!(
        "{label}: host {host} -> guest {guest} (WTX_PORT_{}={host})",
        env_key(label)
    );
    Ok(())
}

pub fn env(name: Option<&str>, json: bool) -> Result<()> {
    let (name, meta) = resolve(name)?;
    // 再arm: VM 停止をまたぐと ssh マスターは自然消滅している（実測）。
    // VM が止まっているなど張れない場合は警告して出力は続ける。
    for (label, p) in &meta.ports {
        if let Err(e) = sshx::ensure_forward(&name, p.host, p.guest) {
            eprintln!("wtx: warning: forward {label} not armed: {e}");
        }
    }
    if json {
        let ports: serde_json::Map<String, Value> = meta
            .ports
            .iter()
            .map(|(l, p)| {
                (
                    l.clone(),
                    serde_json::json!({"host": p.host, "guest": p.guest}),
                )
            })
            .collect();
        let j = serde_json::json!({
            "vm": name,
            "workdir": meta.workdir,
            "sim_udid": meta.sim_udid,
            "sim_devicetype": meta.sim_devicetype,
            "ports": ports,
        });
        println!("{}", serde_json::to_string_pretty(&j)?);
        return Ok(());
    }
    println!("export WTX_VM_NAME={}", shq(&name));
    println!("export WTX_WORKDIR={}", shq(&meta.workdir));
    if !meta.sim_udid.is_empty() {
        println!("export WTX_SIM_UDID={}", shq(&meta.sim_udid));
        println!("export WTX_SIM_DEVICETYPE={}", shq(&meta.sim_devicetype));
    }
    for (label, p) in &meta.ports {
        println!("export WTX_PORT_{}={}", env_key(label), p.host);
    }
    Ok(())
}

pub fn status(name: Option<&str>, json: bool) -> Result<()> {
    let (name, meta) = resolve(name)?;
    let dev = if meta.sim_udid.is_empty() {
        None
    } else {
        device_state(&meta.sim_udid)?
    };
    if json {
        let ports: serde_json::Map<String, Value> = meta
            .ports
            .iter()
            .map(|(l, p)| {
                (
                    l.clone(),
                    serde_json::json!({
                        "host": p.host,
                        "guest": p.guest,
                        "forward_alive": sshx::master_alive(&name, p.host),
                    }),
                )
            })
            .collect();
        let j = serde_json::json!({
            "vm": name,
            "sim_udid": meta.sim_udid,
            "sim_devicetype": meta.sim_devicetype,
            "sim_state": dev.as_ref().map(|(s, _)| s.clone()),
            "sim_runtime": dev.as_ref().map(|(_, r)| r.clone()),
            "ports": ports,
        });
        println!("{}", serde_json::to_string_pretty(&j)?);
        return Ok(());
    }
    if meta.sim_udid.is_empty() {
        println!("{name}: no simulator (create one with `wtx sim up`)");
    } else {
        match dev {
            Some((state, rt)) => {
                println!("{name}: wtx-{name} ({}) {state} [{rt}]", meta.sim_udid)
            }
            None => println!(
                "{name}: recorded simulator {} is missing (recreate with `wtx sim up`)",
                meta.sim_udid
            ),
        }
    }
    for (label, p) in &meta.ports {
        let alive = if sshx::master_alive(&name, p.host) {
            "armed"
        } else {
            "down"
        };
        println!("  {label}: host {} -> guest {} [{alive}]", p.host, p.guest);
    }
    Ok(())
}
