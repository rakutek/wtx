//! Named host-port allocation for services running inside a wtx VM.

use crate::context;
use crate::lima::{self, PortMap};
use crate::sshx;
use crate::util::shq;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::BTreeMap;

const PORT_LO: u16 = 42000;
const PORT_HI: u16 = 42999;

/// Allocate a host port, avoiding values recorded by any VM and confirming that the port can
/// actually be bound. A name-hash scheme would not detect collisions.
fn alloc_host_port(extra_used: &BTreeMap<String, PortMap>) -> Result<u16> {
    let mut used: std::collections::HashSet<u16> = context::all_metas()
        .iter()
        .flat_map(|(_, meta)| meta.ports.values().map(|port| port.host))
        .collect();
    used.extend(extra_used.values().map(|port| port.host));
    for port in PORT_LO..=PORT_HI {
        if used.contains(&port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(anyhow!("no free host port in {PORT_LO}-{PORT_HI}"))
}

/// For `--from`, retain label-to-guest definitions while allocating new host ports.
pub fn inherit_ports(src: &BTreeMap<String, PortMap>) -> BTreeMap<String, PortMap> {
    let mut out = BTreeMap::new();
    for (label, port) in src {
        match alloc_host_port(&out) {
            Ok(host) => {
                out.insert(
                    label.clone(),
                    PortMap {
                        host,
                        guest: port.guest,
                    },
                );
            }
            Err(error) => eprintln!("wtx: warning: port {label} not inherited: {error}"),
        }
    }
    out
}

/// Convert a label to an uppercase environment variable name, replacing non-alphanumerics
/// with `_`.
fn env_key(label: &str) -> String {
    label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_spec(spec: &str) -> Result<(&str, u16)> {
    let (label, guest) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("spec must be LABEL:GUESTPORT (e.g. api:3000)"))?;
    let guest: u16 = guest
        .parse()
        .map_err(|_| anyhow!("bad guest port in {spec}"))?;
    if guest == 0 {
        return Err(anyhow!("guest port must be between 1 and 65535"));
    }
    if label.is_empty()
        || !label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(anyhow!("label must be alphanumeric/-/_ : {label}"));
    }
    Ok((label, guest))
}

/// Add or update a named host-to-guest port mapping and arm its SSH forward.
pub fn add(name: Option<&str>, spec: &str) -> Result<()> {
    let (name, mut meta) = context::resolve(name)?;
    let (label, guest) = parse_spec(spec)?;
    let host = match meta.ports.get(label) {
        Some(port) if port.guest == guest => port.host,
        Some(port) => {
            // When the guest port changes, preserve the allocated host port and stop only
            // the stale forward.
            sshx::drop_forward(&name, port.host);
            port.host
        }
        None => alloc_host_port(&meta.ports)?,
    };
    meta.ports
        .insert(label.to_string(), PortMap { host, guest });
    lima::save_meta(&name, &meta)?;
    sshx::ensure_forward(&name, host, guest)?;
    println!(
        "{label}: 127.0.0.1:{host} -> guest {guest} (WTX_PORT_{}={host})",
        env_key(label)
    );
    Ok(())
}

/// Print shell or JSON environment data and re-arm recorded forwards after VM restarts.
pub fn env(name: Option<&str>, json: bool) -> Result<()> {
    let (name, meta) = context::resolve(name)?;
    for (label, port) in &meta.ports {
        if let Err(error) = sshx::ensure_forward(&name, port.host, port.guest) {
            eprintln!("wtx: warning: forward {label} not armed: {error}");
        }
    }
    if json {
        let ports: serde_json::Map<String, Value> = meta
            .ports
            .iter()
            .map(|(label, port)| {
                (
                    label.clone(),
                    serde_json::json!({"host": port.host, "guest": port.guest}),
                )
            })
            .collect();
        let value = serde_json::json!({
            "vm": name,
            "workdir": meta.workdir,
            "sim_udid": meta.sim_udid,
            "sim_devicetype": meta.sim_devicetype,
            "ports": ports,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!("export WTX_VM_NAME={}", shq(&name));
    println!("export WTX_WORKDIR={}", shq(&meta.workdir));
    if !meta.sim_udid.is_empty() {
        println!("export WTX_SIM_UDID={}", shq(&meta.sim_udid));
        println!("export WTX_SIM_DEVICETYPE={}", shq(&meta.sim_devicetype));
    }
    for (label, port) in &meta.ports {
        println!("export WTX_PORT_{}={}", env_key(label), port.host);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_spec_accepts_labels_and_rejects_invalid_ports() {
        assert_eq!(parse_spec("web-api:3000").unwrap(), ("web-api", 3000));
        assert!(parse_spec("api:0").is_err());
        assert!(parse_spec("api:not-a-port").is_err());
        assert!(parse_spec("bad.label:3000").is_err());
    }

    #[test]
    fn labels_map_to_stable_environment_keys() {
        assert_eq!(env_key("web-api"), "WEB_API");
        assert_eq!(env_key("db_2"), "DB_2");
    }
}
