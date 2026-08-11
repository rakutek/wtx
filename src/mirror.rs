//! 内蔵 pull-through レジストリキャッシュ。
//!
//! distribution のような汎用レジストリではなく、必要な部分だけを実装する:
//!   - blob は digest で不変なのでディスクにキャッシュする（イメージ容量のほぼ全て）
//!   - manifest は tag が動くので常に上流へ問い合わせる（キャッシュ不整合を構造的に排除）
//!   - 上流の 401 は WWW-Authenticate を解釈してトークンを取得し、透過的に再試行する
//!     → docker.io だけでなく ghcr.io / quay.io などでも同じ仕組みで動く
use crate::util::wtx_home;
use anyhow::{anyhow, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

pub static LAST_ACTIVITY: AtomicI64 = AtomicI64::new(0);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const DEFAULT_CACHE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;

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
    BTreeMap::from([("docker.io".to_string(), 5001u16)])
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
    tokens: Arc<tokio::sync::Mutex<TokenCache>>,
}

#[derive(Default)]
struct TokenCache {
    by_challenge: HashMap<AuthChallenge, String>,
    by_repository: HashMap<String, AuthChallenge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AuthChallenge {
    realm: String,
    service: String,
    scope: String,
}

#[derive(Deserialize)]
struct TokenResp {
    #[serde(default)]
    token: String,
    #[serde(default)]
    access_token: String,
}

/// WWW-Authenticate: Bearer realm="...",service="...",scope="..." を解釈してトークンを取る。
fn parse_challenge(challenge: &str) -> Option<AuthChallenge> {
    let rest = challenge.trim().strip_prefix("Bearer ")?;
    let mut params: BTreeMap<&str, String> = BTreeMap::new();
    for part in rest.split(',') {
        if let Some((k, v)) = part.trim().split_once('=') {
            params.insert(k.trim(), v.trim().trim_matches('"').to_string());
        }
    }
    Some(AuthChallenge {
        realm: params.remove("realm")?,
        service: params.remove("service").unwrap_or_default(),
        scope: params.remove("scope").unwrap_or_default(),
    })
}

async fn fetch_token(st: &AppState, challenge: &AuthChallenge) -> Option<String> {
    let mut query = Vec::new();
    if !challenge.service.is_empty() {
        query.push(("service", challenge.service.as_str()));
    }
    if !challenge.scope.is_empty() {
        query.push(("scope", challenge.scope.as_str()));
    }
    let resp = st
        .http
        .get(&challenge.realm)
        .query(&query)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
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
    range: Option<&HeaderValue>,
    head: bool,
    repository: &str,
) -> Result<reqwest::Response> {
    let url = format!("{}{}", st.entry.upstream, path);
    let mut auth = {
        let tokens = st.tokens.lock().await;
        tokens
            .by_repository
            .get(repository)
            .and_then(|key| tokens.by_challenge.get(key))
            .cloned()
    };
    for attempt in 0..2 {
        let mut req = if head {
            st.http.head(&url)
        } else {
            st.http.get(&url)
        };
        if let Some(a) = accept {
            req = req.header(reqwest::header::ACCEPT, a.to_str().unwrap_or("*/*"));
        }
        if let Some(r) = range {
            req = req.header(reqwest::header::RANGE, r.clone());
        }
        if let Some(t) = &auth {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED || attempt == 1 {
            return Ok(resp);
        }
        let Some(challenge) = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_challenge)
        else {
            return Ok(resp);
        };
        // 既存tokenで401なら必ずrefreshする。未認証ならscope別cacheを再利用できる。
        auth = if auth.is_none() {
            st.tokens.lock().await.by_challenge.get(&challenge).cloned()
        } else {
            None
        };
        if auth.is_none() {
            auth = fetch_token(st, &challenge).await;
        }
        let Some(token) = auth.clone() else {
            return Ok(resp);
        };
        let mut tokens = st.tokens.lock().await;
        tokens.by_challenge.insert(challenge.clone(), token);
        tokens
            .by_repository
            .insert(repository.to_string(), challenge);
    }
    Err(anyhow!("unreachable"))
}

fn blob_path(cache: &Path, digest: &str) -> Option<PathBuf> {
    let (algo, hex) = digest.split_once(':')?;
    if algo != "sha256" || hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(cache.join("blobs").join(algo).join(hex))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

fn parse_range(raw: &str, len: u64) -> Option<ByteRange> {
    let value = raw.strip_prefix("bytes=")?;
    if value.contains(',') || len == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix);
        return Some(ByteRange {
            start,
            end: len - 1,
        });
    }
    let start = start.parse::<u64>().ok()?;
    if start >= len {
        return None;
    }
    let end = if end.is_empty() {
        len - 1
    } else {
        end.parse::<u64>().ok()?.min(len - 1)
    };
    (start <= end).then_some(ByteRange { start, end })
}

