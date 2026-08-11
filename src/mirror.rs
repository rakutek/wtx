//! 内蔵 pull-through レジストリキャッシュ。
//!
//! distribution のような汎用レジストリではなく、必要な部分だけを実装する:
//!   - blob は digest で不変なのでディスクにキャッシュする（イメージ容量のほぼ全て）
//!   - manifest は tag が動くので常に上流へ問い合わせる（キャッシュ不整合を構造的に排除）
//!   - 上流の 401 は WWW-Authenticate を解釈してトークンを取得し、透過的に再試行する
//!     → docker.io だけでなく ghcr.io / quay.io などでも同じ仕組みで動く
use crate::util::wtx_home;
use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub static LAST_ACTIVITY: AtomicI64 = AtomicI64::new(0);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Clone)]
pub struct MirrorEntry {
    pub registry: String,
    pub port: u16,
    pub upstream: String,
}

fn default_mirrors() -> BTreeMap<String, u16> {
    BTreeMap::from([
        ("docker.io".to_string(), 5001u16),
        ("ghcr.io".to_string(), 5002),
        ("quay.io".to_string(), 5003),
        ("registry.k8s.io".to_string(), 5004),
    ])
}

fn upstream_for(registry: &str) -> String {
    if registry == "docker.io" {
        "https://registry-1.docker.io".to_string()
    } else {
        format!("https://{registry}")
    }
}

/// ~/.wtx/mirrors.json（{"ghcr.io": 5002, ...}）を読む。無ければ既定値。
pub fn mirror_config() -> Vec<MirrorEntry> {
    let map = std::fs::read_to_string(wtx_home().join("mirrors.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, u16>>(&s).ok())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(default_mirrors);
    map.into_iter()
        .map(|(registry, port)| MirrorEntry {
            upstream: upstream_for(&registry),
            registry,
            port,
        })
        .collect()
}

pub fn mirror_port() -> u16 {
    if let Ok(p) = std::env::var("WTX_MIRROR_PORT") {
        if let Ok(n) = p.parse() {
            return n;
        }
    }
    mirror_config()
        .iter()
        .find(|e| e.registry == "docker.io")
        .map(|e| e.port)
        .unwrap_or(5001)
}

pub fn port_alive(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_secs(3),
    )
    .is_ok()
}

pub fn mirror_alive() -> bool {
    port_alive(mirror_port())
}

#[derive(Clone)]
struct AppState {
    entry: Arc<MirrorEntry>,
    http: reqwest::Client,
    cache: PathBuf,
    token: Arc<tokio::sync::Mutex<Option<String>>>,
}

#[derive(Deserialize)]
struct TokenResp {
    #[serde(default)]
    token: String,
    #[serde(default)]
    access_token: String,
}

/// WWW-Authenticate: Bearer realm="...",service="...",scope="..." を解釈してトークンを取る。
async fn fetch_token(st: &AppState, challenge: &str) -> Option<String> {
    let rest = challenge.trim().strip_prefix("Bearer ")?;
    let mut params: BTreeMap<&str, String> = BTreeMap::new();
    for part in rest.split(',') {
        if let Some((k, v)) = part.trim().split_once('=') {
            params.insert(k.trim(), v.trim().trim_matches('"').to_string());
        }
    }
    let realm = params.remove("realm")?;
    let query: Vec<(String, String)> = params
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let resp = st.http.get(&realm).query(&query).send().await.ok()?;
    let t: TokenResp = resp.json().await.ok()?;
    let tok = if t.token.is_empty() {
        t.access_token
    } else {
        t.token
    };
    if tok.is_empty() {
        None
    } else {
        Some(tok)
    }
}

/// 上流へのリクエスト。401 ならトークンを取り直して一度だけ再試行する。
async fn upstream_get(
    st: &AppState,
    path: &str,
    accept: Option<&HeaderValue>,
    head: bool,
) -> Result<reqwest::Response> {
    let url = format!("{}{}", st.entry.upstream, path);
    for attempt in 0..2 {
        let mut req = if head {
            st.http.head(&url)
        } else {
            st.http.get(&url)
        };
        if let Some(a) = accept {
            req = req.header(reqwest::header::ACCEPT, a.to_str().unwrap_or("*/*"));
        }
        if let Some(t) = st.token.lock().await.clone() {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED || attempt == 1 {
            return Ok(resp);
        }
        let challenge = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        match fetch_token(st, &challenge).await {
            Some(t) => *st.token.lock().await = Some(t),
            None => return Ok(resp),
        }
    }
    Err(anyhow!("unreachable"))
}

fn blob_path(cache: &Path, digest: &str) -> Option<PathBuf> {
    let (algo, hex) = digest.split_once(':')?;
    if !hex.chars().all(|c| c.is_ascii_alphanumeric()) || hex.len() < 8 {
        return None;
    }
    Some(cache.join("blobs").join(algo).join(hex))
}

async fn handle_blob(
    State(st): State<AppState>,
    AxPath((name, digest)): AxPath<(String, String)>,
) -> Response {
    LAST_ACTIVITY.store(now(), Ordering::Relaxed);
    let path = match blob_path(&st.cache, &digest) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "bad digest").into_response(),
    };
    if let Ok(data) = tokio::fs::read(&path).await {
        return (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response();
    }
    let resp = match upstream_get(&st, &format!("/v2/{name}/blobs/{digest}"), None, false).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    if !resp.status().is_success() {
        return (
            StatusCode::from_u16(resp.status().as_u16()).unwrap(),
            "upstream error",
        )
            .into_response();
    }
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let tmp = path.with_extension("tmp");
    if tokio::fs::write(&tmp, &body).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, &path).await;
    }
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response()
}

