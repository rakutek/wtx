use crate::mirror;
use crate::repo::{self, RepoInfo, RepoKind};
use crate::util::{
    git_config_global, git_out, git_run, lima_dir, lima_status, lima_status_checked, limactl,
    limactl_capture, limactl_out, pad, shq, wtx_home,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const GOLDEN: &str = "wtx-golden";
const PROVISION_SCHEMA_VERSION: u32 = 2;
const PROVISION_DOCKER_VERSION: &str = "29.7.2";
const TEMPLATE: &str = include_str!("../templates/vm.yaml.tmpl");

#[derive(Debug, Clone)]
pub struct Mount {
    pub location: String,
    pub writable: bool,
}

/// Record decisions made by `wtx up` for use by rm and the TUI.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceMeta {
    pub workdir: String,
    #[serde(default)]
    pub main_repo: String,
    #[serde(default)]
    pub branch: String,
    /// Source cloned by `wtx up --from`, or empty for regular creation.
    #[serde(default)]
    pub seeded_from: String,
    /// UDID of the worktree-specific simulator, or empty if none has been created.
    #[serde(default)]
    pub sim_udid: String,
    #[serde(default)]
    pub sim_devicetype: String,
    /// `wtx port add` mappings (label -> host/guest), used by `wtx env` to re-arm forwards.
    #[serde(default)]
    pub ports: std::collections::BTreeMap<String, PortMap>,
    /// Whether the VM was created with explicit access to `~/.claude` and the SSH agent.
    #[serde(default)]
    pub agent_access: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct GoldenReceipt {
    provision_schema_version: u32,
    wtx_version: String,
    docker_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMap {
    pub host: u16,
    pub guest: u16,
}

pub const RECEIPT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Serialize)]
struct WorktreeInspection {
    path: String,
    repo: String,
    branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    head: String,
    orphaned: bool,
}

#[derive(Debug, Serialize)]
struct RuntimeInspection {
    docker: &'static str,
}

#[derive(Debug, Serialize)]
struct PortInspection {
    host: u16,
    guest: u16,
    forward_alive: bool,
}

#[derive(Debug, Serialize)]
struct SimulatorInspection {
    udid: String,
    device_type: String,
    state: String,
}

#[derive(Debug, Serialize)]
struct InstanceInspection {
    name: String,
    status: String,
    ready: bool,
    runtime: RuntimeInspection,
    worktree: WorktreeInspection,
    #[serde(skip_serializing_if = "String::is_empty")]
    seeded_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    simulator: Option<SimulatorInspection>,
    ports: BTreeMap<String, PortInspection>,
    agent_access: bool,
}

#[derive(Debug, Serialize)]
struct InspectReceipt {
    schema_version: u32,
    instance: InstanceInspection,
}

#[derive(Debug, Serialize)]
struct EnsureReceipt {
    schema_version: u32,
    action: &'static str,
    instance: InstanceInspection,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RemoveAction {
    Deleted,
    NotFound,
}

#[derive(Debug, Serialize)]
struct RemoveReceipt<'a> {
    schema_version: u32,
    action: RemoveAction,
    name: &'a str,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOpts {
    pub with_worktree: bool,
    pub if_exists: bool,
    pub json: bool,
}

pub fn meta_path(name: &str) -> PathBuf {
    wtx_home().join(format!("{name}.json"))
}

pub fn load_meta(name: &str) -> Option<InstanceMeta> {
    serde_json::from_str(&std::fs::read_to_string(meta_path(name)).ok()?).ok()
}

pub fn save_meta(name: &str, meta: &InstanceMeta) -> Result<()> {
    std::fs::write(meta_path(name), serde_json::to_string_pretty(meta)?)?;
    Ok(())
}

pub struct UpOpts {
    pub memory: Option<String>,
    pub cpus: Option<u32>,
    pub disk: String,
    pub from: Option<String>,
    pub agent_access: bool,
    pub no_clone: bool,
    pub extra_mounts: Vec<String>,
    pub sim: bool,
    pub sim_device: Option<String>,
}

fn up_limactl(args: &[&str], quiet: bool) -> Result<()> {
    if quiet {
        limactl_capture(args).map_err(|e| anyhow!("limactl {}: {e}", args.join(" ")))
    } else {
        limactl(args)
    }
}