fn response_with_upstream_headers(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> axum::http::response::Builder {
    let mut out = Response::builder().status(status);
    for h in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
        "docker-content-digest",
        "etag",
        "last-modified",
        "www-authenticate",
    ] {
        if let Some(v) = headers.get(h) {
            if let (Ok(n), Ok(v)) = (
                HeaderName::try_from(h),
                HeaderValue::from_bytes(v.as_bytes()),
            ) {
                out = out.header(n, v);
            }
        }
    }
    out
}

async fn serve_cached_blob(path: &Path, headers: &HeaderMap, head: bool) -> io::Result<Response> {
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let requested = headers
        .get(axum::http::header::RANGE)
        .and_then(|h| h.to_str().ok());
    let range = match requested {
        Some(raw) => match parse_range(raw, len) {
            Some(range) => Some(range),
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(axum::http::header::CONTENT_RANGE, format!("bytes */{len}"))
                    .body(Body::empty())
                    .unwrap())
            }
        },
        None => None,
    };
    let (status, start, length) = match range {
        Some(r) => (StatusCode::PARTIAL_CONTENT, r.start, r.end - r.start + 1),
        None => (StatusCode::OK, 0, len),
    };
    if start != 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let mut out = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CONTENT_LENGTH, length.to_string());
    if let Some(r) = range {
        out = out.header(
            axum::http::header::CONTENT_RANGE,
            format!("bytes {}-{}/{len}", r.start, r.end),
        );
    }
    if head {
        return Ok(out.body(Body::empty()).unwrap());
    }
    let stream = ReaderStream::new(file.take(length));
    Ok(out.body(Body::from_stream(stream)).unwrap())
}

fn cache_root() -> PathBuf {
    wtx_home().join("mirror-cache")
}

fn cache_limit_path() -> PathBuf {
    wtx_home().join("mirror-cache-limit-bytes")
}

fn cache_limit() -> u64 {
    std::fs::read_to_string(cache_limit_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_CACHE_MAX_BYTES)
}

#[derive(Debug, Default)]
struct GcStats {
    before: u64,
    after: u64,
    removed: usize,
}

fn collect_cache_files(dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_cache_files(&path, out);
        } else if meta.is_file()
            && !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".part-"))
        {
            out.push((path, meta.len(), meta.modified().unwrap_or(UNIX_EPOCH)));
        }
    }
}

fn gc_cache_sync(root: &Path, limit: u64, protected: Option<&Path>) -> GcStats {
    let mut files = Vec::new();
    collect_cache_files(root, &mut files);
    let before: u64 = files.iter().map(|(_, size, _)| size).sum();
    let mut after = before;
    let mut removed = 0;
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if after <= limit {
            break;
        }
        if protected.is_some_and(|p| p == path) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            after = after.saturating_sub(size);
            removed += 1;
        }
    }
    GcStats {
        before,
        after,
        removed,
    }
}

async fn gc_cache_after_write(protected: PathBuf) {
    let root = cache_root();
    let limit = cache_limit();
    let _ =
        tokio::task::spawn_blocking(move || gc_cache_sync(&root, limit, Some(&protected))).await;
}

