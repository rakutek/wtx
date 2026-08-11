//! Worktree-aware iOS simulator management. See docs/DESIGN-sim.md for the design.
//! wtx manages the VM-linked device lifecycle; generic port wiring lives in `port`.
//! Interactions such as taps belong to Orca emulator or simctl; the contract consists only
//! of the UDID and `WTX_*` environment variables. Simulators run on the host because
//! CoreSimulator is part of Xcode on macOS.
use crate::context;
use crate::lima::{self, InstanceMeta};
use crate::sshx;
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

/// Return the newest available iOS runtime by version.
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

/// Choose the default device type from the iPhones supported by the runtime, selecting the
/// highest catalog `minRuntimeVersion` as the newest model without relying on the order of
/// `supportedDeviceTypes`.
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

/// Map a UDID to its state and runtime identifier, or return None if the device is absent.
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

/// Fetch states for a set of UDIDs for `wtx ls`. Return an empty map when simctl is
/// unavailable. The caller passes an empty list when no VM has a `sim_udid`, avoiding an
/// unnecessary `xcrun` call.
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

/// Idempotently create a missing device. The caller is responsible for saving metadata.
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
    // Specifying the runtime yields a single UDID line; omitting it adds informational output.
    let out = xcrun(&["simctl", "create", &format!("wtx-{name}"), &dt_id, &rt_id])?;
    let udid = out.lines().last().unwrap_or_default().trim().to_string();
    meta.sim_udid = udid;
    meta.sim_devicetype = dt_name;
    if print_progress {
        println!(
            "created simulator wtx-{name} ({}, {})",
            meta.sim_devicetype, meta.sim_udid
        );
    }
    Ok(())
}

/// Clone the source device for `wtx up --from`, including installed applications and data.
/// `simctl clone` requires the source to be shut down, so stop it, clone it, and restore it,
/// mirroring the VM clone flow.
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
        let _ = xcrun(&["simctl", "boot", src_udid]); // Boot restoration continues in the background.
    }
    if print_progress {
        println!("cloned simulator -> wtx-{new_vm} ({udid})");
    }
    Ok(udid)
}

/// Best-effort device deletion for VM removal and `sim rm`. Shut down booted devices first.
pub fn delete_device(udid: &str) {
    delete_device_with_output(udid, true);
}

pub fn delete_device_quietly(udid: &str) {
    delete_device_with_output(udid, false);
}

fn delete_device_with_output(udid: &str, print_progress: bool) {
    if udid.is_empty() {
        return;
    }
    let _ = xcrun(&["simctl", "shutdown", udid]);
    match xcrun(&["simctl", "delete", udid]) {
        Ok(_) if print_progress => println!("deleted simulator {udid}"),
        Ok(_) => {}
        Err(e) => eprintln!("wtx: could not delete simulator {udid}: {e}"),
    }
}

/// Shut down a device when its VM stops while retaining the device and metadata for reuse.
pub fn shutdown_device(udid: &str) -> Result<()> {
    if udid.is_empty() {
        return Ok(());
    }
    if matches!(device_state(udid)?, Some((state, _)) if state == "Booted") {
        xcrun(&["simctl", "shutdown", udid])?;
    }
    Ok(())
}

pub fn up(name: Option<&str>, device: Option<&str>) -> Result<()> {
    let (name, mut meta) = context::resolve(name)?;
    ensure_device(&name, &mut meta, device)?;
    lima::save_meta(&name, &meta)
}

pub fn rm(name: Option<&str>) -> Result<()> {
    let (name, mut meta) = context::resolve(name)?;
    if meta.sim_udid.is_empty() {
        println!("no simulator recorded for {name}");
        return Ok(());
    }
    delete_device(&meta.sim_udid);
    meta.sim_udid.clear();
    meta.sim_devicetype.clear();
    lima::save_meta(&name, &meta)
}

pub fn status(name: Option<&str>, json: bool) -> Result<()> {
    let (name, meta) = context::resolve(name)?;
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
                println!("{name}: wtx-{name} ({}) {state} [{rt}]", meta.sim_udid);
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