fn render_yaml(
    mounts: &[Mount],
    cpus: u32,
    memory: &str,
    disk: &str,
    agent_access: bool,
    path: &Path,
) -> Result<()> {
    let m: String = mounts
        .iter()
        .map(|m| {
            Ok(format!(
                "- location: {}\n  writable: {}\n",
                serde_json::to_string(&m.location)?,
                m.writable
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("");
    let yaml = TEMPLATE
        .replace("__CPUS__", &cpus.to_string())
        .replace("__MEMORY__", &serde_json::to_string(memory)?)
        .replace("__DISK__", &serde_json::to_string(disk)?)
        .replace("__MIRROR_PORT__", &mirror::mirror_port().to_string())
        .replace(
            "__FORWARD_AGENT__",
            if agent_access { "true" } else { "false" },
        )
        .replace("__MOUNTS__", m.trim_end());
    std::fs::write(path, yaml)?;
    Ok(())
}

fn golden_receipt_path() -> PathBuf {
    wtx_home().join("golden-receipt.json")
}

fn compatible_golden_receipt() -> Option<GoldenReceipt> {
    let receipt: GoldenReceipt =
        serde_json::from_str(&std::fs::read_to_string(golden_receipt_path()).ok()?).ok()?;
    golden_receipt_is_compatible(&receipt).then_some(receipt)
}

fn golden_receipt_is_compatible(receipt: &GoldenReceipt) -> bool {
    receipt.provision_schema_version == PROVISION_SCHEMA_VERSION
        && receipt.docker_version == PROVISION_DOCKER_VERSION
}

pub fn golden_usable() -> bool {
    lima_dir(GOLDEN).join("lima.yaml").exists()
        && lima_status(GOLDEN) == "Stopped"
        && compatible_golden_receipt().is_some()
}

fn golden_lock() -> Result<File> {
    let path = wtx_home().join("golden.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(anyhow!(
            "lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

fn image_build_inner(quiet: bool) -> Result<()> {
    if lima_dir(GOLDEN).exists() {
        return Err(anyhow!(
            "{GOLDEN} already exists (run `wtx image rm` to rebuild it)"
        ));
    }
    let yaml = wtx_home().join(format!("{GOLDEN}.yaml"));
    render_yaml(&[], 2, "4GiB", "20GiB", false, &yaml)?;
    if quiet {
        eprintln!("wtx: preparing the shared base VM...");
    } else {
        println!("Preparing the shared base VM...");
    }
    up_limactl(
        &[
            "start",
            "--name",
            GOLDEN,
            "--tty=false",
            &yaml.to_string_lossy(),
        ],
        quiet,
    )?;
    let docker_version =
        crate::sshx::capture(GOLDEN, "docker version --format '{{.Server.Version}}'")?;
    std::fs::write(
        golden_receipt_path(),
        serde_json::to_string_pretty(&GoldenReceipt {
            provision_schema_version: PROVISION_SCHEMA_VERSION,
            wtx_version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version,
        })?,
    )?;
    up_limactl(&["stop", GOLDEN], quiet)?; // Clone only from a stopped instance.
    if !quiet {
        println!("Shared base VM ready");
    }
    Ok(())
}

fn image_rm_inner(quiet: bool) -> Result<()> {
    up_limactl(&["delete", "-f", GOLDEN], quiet)?;
    let _ = std::fs::remove_file(wtx_home().join(format!("{GOLDEN}.yaml")));
    let _ = std::fs::remove_file(golden_receipt_path());
    Ok(())
}

pub fn image_build() -> Result<()> {
    let _lock = golden_lock()?;
    image_build_inner(false)
}

pub fn image_rm() -> Result<()> {
    let _lock = golden_lock()?;
    image_rm_inner(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoldenPreparation {
    Ready,
    Stop,
    Rebuild,
    Build,
}

fn golden_preparation(exists: bool, status: &str, compatible: bool) -> GoldenPreparation {
    match (exists, status, compatible) {
        (true, "Stopped", true) => GoldenPreparation::Ready,
        (true, "Running", true) => GoldenPreparation::Stop,
        (true, _, _) => GoldenPreparation::Rebuild,
        (false, _, _) => GoldenPreparation::Build,
    }
}

fn ensure_golden(quiet: bool) -> Result<File> {
    let lock = golden_lock()?;
    let exists = lima_dir(GOLDEN).exists();
    let status = lima_status(GOLDEN);
    let compatible = compatible_golden_receipt().is_some();

    match golden_preparation(exists, &status, compatible) {
        GoldenPreparation::Ready => return Ok(lock),
        GoldenPreparation::Stop => {
            if !quiet {
                println!("Stopping the shared base VM before cloning...");
            }
            up_limactl(&["stop", GOLDEN], quiet)?;
            return Ok(lock);
        }
        GoldenPreparation::Rebuild => {
            if quiet {
                eprintln!("wtx: refreshing an incompatible shared base VM...");
            } else {
                println!("Refreshing the shared base VM...");
            }
            image_rm_inner(quiet)?;
        }
        GoldenPreparation::Build => {}
    }

    image_build_inner(quiet)?;
    Ok(lock)
}

pub fn image_status() {
    if golden_usable() {
        let receipt = compatible_golden_receipt().unwrap();
        println!(
            "{GOLDEN}: ready (schema {}, Docker {}, built by wtx {})",
            receipt.provision_schema_version, receipt.docker_version, receipt.wtx_version
        );
    } else {
        let st = lima_status(GOLDEN);
        if st.is_empty() {
            println!("{GOLDEN}: not built (prepared automatically when first needed)");
        } else {
            let why = if compatible_golden_receipt().is_none() {
                "missing or stale build receipt"
            } else {
                "it must be stopped"
            };
            println!("{GOLDEN}: {st} - {why} (repaired automatically when next needed)");
        }
    }
}

/// Build arguments for cloning a stopped VM. The cloned `lima.yaml` is already resolved and
/// cannot be overwritten with a template, so replace mounts with `--mount-only`. Every mount
/// therefore uses the same absolute path as the host. Pass `--memory` and `--cpus` only when
/// explicitly set; otherwise inherit them from the source.
fn clone_args(src: &str, name: &str, o: &UpOpts, mounts: &[Mount]) -> Vec<String> {
    let mut args: Vec<String> = vec!["clone".into(), src.into(), name.into()];
    args.push("--set".into());
    args.push(format!(".ssh.forwardAgent={}", o.agent_access));
    if let Some(mem) = &o.memory {
        args.push("--memory".into());
        args.push(mem.trim_end_matches("GiB").to_string());
    }
    if let Some(c) = o.cpus {
        args.push("--cpus".into());
        args.push(c.to_string());
    }
    for m in mounts {
        args.push("--mount-only".into());
        args.push(if m.writable {
            format!("{}:w", m.location)
        } else {
            m.location.clone()
        });
    }
    args
}

/// Compute Docker Compose's default project name by lowercasing the directory name and
/// removing characters other than alphanumerics, `-`, and `_`. Compose uses it as a volume
/// name prefix, so moving to a differently named worktree changes volume names even with the
/// same Compose file.
fn compose_project_name(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    cleaned
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

fn renamed_volume(src_project: &str, dst_project: &str, volume: &str) -> Option<(String, String)> {
    if src_project.is_empty() || src_project == dst_project {
        return None;
    }
    let suffix = volume.strip_prefix(&format!("{src_project}_"))?;
    if suffix.is_empty() {
        return None;
    }
    Some((suffix.to_string(), format!("{dst_project}_{suffix}")))
}

/// Remove source-specific state from a VM cloned with `--from` and rename Compose volumes for
/// the new worktree's project name. Removing isolated Git state migrates VMs created by older
/// wtx versions: if the source retains a `.wtx-local` overlay, Git in the new VM would silently
/// keep using the source VM's local repository. Fail only when that overlay cannot be removed.
fn seed_cleanup(name: &str, src: &str, workdir: &Path, quiet: bool) -> Result<()> {
    let src_meta = load_meta(src);
    let old_gitdir = src_meta
        .as_ref()
        .filter(|m| !m.main_repo.is_empty())
        .map(|m| format!("{}/.git", m.main_repo))
        .unwrap_or_default();
    let src_project = src_meta
        .as_ref()
        .map(|m| compose_project_name(Path::new(&m.workdir)))
        .unwrap_or_default();
    let dst_project = compose_project_name(workdir);

    let script = format!(
        r#"set -eu
OLDGIT={oldgit}

# Remove isolated Git state created by older wtx versions when migrating a legacy VM.
# Stopping and deleting the unit is best effort, but fail if the overlay cannot be removed;
# otherwise Git would silently continue using the source VM's state.
sudo systemctl disable --now wtx-gitmount.service >/dev/null 2>&1 || true
sudo rm -f /etc/systemd/system/wtx-gitmount.service /usr/local/sbin/wtx-gitmount
sudo systemctl daemon-reload >/dev/null 2>&1 || true
if [ -n "$OLDGIT" ]; then
  n=0
  while [ -e "$OLDGIT/.wtx-local" ] && [ $n -lt 5 ]; do
    sudo umount "$OLDGIT" 2>/dev/null || true
    n=$((n+1))
  done
  if [ -e "$OLDGIT/.wtx-local" ]; then
    echo "wtx: could not remove the stale git overlay at $OLDGIT" >&2
    exit 1
  fi
fi
if mountpoint -q /run/wtx/base.git 2>/dev/null; then sudo umount /run/wtx/base.git || true; fi
sudo rm -rf /var/lib/wtx/git

# Docker containers are disposable because Compose recreates them. Preserve valuable volumes
# and images while removing containers and unused networks.
n=0
until docker info >/dev/null 2>&1; do
  n=$((n+1))
  if [ $n -ge 120 ]; then echo "wtx: dockerd did not come up in the seeded VM" >&2; exit 1; fi
  sleep 1
done
docker ps -aq | xargs -r docker rm -fv >/dev/null
docker network prune -f >/dev/null 2>&1 || true

# Compose volume names use <project-name>_ as a prefix. Rename source-project volumes because
# changing the worktree directory changes the referenced names. Projects with a fixed `name:`
# in the Compose file do not match this prefix and are skipped.
"#,
        oldgit = shq(&old_gitdir),
    );
    crate::sshx::vm_script_with_output(name, &script, None, !quiet)?;

    let volumes = crate::sshx::capture(name, "docker volume ls -q")?;
    for volume in volumes.lines() {
        let Some((suffix, renamed)) = renamed_volume(&src_project, &dst_project, volume) else {
            continue;
        };
        let rename_script = format!(
            r"set -eu
docker volume create \
  --label {} \
  --label {} \
  {} >/dev/null
sudo cp -a {}/. {}/
docker volume rm {} >/dev/null
printf 'wtx: volume %s -> %s\n' {} {}
",
            shq(&format!("com.docker.compose.project={dst_project}")),
            shq(&format!("com.docker.compose.volume={suffix}")),
            shq(&renamed),
            shq(&format!("/var/lib/docker/volumes/{volume}/_data")),
            shq(&format!("/var/lib/docker/volumes/{renamed}/_data")),
            shq(volume),
            shq(volume),
            shq(&renamed),
        );
        crate::sshx::vm_script_with_output(name, &rename_script, None, !quiet)?;
    }
    Ok(())
}

pub fn up(name: &str, workdir: &str, o: &UpOpts) -> Result<()> {
    up_inner(name, workdir, o, false)?;
    println!("ready:\n  wtx shell {name}\n  wtx rm {name}");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProvisionPath {
    Seed,
    Reattach,
    Golden,
    Fresh,
}

fn provision_path(status: &str, has_seed: bool, no_clone: bool) -> Result<ProvisionPath> {
    if has_seed && !status.is_empty() {
        return Err(anyhow!("--from can only seed a new VM"));
    }
    if has_seed {
        return Ok(ProvisionPath::Seed);
    }
    if !status.is_empty() {
        return Ok(ProvisionPath::Reattach);
    }
    if no_clone {
        return Ok(ProvisionPath::Fresh);
    }
    Ok(ProvisionPath::Golden)
}

fn validate_agent_access(existed: bool, requested: bool, previous_enabled: bool) -> Result<()> {
    if existed && requested && !previous_enabled {
        return Err(anyhow!(
            "existing VM was created without --agent-access; mount policy is immutable, so recreate the VM to opt in"
        ));
    }
    Ok(())
}

fn collect_mounts(
    workdir: &Path,
    repo: Option<&RepoInfo>,
    o: &UpOpts,
    host_claude: &Path,
) -> Result<Vec<Mount>> {
    let mut mounts = vec![Mount {
        location: workdir.to_string_lossy().into_owned(),
        writable: true,
    }];
    if let Some(repo) = repo.filter(|repo| repo.kind == RepoKind::Worktree) {
        // Mount the main `.git` separately because it is outside the workdir. The mount is
        // read-write, so commits made in the VM advance the host branch directly.
        mounts.push(Mount {
            location: repo.host_git.to_string_lossy().into_owned(),
            writable: true,
        });
    }

    // Do not share credentials by default. Require explicit opt-in for a trusted in-VM agent.
    if o.agent_access {
        if host_claude.is_dir() {
            mounts.push(Mount {
                location: host_claude.to_string_lossy().into_owned(),
                writable: true,
            });
        } else {
            eprintln!(
                "wtx: warning: --agent-access requested but {} was not found",
                host_claude.display()
            );
        }
    }

    for mount in &o.extra_mounts {
        let (location, writable) = mount
            .strip_suffix(":ro")
            .map_or((mount.as_str(), true), |location| (location, false));
        let location = std::fs::canonicalize(location)?
            .to_string_lossy()
            .into_owned();
        if mounts.iter().any(|mount| mount.location == location) {
            eprintln!("wtx: ignoring {location}: already mounted automatically");
            continue;
        }
        mounts.push(Mount { location, writable });
    }
    Ok(mounts)
}

fn warn_legacy_agent_policy(name: &str, existed: bool) {
    if existed
        && std::fs::read_to_string(meta_path(name))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|value| value.get("agent_access").cloned())
            .is_none()
    {
        eprintln!(
            "wtx: warning: {name} predates explicit credential policy and may still share host credentials; recreate it"
        );
    }
}

fn provision_instance(
    name: &str,
    workdir: &Path,
    o: &UpOpts,
    mounts: &[Mount],
    yaml: &Path,
    status: &str,
    quiet: bool,
) -> Result<ProvisionPath> {
    let path = provision_path(status, o.from.is_some(), o.no_clone)?;
    match path {
        ProvisionPath::Seed => {
            let src = o.from.as_deref().expect("seed path requires --from");
            // Clone an existing VM to retain database volumes, images, and installed tools.
            // Copy the stopped disk to produce a consistent at-rest snapshot.
            if src == name {
                return Err(anyhow!("--from {src}: cannot seed a VM from itself"));
            }
            let src_status = lima_status(src);
            if src_status.is_empty() {
                return Err(anyhow!("--from {src}: no such VM"));
            }
            let was_running = src_status == "Running";
            if was_running {
                if !quiet {
                    println!(
                        "stopping {src} for a consistent copy (it restarts in the background)..."
                    );
                }
                up_limactl(&["stop", src], quiet)?;
            }
            let args = clone_args(src, name, o, mounts);
            up_limactl(&args.iter().map(String::as_str).collect::<Vec<_>>(), quiet)?;
            if was_running {
                // Restart the source while starting the new VM so it remains stopped only for
                // the clone operation.
                let _ = std::process::Command::new("limactl")
                    .args(["start", "--tty=false", src])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            up_limactl(&["start", name, "--tty=false"], quiet)?;
            seed_cleanup(name, src, workdir, quiet)
        }
        ProvisionPath::Reattach => {
            // Reattach an existing instance; its mount configuration was fixed at creation time.
            if status != "Running" {
                up_limactl(&["start", name, "--tty=false"], quiet)?;
            }
            Ok(())
        }
        ProvisionPath::Golden => {
            let _golden_lock = ensure_golden(quiet)?;
            let args = clone_args(GOLDEN, name, o, mounts);
            up_limactl(&args.iter().map(String::as_str).collect::<Vec<_>>(), quiet)?;
            up_limactl(&["start", name, "--tty=false"], quiet)
        }
        ProvisionPath::Fresh => up_limactl(
            &[
                "start",
                "--name",
                name,
                "--tty=false",
                &yaml.to_string_lossy(),
            ],
            quiet,
        ),
    }?;
    Ok(path)
}

fn rebuild_meta(
    workdir: &Path,
    repo: Option<&RepoInfo>,
    o: &UpOpts,
    existed: bool,
    previous: InstanceMeta,
) -> InstanceMeta {
    // Preserve simulator and port assignments when reattaching because metadata is rewritten.
    let mut meta = InstanceMeta {
        workdir: workdir.to_string_lossy().into_owned(),
        seeded_from: o.from.clone().unwrap_or(previous.seeded_from),
        sim_udid: previous.sim_udid,
        sim_devicetype: previous.sim_devicetype,
        ports: previous.ports,
        agent_access: if existed {
            previous.agent_access
        } else {
            o.agent_access
        },
        ..Default::default()
    };
    if let Some(repo) = repo {
        meta.main_repo = repo.host_repo.to_string_lossy().into_owned();
        meta.branch.clone_from(&repo.branch);
    }
    meta
}

fn warn_legacy_git_overlay(name: &str, repo: Option<&RepoInfo>, reattached: bool) {
    let Some(repo) = repo.filter(|_| reattached) else {
        return;
    };
    // VMs created by older wtx versions can retain an isolated Git overlay, causing commits
    // to remain local to the VM instead of appearing on the host. Require recreation.
    let marker = format!(
        "test -e {}/.wtx-local && echo legacy || true",
        shq(&repo.host_git.to_string_lossy())
    );
    if let Ok(out) = crate::sshx::capture(name, &marker) {
        if out.contains("legacy") {
            eprintln!(
                "wtx: warning: {name} was created by an older wtx with isolated git; \
                 commits made inside it do NOT reach the host. Recreate it: wtx rm {name} && wtx up ..."
            );
        }
    }
}

fn link_agent_credentials(name: &str, meta: &InstanceMeta, host_claude: &Path) {
    // Mounts are fixed at creation. In an existing VM created without `--agent-access`, the
    // mount target is absent, so the script's `-d` guard skips symlink creation.
    if !meta.agent_access || !host_claude.is_dir() {
        return;
    }
    let script = format!(
        r#"set -eu
H={h}
[ -d "$H" ] || exit 0
if [ ! -L "$HOME/.claude" ]; then rm -rf "$HOME/.claude"; ln -s "$H" "$HOME/.claude"; fi"#,
        h = shq(&host_claude.to_string_lossy()),
    );
    if let Err(e) = crate::sshx::vm_script(name, &script, None) {
        eprintln!("wtx: warning: could not link ~/.claude in the VM: {e}");
    }
}

fn reconcile_simulator(name: &str, o: &UpOpts, quiet: bool, meta: &mut InstanceMeta) {
    // With `--from`, clone the source simulator and its applications and data when present.
    // Retain only label-to-guest port definitions and allocate new host ports, because source
    // and destination cannot use the same host port concurrently.
    if meta.sim_udid.is_empty() {
        if let Some(src_meta) = o.from.as_deref().and_then(load_meta) {
            if !src_meta.sim_udid.is_empty() {
                match crate::sim::clone_device_with_output(&src_meta.sim_udid, name, !quiet) {
                    Ok(udid) => {
                        meta.sim_udid = udid;
                        meta.sim_devicetype.clone_from(&src_meta.sim_devicetype);
                        meta.ports = crate::port::inherit_ports(&src_meta.ports);
                    }
                    Err(e) => eprintln!("wtx: warning: simulator not cloned: {e}"),
                }
            }
        }
    }
    if o.sim || o.sim_device.is_some() {
        if let Err(e) =
            crate::sim::ensure_device_with_output(name, meta, o.sim_device.as_deref(), !quiet)
        {
            eprintln!("wtx: warning: simulator not created: {e}");
        }
    }
}

fn restore_forwards(name: &str, ports: &BTreeMap<String, PortMap>) {
    for (label, port) in ports {
        if let Err(e) = crate::sshx::ensure_forward(name, port.host, port.guest) {
            eprintln!("wtx: warning: forward {label} not armed: {e}");
        }
    }
}

fn up_inner(name: &str, workdir: &str, o: &UpOpts, quiet: bool) -> Result<()> {
    let workdir = std::fs::canonicalize(workdir)?;
    if !workdir.is_dir() {
        return Err(anyhow!("workdir not found: {}", workdir.display()));
    }
    if !mirror::mirror_alive() {
        eprintln!("wtx: warning: mirror is down - pulls go straight upstream (wtx mirror up)");
    }

    let repo = repo::inspect_repo(&workdir)?;
    let host_claude = dirs::home_dir().unwrap_or_default().join(".claude");
    let mounts = collect_mounts(&workdir, repo.as_ref(), o, &host_claude)?;

    let yaml = wtx_home().join(format!("{name}.yaml"));
    render_yaml(
        &mounts,
        o.cpus.unwrap_or(2),
        o.memory.as_deref().unwrap_or("4GiB"),
        &o.disk,
        o.agent_access,
        &yaml,
    )?;

    let status = lima_status(name);
    let existed = !status.is_empty();
    let previous_meta = load_meta(name);
    warn_legacy_agent_policy(name, existed);
    validate_agent_access(
        existed,
        o.agent_access,
        previous_meta.as_ref().is_some_and(|meta| meta.agent_access),
    )
    .map_err(|error| anyhow!("{name}: {error}"))?;
    let path = provision_instance(name, &workdir, o, &mounts, &yaml, &status, quiet)?;

    let mut meta = rebuild_meta(
        &workdir,
        repo.as_ref(),
        o,
        existed,
        previous_meta.unwrap_or_default(),
    );
    warn_legacy_git_overlay(name, repo.as_ref(), path == ProvisionPath::Reattach);
    if let Err(e) = mirror::apply_to_vm(name) {
        eprintln!("wtx: warning: mirror config not applied: {e}");
    }
    apply_host_git_identity(name, quiet)?;
    link_agent_credentials(name, &meta, &host_claude);
    reconcile_simulator(name, o, quiet, &mut meta);
    save_meta(name, &meta)?;
    // Restore forwards lost during a VM stop when a regular `wtx up` reattaches it.
    restore_forwards(name, &meta.ports);
    Ok(())
}

/// Reconcile host-dependent configuration on fresh, clone, and restart paths instead of
/// baking it into the golden image. Values are shell-quoted into a script sent over SSH stdin,
/// so apostrophes and newlines are preserved safely.
fn apply_host_git_identity(name: &str, quiet: bool) -> Result<()> {
    let git_name = git_config_global("user.name", "wtx");
    let git_email = git_config_global("user.email", "wtx@localhost");
    let script = format!(
        "set -eu\ngit config --global --replace-all user.name {}\ngit config --global --replace-all user.email {}\n",
        shq(&git_name),
        shq(&git_email),
    );
    crate::sshx::vm_script_with_output(name, &script, None, !quiet)
}

/// Stop a VM's forwards and running simulator along with the VM. A simulator shutdown failure
/// does not block the VM stop, but interactive calls emit a warning.
pub fn stop(name: &str, quiet: bool) -> std::result::Result<(), String> {
    crate::sshx::close_all_forwards(name);
    if let Some(meta) = load_meta(name) {
        if !meta.sim_udid.is_empty() {
            if let Err(e) = crate::sim::shutdown_device(&meta.sim_udid) {
                if !quiet {
                    eprintln!(
                        "wtx: warning: simulator {} not shut down: {e}",
                        meta.sim_udid
                    );
                }
            }
        }
    }
    limactl_capture(&["stop", name])
}

/// Idempotently prepare a VM for an orchestrator. Do not reapply creation-only `--from` to an
/// existing VM; only verify that the requested source matches the recorded seed.
pub fn ensure(
    name: &str,
    workdir: &str,
    mut o: UpOpts,
    timeout_seconds: u64,
    json: bool,
) -> Result<()> {
    if timeout_seconds == 0 {
        return Err(anyhow!("--timeout-seconds must be greater than zero"));
    }
    let workdir = std::fs::canonicalize(workdir)?;
    ensure_name_matches(name, &workdir)?;

    let before = lima_status(name);
    let action = if before.is_empty() {
        "created"
    } else if before == "Running" {
        "reused"
    } else {
        "started"
    };

    if !before.is_empty() {
        let existing = load_meta(name).ok_or_else(|| {
            anyhow!("VM {name} exists but has no wtx metadata; refusing to adopt it implicitly")
        })?;
        if let Some(requested) = o.from.as_deref() {
            if existing.seeded_from != requested {
                let actual = if existing.seeded_from.is_empty() {
                    "<none>"
                } else {
                    &existing.seeded_from
                };
                return Err(anyhow!(
                    "VM {name} already exists with seeded_from={actual}; requested {requested}"
                ));
            }
        }
        // `--from` is creation-only. After validation, continue through the normal reattach path.
        o.from = None;
    }

    up_inner(name, &workdir.to_string_lossy(), &o, json)?;

    crate::sshx::wait_docker_ready(name, Duration::from_secs(timeout_seconds))?;
    let instance = inspect_instance(name)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&EnsureReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                action,
                instance,
            })?
        );
    } else {
        println!("{action}: {name} is ready");
        println!("  worktree: {}", instance.worktree.path);
        println!("  run: wtx shell {name}");
        println!("  remove: wtx rm {name}");
    }
    Ok(())
}

fn inspect_instance(name: &str) -> Result<InstanceInspection> {
    let meta =
        load_meta(name).ok_or_else(|| anyhow!("no metadata for {name} (is it a wtx VM?)"))?;
    let status = lima_status(name);
    let docker_ready = status == "Running" && crate::sshx::docker_ready(name);
    let orphaned = meta.workdir.is_empty() || !Path::new(&meta.workdir).exists();
    let head = if orphaned {
        String::new()
    } else {
        git_out(Path::new(&meta.workdir), &["rev-parse", "HEAD"])
    };

    let sim_states = if meta.sim_udid.is_empty() {
        BTreeMap::new()
    } else {
        crate::sim::states_for(std::slice::from_ref(&meta.sim_udid))
    };
    let simulator = if meta.sim_udid.is_empty() {
        None
    } else {
        Some(SimulatorInspection {
            state: sim_states
                .get(&meta.sim_udid)
                .cloned()
                .unwrap_or_else(|| "missing".to_string()),
            udid: meta.sim_udid.clone(),
            device_type: meta.sim_devicetype.clone(),
        })
    };
    let ports = meta
        .ports
        .iter()
        .map(|(label, port)| {
            (
                label.clone(),
                PortInspection {
                    host: port.host,
                    guest: port.guest,
                    forward_alive: crate::sshx::master_alive(name, port.host),
                },
            )
        })
        .collect();

    Ok(InstanceInspection {
        name: name.to_string(),
        status,
        ready: docker_ready,
        runtime: RuntimeInspection {
            docker: if docker_ready { "ready" } else { "unavailable" },
        },
        worktree: WorktreeInspection {
            path: meta.workdir,
            repo: meta.main_repo,
            branch: meta.branch,
            head,
            orphaned,
        },
        seeded_from: meta.seeded_from,
        simulator,
        ports,
        agent_access: meta.agent_access,
    })
}

pub fn inspect(name: Option<&str>, json: bool) -> Result<()> {
    let (name, _) = crate::context::resolve(name)?;
    let instance = inspect_instance(&name)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&InspectReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                instance,
            })?
        );
        return Ok(());
    }

    println!(
        "{}: {} ({})",
        instance.name,
        instance.status,
        if instance.ready { "ready" } else { "not ready" }
    );
    println!("  worktree: {}", instance.worktree.path);
    if !instance.worktree.branch.is_empty() {
        println!("  branch: {}", instance.worktree.branch);
    }
    println!("  docker: {}", instance.runtime.docker);
    println!(
        "  credentials: {}",
        if instance.agent_access {
            "host-shared (--agent-access)"
        } else {
            "host-only"
        }
    );
    if !instance.seeded_from.is_empty() {
        println!("  seeded from: {}", instance.seeded_from);
    }
    if let Some(sim) = &instance.simulator {
        println!(
            "  simulator: {} ({}) [{}]",
            sim.udid, sim.device_type, sim.state
        );
    }
    for (label, port) in &instance.ports {
        println!(
            "  port {label}: host {} -> guest {} [{}]",
            port.host,
            port.guest,
            if port.forward_alive { "armed" } else { "down" }
        );
    }
    Ok(())
}

/// Best-effort cleanup of GC-protection refs (`refs/wtx/keep/<name>/*`) created by older wtx
/// versions.
fn unpin_legacy_keep_refs(host_repo: &Path, name: &str) {
    let prefix = format!("refs/wtx/keep/{name}/");
    let out = git_out(host_repo, &["for-each-ref", "--format=%(refname)", &prefix]);
    for r in out.split_whitespace() {
        let _ = git_run(host_repo, &["update-ref", "-d", r]);
    }
}

pub fn rm(name: &str, opts: RemoveOpts) -> Result<()> {
    let action = remove_instance(name, opts)?;
    print_remove_result(name, action, opts.json)
}

fn remove_instance(name: &str, opts: RemoveOpts) -> Result<RemoveAction> {
    let meta = load_meta(name);
    let vm_exists = !lima_status_checked(name)?.is_empty();
    if !vm_exists && meta.is_none() {
        if !opts.if_exists {
            return Err(anyhow!("no such wtx VM: {name}"));
        }
        return Ok(RemoveAction::NotFound);
    }
    if let Some(m) = &meta {
        if !m.main_repo.is_empty() {
            unpin_legacy_keep_refs(Path::new(&m.main_repo), name);
        }
    }
    crate::sshx::close_all_forwards(name);
    if let Some(m) = &meta {
        if !m.sim_udid.is_empty() {
            if opts.json {
                crate::sim::delete_device_quietly(&m.sim_udid);
            } else {
                crate::sim::delete_device(&m.sim_udid);
            }
        }
    }
    if vm_exists {
        limactl_capture(&["delete", "-f", name]).map_err(|e| anyhow!("limactl delete: {e}"))?;
    }
    remove_managed_file(&wtx_home().join(format!("{name}.yaml")))?;
    remove_managed_file(&meta_path(name))?;

    if opts.with_worktree {
        remove_linked_worktree(name, meta.as_ref(), opts.json)?;
    }
    Ok(RemoveAction::Deleted)
}

fn remove_linked_worktree(name: &str, meta: Option<&InstanceMeta>, quiet: bool) -> Result<()> {
    let m = meta.ok_or_else(|| anyhow!("no metadata for {name}; cannot locate the worktree"))?;

    // Remove only linked worktrees; removing a regular repository would delete the main copy.
    if m.main_repo.is_empty() || m.main_repo == m.workdir {
        eprintln!(
            "wtx: {name} is not a linked worktree; left {} in place",
            m.workdir
        );
    } else if !Path::new(&m.workdir).exists() {
        eprintln!("wtx: worktree {} is already gone", m.workdir);
    } else {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&m.main_repo)
            .args(["worktree", "remove", "--force", &m.workdir])
            .status()?;
        if st.success() {
            if !quiet {
                println!("removed worktree {}", m.workdir);
            }
        } else {
            eprintln!(
                "wtx: could not remove worktree {} (remove it manually)",
                m.workdir
            );
        }
    }
    Ok(())
}

fn print_remove_result(name: &str, action: RemoveAction, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&RemoveReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                action,
                name,
            })?
        );
    } else if matches!(action, RemoveAction::NotFound) {
        println!("not_found: {name}");
    }
    Ok(())
}