async fn handle_blob(
    State(st): State<AppState>,
    AxPath((name, digest)): AxPath<(String, String)>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    LAST_ACTIVITY.store(now(), Ordering::Relaxed);
    let path = match blob_path(&st.cache, &digest) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "bad digest").into_response(),
    };
    if tokio::fs::metadata(&path).await.is_ok() {
        return match serve_cached_blob(&path, &headers, method == Method::HEAD).await {
            Ok(response) => response,
            Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        };
    }
    let range = headers.get(axum::http::header::RANGE).cloned();
    let resp = match upstream_get(
        &st,
        &format!("/v2/{name}/blobs/{digest}"),
        None,
        range.as_ref(),
        method == Method::HEAD,
        &name,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !resp.status().is_success() || method == Method::HEAD {
        return response_with_upstream_headers(status, resp.headers())
            .body(Body::empty())
            .unwrap();
    }
    let out = response_with_upstream_headers(status, resp.headers());
    // Range responseは部分blobなのでcacheしない。上流bodyはそのままstreamする。
    if range.is_some() || status == StatusCode::PARTIAL_CONTENT {
        let stream = resp
            .bytes_stream()
            .map(|item| item.map_err(io::Error::other));
        return out.body(Body::from_stream(stream)).unwrap();
    }
    let content_too_large = resp
        .content_length()
        .is_some_and(|length| length > cache_limit());
    let tmp = path.with_file_name(format!(
        "{}.part-{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("blob"),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let expected = digest.trim_start_matches("sha256:").to_ascii_lowercase();
    let stream = async_stream::stream! {
        let mut upstream = resp.bytes_stream();
        let mut file = if content_too_large {
            None
        } else {
            tokio::fs::File::create(&tmp).await.ok()
        };
        let mut hasher = Sha256::new();
        let mut complete = true;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    hasher.update(&chunk);
                    if let Some(cache_file) = file.as_mut() {
                        if cache_file.write_all(&chunk).await.is_err() {
                            file = None;
                        }
                    }
                    yield Ok::<Bytes, io::Error>(chunk);
                }
                Err(error) => {
                    complete = false;
                    yield Err(io::Error::other(error));
                    break;
                }
            }
        }
        if let Some(mut cache_file) = file {
            let flushed = cache_file.flush().await.is_ok();
            drop(cache_file);
            let actual = format!("{:x}", hasher.finalize());
            if complete && flushed && actual == expected && tokio::fs::rename(&tmp, &path).await.is_ok() {
                gc_cache_after_write(path.clone()).await;
            } else {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    };
    out.body(Body::from_stream(stream)).unwrap()
}

/// manifest は tag が動くので常に上流に問い合わせる（キャッシュしない）。
async fn handle_manifest(
    State(st): State<AppState>,
    AxPath((name, reference)): AxPath<(String, String)>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    LAST_ACTIVITY.store(now(), Ordering::Relaxed);
    let accept = headers.get(axum::http::header::ACCEPT).cloned();
    let resp = match upstream_get(
        &st,
        &format!("/v2/{name}/manifests/{reference}"),
        accept.as_ref(),
        None,
        method == Method::HEAD,
        &name,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let out = response_with_upstream_headers(status, resp.headers());
    if method == Method::HEAD {
        return out.body(Body::empty()).unwrap();
    }
    let stream = resp
        .bytes_stream()
        .map(|item| item.map_err(io::Error::other));
    out.body(Body::from_stream(stream)).unwrap()
}

fn router(entry: MirrorEntry) -> Router {
    let cache = wtx_home().join("mirror-cache").join(&entry.registry);
    let _ = std::fs::create_dir_all(&cache);
    let state = AppState {
        entry: Arc::new(entry),
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()
            .expect("http client"),
        cache,
        tokens: Arc::new(tokio::sync::Mutex::new(TokenCache::default())),
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
    method: Method,
) -> Response {
    eprintln!("[wtx-mirror] {} /v2/{}", st.entry.registry, rest);
    if let Some((name, reference)) = rest.rsplit_once("/manifests/") {
        return handle_manifest(
            State(st),
            AxPath((name.to_string(), reference.to_string())),
            headers,
            method,
        )
        .await;
    }
    if let Some((name, digest)) = rest.rsplit_once("/blobs/") {
        return handle_blob(
            State(st),
            AxPath((name.to_string(), digest.to_string())),
            headers,
            method,
        )
        .await;
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
        println!("mirror: up (configured registry endpoints)");
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
            println!("mirror: up (configured registry endpoints)");
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
    let stats = gc_cache_sync(&cache_root(), u64::MAX, None);
    println!(
        "cache: {} / {}",
        human_bytes(stats.before),
        human_bytes(cache_limit())
    );
}

fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    format!("{:.2} GiB", bytes as f64 / GIB)
}

pub fn gc(max_gib: Option<u64>) -> Result<()> {
    if let Some(gib) = max_gib {
        if gib == 0 {
            return Err(anyhow!("--max-gib must be greater than zero"));
        }
        let bytes = gib
            .checked_mul(1024 * 1024 * 1024)
            .ok_or_else(|| anyhow!("--max-gib is too large"))?;
        std::fs::write(cache_limit_path(), bytes.to_string())?;
    }
    let stats = gc_cache_sync(&cache_root(), cache_limit(), None);
    println!(
        "cache GC: {} -> {} ({} files removed, limit {})",
        human_bytes(stats.before),
        human_bytes(stats.after),
        stats.removed,
        human_bytes(cache_limit())
    );
    Ok(())
}

/// Docker Engineが実際に透過利用するHub mirrorだけをdaemon.jsonへ収束させる。
/// 非Hub entryは明示的な localhost pull用で、効かないcerts.d設定は生成しない。
pub fn apply_to_vm(vm: &str) -> Result<()> {
    let port = mirror_port();
    let script = format!(
        r#"set -eu
tmp=$(mktemp)
cat >"$tmp" <<'EOF'
{{
  "registry-mirrors": ["http://host.lima.internal:{port}"],
  "insecure-registries": ["host.lima.internal:{port}"]
}}
EOF
if ! sudo cmp -s "$tmp" /etc/docker/daemon.json; then
  sudo install -m 0644 "$tmp" /etc/docker/daemon.json
  sudo systemctl restart docker
fi
rm -f "$tmp"
"#,
    );
    crate::sshx::vm_script(vm, &script, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_challenge_keeps_repository_scope_separate() {
        let a = parse_challenge(
            r#"Bearer realm="https://auth.test/token",service="registry.test",scope="repository:a/app:pull""#,
        )
        .unwrap();
        let b = parse_challenge(
            r#"Bearer realm="https://auth.test/token",service="registry.test",scope="repository:b/app:pull""#,
        )
        .unwrap();
        assert_ne!(a, b);
        assert_eq!(a.scope, "repository:a/app:pull");
    }

    #[test]
    fn byte_ranges_cover_prefix_open_and_suffix_forms() {
        assert_eq!(
            parse_range("bytes=2-5", 10),
            Some(ByteRange { start: 2, end: 5 })
        );
        assert_eq!(
            parse_range("bytes=7-", 10),
            Some(ByteRange { start: 7, end: 9 })
        );
        assert_eq!(
            parse_range("bytes=-3", 10),
            Some(ByteRange { start: 7, end: 9 })
        );
        assert_eq!(parse_range("bytes=10-", 10), None);
    }

    #[tokio::test]
    async fn cached_blob_returns_partial_content_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"0123456789").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::RANGE,
            HeaderValue::from_static("bytes=2-5"),
        );

        let response = serve_cached_blob(&path, &headers, false).await.unwrap();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[axum::http::header::CONTENT_LENGTH], "4");
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_RANGE],
            "bytes 2-5/10"
        );
    }

    #[test]
    fn blob_path_accepts_only_verified_sha256_shape() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(blob_path(Path::new("/cache"), &digest).is_some());
        assert!(blob_path(Path::new("/cache"), "sha256:../../etc/passwd").is_none());
        assert!(blob_path(Path::new("/cache"), &format!("sha512:{}", "a".repeat(64))).is_none());
    }

    #[test]
    fn gc_enforces_capacity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 10]).unwrap();
        std::fs::write(dir.path().join("b"), vec![0u8; 10]).unwrap();
        let stats = gc_cache_sync(dir.path(), 10, None);
        assert_eq!(stats.before, 20);
        assert!(stats.after <= 10);
        assert_eq!(stats.removed, 1);
    }
}
