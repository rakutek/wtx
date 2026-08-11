//! Resolve wtx instances from explicit names or the current worktree.

use crate::lima::{self, InstanceMeta};
use crate::util::wtx_home;
use anyhow::{anyhow, Result};

/// Read all instance metadata under `~/.wtx`; unrelated JSON files are ignored.
pub(crate) fn all_metas() -> Vec<(String, InstanceMeta)> {
    let Ok(entries) = std::fs::read_dir(wtx_home()) else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            let meta = lima::load_meta(&name)?;
            if meta.workdir.is_empty() {
                return None;
            }
            Some((name, meta))
        })
        .collect()
}

/// Find VMs whose workdir contains the current directory, using the longest metadata
/// workdir prefix. Return all tied matches so callers can report ambiguity.
pub fn covering_cwd() -> Result<Vec<(String, InstanceMeta)>> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let cwd = cwd.to_string_lossy().into_owned();
    let mut best: Vec<(String, InstanceMeta)> = vec![];
    let mut best_len = 0usize;
    for (name, meta) in all_metas() {
        let workdir = meta.workdir.clone();
        if cwd != workdir && !cwd.starts_with(&format!("{workdir}/")) {
            continue;
        }
        if workdir.len() > best_len {
            best_len = workdir.len();
            best = vec![(name, meta)];
        } else if workdir.len() == best_len {
            best.push((name, meta));
        }
    }
    Ok(best)
}

/// Resolve an omitted VM name from the current directory. If multiple VMs have the same
/// workdir, list the candidates and fail instead of guessing.
pub fn resolve(explicit: Option<&str>) -> Result<(String, InstanceMeta)> {
    if let Some(name) = explicit {
        let meta = lima::load_meta(name)
            .ok_or_else(|| anyhow!("no metadata for {name} (is it a wtx VM?)"))?;
        return Ok((name.to_string(), meta));
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
                .map(|(name, _)| name.as_str())
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