fn remove_managed_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow!("remove {}: {err}", path.display())),
    }
}

/// Clean up VMs orphaned by a removed worktree. Commits are stored directly in the host's
/// `.git`, so deleting the VM does not discard work.
pub fn prune(yes: bool) {
    let orphans: Vec<Instance> = list_instances()
        .into_iter()
        .filter(|i| i.orphaned)
        .collect();
    if orphans.is_empty() {
        println!("no orphaned VMs");
        return;
    }
    for i in &orphans {
        if !yes {
            println!("  would delete {} (workdir gone: {})", i.name, i.workdir);
            continue;
        }
        match rm(&i.name, RemoveOpts::default()) {
            Ok(()) => println!("  deleted {}", i.name),
            Err(e) => println!("  failed to delete {}: {e}", i.name),
        }
    }
    if !yes {
        println!("re-run with --yes to delete them");
    }
}

/// List VMs known to wtx, including orphan status.
pub fn ls() {
    let rows = list_instances();
    if rows.is_empty() {
        println!("no VMs. Create one with `wtx up NAME WORKDIR`");
        return;
    }
    println!(
        "{}{}{}",
        pad("NAME", 24),
        pad("STATUS", 10),
        pad("BRANCH", 16)
    );
    // Query simulator state only when a VM uses one, avoiding xcrun on unsupported systems.
    let sim_states = crate::sim::states_for(
        &rows
            .iter()
            .filter(|i| !i.sim_udid.is_empty())
            .map(|i| i.sim_udid.clone())
            .collect::<Vec<_>>(),
    );
    for i in &rows {
        let orphan = if i.orphaned {
            "  (orphaned: workdir gone)"
        } else {
            ""
        };
        let sim = if i.sim_udid.is_empty() {
            String::new()
        } else {
            let st = sim_states
                .get(&i.sim_udid)
                .map_or("missing", String::as_str);
            format!("  sim:{st}")
        };
        println!(
            "{}{}{}{}{}{}",
            pad(&i.name, 24),
            pad(&i.status, 10),
            pad(&i.branch, 16),
            i.workdir,
            sim,
            orphan
        );
    }
    if rows.iter().any(|i| i.orphaned) {
        println!("\norphaned VMs can be cleaned up with `wtx prune`");
    }
}