/// manifest は tag が動くので常に上流に問い合わせる（キャッシュしない）。
async fn handle_manifest(
    State(st): State<AppState>,
    AxPath((name, reference)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    LAST_ACTIVITY.store(now(), Ordering::Relaxed);
    let accept = headers.get(axum::http::header::ACCEPT).cloned();
    let resp = match upstream_get(
        &st,
        &format!("/v2/{name}/manifests/{reference}"),
        accept.as_ref(),
        false,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out = Response::builder().status(status);
    for h in ["content-type", "docker-content-digest", "etag"] {
        if let Some(v) = resp.headers().get(h) {
            if let (Ok(n), Ok(v)) = (
                HeaderName::try_from(h),
                HeaderValue::from_bytes(v.as_bytes()),
            ) {
                out = out.header(n, v);
            }
        }
    }
    match resp.bytes().await {
        Ok(b) => out.body(Body::from(b)).unwrap(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

fn router(entry: MirrorEntry) -> Router {
    let cache = wtx_home().join("mirror-cache").join(&entry.registry);
    let _ = std::fs::create_dir_all(&cache);
    let state = AppState {
        entry: Arc::new(entry),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("http client"),
        cache,
        token: Arc::new(tokio::sync::Mutex::new(None)),
    };
    Router::new()
        .route("/v2/", get(|| async { axum::Json(serde_json::json!({})) }))
        .route("/v2/{*rest}", get(dispatch).head(dispatch))
        .with_state(state)
}

/// /v2/<name...>/{manifests,blobs}/<ref> を name に / を含む形で振り分ける。
async fn dispatch(
    State(st): State<AppState>,
    AxPath(rest): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    eprintln!("[wtx-mirror] {} /v2/{}", st.entry.registry, rest);
    if let Some((name, reference)) = rest.rsplit_once("/manifests/") {
        return handle_manifest(
            State(st),
            AxPath((name.to_string(), reference.to_string())),
            headers,
        )
        .await;
    }
    if let Some((name, digest)) = rest.rsplit_once("/blobs/") {
        return handle_blob(State(st), AxPath((name.to_string(), digest.to_string()))).await;
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

pub fn serve() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let entries = mirror_config();
        let mut activated = false;
        for e in entries {
            let app = router(e.clone());
            let listener = match crate::launchd::activated_listener(&e.registry) {
                Some(l) => {
                    activated = true;
                    l
                }
                None => std::net::TcpListener::bind(("127.0.0.1", e.port))?,
            };
            listener.set_nonblocking(true)?;
            let tl = tokio::net::TcpListener::from_std(listener)?;
            tokio::spawn(async move {
                let _ = axum::serve(tl, app).await;
            });
        }
        LAST_ACTIVITY.store(now(), Ordering::Relaxed);
        let pid_file = wtx_home().join("mirror.pid");
        let _ = std::fs::write(&pid_file, std::process::id().to_string());

        if activated {
            // launchd 起動時はアイドルで終了する（次のアクセスで launchd が再起動する）
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if now() - LAST_ACTIVITY.load(Ordering::Relaxed) > 600 {
                    let _ = std::fs::remove_file(&pid_file);
                    return Ok(());
                }
            }
        }
        std::future::pending::<()>().await;
        Ok(())
    })
}

pub fn up() -> Result<()> {
    if mirror_alive() {
        println!("mirror: up (127.0.0.1:{} and others)", mirror_port());
        return Ok(());
    }
    let self_exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(wtx_home().join("mirror.log"))?;
    let log2 = log.try_clone()?;
    std::process::Command::new(self_exe)
        .args(["mirror", "serve"])
        .stdout(log)
        .stderr(log2)
        .spawn()?;
    for _ in 0..20 {
        if mirror_alive() {
            println!("mirror: up (127.0.0.1:{} and others)", mirror_port());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(anyhow!("mirror failed to start; see ~/.wtx/mirror.log"))
}

pub fn down() -> Result<()> {
    let pid_file = wtx_home().join("mirror.pid");
    let pid: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let _ = std::fs::remove_file(&pid_file);
    println!("mirror: stopped");
    Ok(())
}

pub fn status() {
    let mode = if crate::launchd::installed() {
        "launchd on-demand (starts on access, exits after 10 min idle)"
    } else {
        "manual (wtx mirror up)"
    };
    println!("mode: {mode}");
    for e in mirror_config() {
        let state = if port_alive(e.port) { "up" } else { "down" };
        println!("  {:<16} :{}  {}", e.registry, e.port, state);
    }
}

/// certs.d をVMに反映する（containerd は pull ごとに読むので docker の再起動は不要。
/// 再起動すると稼働中のDBコンテナが落ちるため意図的に行わない）。
pub fn apply_to_vm(vm: &str) -> Result<()> {
    let mut s = String::from("set -eu\n");
    for e in mirror_config() {
        s.push_str(&format!(
            r#"sudo mkdir -p /etc/containerd/certs.d/{reg}
sudo tee /etc/containerd/certs.d/{reg}/hosts.toml >/dev/null <<'EOF'
server = "{up}"

[host."http://host.lima.internal:{port}"]
  capabilities = ["pull", "resolve"]
  skip_verify = true
EOF
"#,
            reg = e.registry,
            up = e.upstream,
            port = e.port
        ));
    }
    crate::sshx::vm_script(vm, &s, None)
}