/// Machine-readable `wtx ls --json` output for agents and scripts.
pub fn ls_json() -> Result<()> {
    #[derive(Serialize)]
    struct Row {
        name: String,
        status: String,
        branch: String,
        workdir: String,
        repo: String,
        orphaned: bool,
        #[serde(skip_serializing_if = "String::is_empty")]
        sim_udid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sim_state: Option<String>,
    }
    let rows = list_instances();
    let sim_states = crate::sim::states_for(
        &rows
            .iter()
            .filter(|i| !i.sim_udid.is_empty())
            .map(|i| i.sim_udid.clone())
            .collect::<Vec<_>>(),
    );
    let out: Vec<Row> = rows
        .into_iter()
        .map(|i| Row {
            sim_state: if i.sim_udid.is_empty() {
                None
            } else {
                Some(
                    sim_states
                        .get(&i.sim_udid)
                        .cloned()
                        .unwrap_or_else(|| "missing".to_string()),
                )
            },
            name: i.name,
            status: i.status,
            branch: i.branch,
            workdir: i.workdir,
            repo: i.repo,
            orphaned: i.orphaned,
            sim_udid: i.sim_udid,
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Instance list for the TUI.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub status: String,
    pub workdir: String,
    pub branch: String,
    /// Project host repository root, used as the TUI grouping key.
    pub repo: String,
    /// Whether the VM's worktree is gone. Deleting it does not lose host-stored commits.
    pub orphaned: bool,
    /// Worktree-specific simulator UDID, empty if no simulator has been created.
    pub sim_udid: String,
}

pub fn list_instances() -> Vec<Instance> {
    let out = limactl_out(&["list", "--format", "{{.Name}}\t{{.Status}}"]);
    out.lines()
        .filter_map(|l| {
            let (name, status) = l.split_once('\t')?;
            let meta = load_meta(name);
            let workdir = meta.as_ref().map(|m| m.workdir.clone()).unwrap_or_default();
            Some(Instance {
                name: name.to_string(),
                status: status.to_string(),
                orphaned: !workdir.is_empty() && !Path::new(&workdir).exists(),
                workdir: meta.as_ref().map(|m| m.workdir.clone()).unwrap_or_default(),
                branch: meta.as_ref().map(|m| m.branch.clone()).unwrap_or_default(),
                sim_udid: meta
                    .as_ref()
                    .map(|m| m.sim_udid.clone())
                    .unwrap_or_default(),
                repo: meta.map(|m| m.main_repo).unwrap_or_default(),
            })
        })
        .collect()
}

/// Derive a VM name from a directory name using only characters valid for Lima instances.
pub fn derive_name(dir: &Path) -> Result<String> {
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    if cleaned.is_empty() {
        return Err(anyhow!(
            "cannot derive a VM name from {}; pass NAME explicitly",
            dir.display()
        ));
    }
    Ok(cleaned)
}

/// Fail if a derived name is already used by a VM for another workdir. Silently reattaching
/// would overwrite the recorded workdir while leaving mounts pointed at the old worktree.
pub fn ensure_name_matches(name: &str, dir: &Path) -> Result<()> {
    if let Some(m) = load_meta(name) {
        if !m.workdir.is_empty() && Path::new(&m.workdir) != dir {
            return Err(anyhow!(
                "VM {name} already exists for {}; pass a different NAME",
                m.workdir
            ));
        }
    }
    Ok(())
}

/// `wtx new BRANCH`: create a worktree and VM together, creating a missing branch from HEAD.
pub fn new(branch: &str, dir: Option<&str>, o: &UpOpts) -> Result<()> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let repo = repo::inspect_repo(&cwd)?.ok_or_else(|| anyhow!("not inside a git repository"))?;
    let main_repo = repo.host_repo;
    let dirpath = if let Some(d) = dir {
        // Resolve relative paths against the cwd first because Git resolves them from the
        // repository root.
        let p = Path::new(d);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()?.join(p)
        }
    } else {
        let repo_name = main_repo
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        main_repo
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{repo_name}-{}", branch.replace('/', "-")))
    };
    if dirpath.exists() {
        return Err(anyhow!("{} already exists", dirpath.display()));
    }
    // Check for a VM name collision before creating the worktree to avoid cleanup on failure.
    let name = derive_name(&dirpath)?;
    ensure_name_matches(&name, &dirpath)?;

    let branch_ref = format!("refs/heads/{branch}");
    let exists = git_run(
        &main_repo,
        &["show-ref", "--verify", "--quiet", &branch_ref],
    )
    .is_ok();
    let dirstr = dirpath.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if exists {
        args.push(&dirstr);
        args.push(branch);
    } else {
        args.push("-b");
        args.push(branch);
        args.push(&dirstr);
    }
    // Preserve Git output for errors such as a branch already checked out in another worktree.
    let st = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_repo)
        .args(&args)
        .status()?;
    if !st.success() {
        return Err(anyhow!("git worktree add failed"));
    }
    up(&name, &dirstr, o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UpOpts {
        UpOpts {
            memory: None,
            cpus: None,
            disk: "20GiB".into(),
            from: None,
            agent_access: false,
            no_clone: false,
            extra_mounts: vec![],
            sim: false,
            sim_device: None,
        }
    }

    #[test]
    fn provision_path_covers_all_state_machine_branches() {
        assert_eq!(
            provision_path("", true, false).unwrap(),
            ProvisionPath::Seed
        );
        assert_eq!(
            provision_path("Stopped", false, false).unwrap(),
            ProvisionPath::Reattach
        );
        assert_eq!(
            provision_path("", false, false).unwrap(),
            ProvisionPath::Golden
        );
        assert_eq!(
            provision_path("", false, true).unwrap(),
            ProvisionPath::Fresh
        );
        assert!(provision_path("Running", true, false).is_err());
    }

    #[test]
    fn golden_preparation_covers_first_use_recovery_and_ready_paths() {
        assert_eq!(
            golden_preparation(false, "", false),
            GoldenPreparation::Build
        );
        assert_eq!(
            golden_preparation(true, "Stopped", true),
            GoldenPreparation::Ready
        );
        assert_eq!(
            golden_preparation(true, "Running", true),
            GoldenPreparation::Stop
        );
        assert_eq!(
            golden_preparation(true, "Stopped", false),
            GoldenPreparation::Rebuild
        );
        assert_eq!(
            golden_preparation(true, "Broken", true),
            GoldenPreparation::Rebuild
        );
    }

    #[test]
    fn existing_vm_cannot_silently_change_credential_mount_policy() {
        assert!(validate_agent_access(true, true, false).is_err());
        assert!(validate_agent_access(true, false, true).is_ok());
        assert!(validate_agent_access(false, true, false).is_ok());
    }

    #[test]
    fn clone_args_reapply_agent_policy() {
        let mut options = opts();
        options.agent_access = true;
        let args = clone_args(
            "source",
            "target",
            &options,
            &[Mount {
                location: "/tmp/work tree".into(),
                writable: true,
            }],
        );
        assert!(args
            .windows(2)
            .any(|v| v == ["--set", ".ssh.forwardAgent=true"]));
        assert!(args.iter().any(|v| v == "/tmp/work tree:w"));
    }

    #[test]
    fn mount_collection_keeps_automatic_mounts_unique_and_extra_modes() {
        let root = tempfile::tempdir().unwrap();
        let workdir = root.path().join("worktree");
        let host_git = root.path().join("main/.git");
        let host_claude = root.path().join(".claude");
        let extra = root.path().join("extra");
        for path in [&workdir, &host_git, &host_claude, &extra] {
            std::fs::create_dir_all(path).unwrap();
        }
        let workdir = std::fs::canonicalize(workdir).unwrap();
        let repo = RepoInfo {
            kind: RepoKind::Worktree,
            host_git: host_git.clone(),
            host_repo: root.path().join("main"),
            branch: "feature".into(),
        };
        let mut options = opts();
        options.agent_access = true;
        options.extra_mounts = vec![
            format!("{}:ro", extra.display()),
            workdir.to_string_lossy().into_owned(),
        ];

        let mounts = collect_mounts(&workdir, Some(&repo), &options, &host_claude).unwrap();

        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0].location, workdir.to_string_lossy());
        assert!(mounts[0].writable);
        assert_eq!(mounts[1].location, host_git.to_string_lossy());
        assert!(mounts[1].writable);
        assert_eq!(mounts[2].location, host_claude.to_string_lossy());
        assert!(mounts[2].writable);
        assert_eq!(
            mounts[3].location,
            std::fs::canonicalize(extra).unwrap().to_string_lossy()
        );
        assert!(!mounts[3].writable);
    }

    #[test]
    fn metadata_rebuild_preserves_reattached_runtime_assignments() {
        let workdir = Path::new("/tmp/worktree");
        let repo = RepoInfo {
            kind: RepoKind::Normal,
            host_git: workdir.join(".git"),
            host_repo: workdir.to_path_buf(),
            branch: "feature".into(),
        };
        let mut ports = BTreeMap::new();
        ports.insert(
            "api".into(),
            PortMap {
                host: 42000,
                guest: 3000,
            },
        );
        let previous = InstanceMeta {
            seeded_from: "seed-vm".into(),
            sim_udid: "sim-udid".into(),
            sim_devicetype: "iPhone".into(),
            ports,
            agent_access: true,
            ..Default::default()
        };

        let meta = rebuild_meta(workdir, Some(&repo), &opts(), true, previous);

        assert_eq!(meta.workdir, "/tmp/worktree");
        assert_eq!(meta.main_repo, "/tmp/worktree");
        assert_eq!(meta.branch, "feature");
        assert_eq!(meta.seeded_from, "seed-vm");
        assert_eq!(meta.sim_udid, "sim-udid");
        assert_eq!(meta.sim_devicetype, "iPhone");
        assert!(meta.agent_access);
        let api = &meta.ports["api"];
        assert_eq!((api.host, api.guest), (42000, 3000));
    }

    #[test]
    fn rendered_yaml_escapes_mount_paths_and_quotes_scalars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.yaml");
        render_yaml(
            &[Mount {
                location: "/tmp/O\"Brien\nworktree".into(),
                writable: true,
            }],
            2,
            "4GiB",
            "20GiB",
            false,
            &path,
        )
        .unwrap();
        let yaml = std::fs::read_to_string(path).unwrap();
        assert!(yaml.contains(r#"location: "/tmp/O\"Brien\nworktree""#));
        assert!(yaml.contains("memory: \"4GiB\""));
        assert!(yaml.contains("forwardAgent: false"));
        assert!(yaml.contains(PROVISION_DOCKER_VERSION));
    }

    #[test]
    fn golden_receipt_requires_matching_schema_and_docker_version() {
        let mut receipt = GoldenReceipt {
            provision_schema_version: PROVISION_SCHEMA_VERSION,
            wtx_version: "0.9.0".into(),
            docker_version: PROVISION_DOCKER_VERSION.into(),
        };
        assert!(golden_receipt_is_compatible(&receipt));
        receipt.docker_version = "29.7.1".into();
        assert!(!golden_receipt_is_compatible(&receipt));
        receipt.docker_version = PROVISION_DOCKER_VERSION.into();
        receipt.provision_schema_version += 1;
        assert!(!golden_receipt_is_compatible(&receipt));
    }

    #[test]
    fn compose_project_and_seed_volume_names_are_deterministic() {
        assert_eq!(compose_project_name(Path::new("/tmp/My App")), "myapp");
        assert_eq!(compose_project_name(Path::new("/tmp/_Project")), "project");
        assert_eq!(
            renamed_volume("app-a", "app-b", "app-a_database"),
            Some(("database".into(), "app-b_database".into()))
        );
        assert_eq!(renamed_volume("app-a", "app-b", "fixed-name"), None);
        assert_eq!(renamed_volume("app-a", "app-a", "app-a_database"), None);
        assert_eq!(renamed_volume("", "app-b", "app-a_database"), None);
    }

    #[test]
    fn remove_receipt_uses_stable_machine_contract() {
        let receipt = RemoveReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            action: RemoveAction::NotFound,
            name: "vm-a",
        };

        assert_eq!(
            serde_json::to_value(receipt).unwrap(),
            serde_json::json!({
                "schema_version": 2,
                "action": "not_found",
                "name": "vm-a",
            })
        );
    }
}
