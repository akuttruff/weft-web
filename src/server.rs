//! `weft-web serve`: the server that owns weft sessions, so a browser (a phone
//! over Tailscale) and a terminal on this Mac (`weft-web`) can watch and drive
//! the same run.
//!
//! Each session is the ordinary terminal weft, run in a pseudo-terminal (see
//! [`crate::pty`]); a browser draws it with xterm.js, a terminal just passes
//! the bytes through. weft itself knows nothing of this.
//!
//! A session is a shell as you, so who may reach it is the whole question:
//! - a terminal comes in over a Unix socket only you can open (0600, in a
//!   0700 folder);
//! - a browser comes in over TCP on 127.0.0.1, by way of `tailscale serve`,
//!   and must carry your tailnet login in the header serve adds, name a host
//!   this server answers to (no DNS rebinding), and, for anything that
//!   changes something, come from this page's own origin (no other site's
//!   page can open the socket or post to it).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::serve::ListenerExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::pty::{Pty, Sessions, Spawn};

pub const DEFAULT_PORT: u16 = 8510;

/// Where the terminal side listens: `WEFT_WEB_SOCK`, or
/// `~/.local/state/weft-web/sock`.
pub fn socket_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("WEFT_WEB_SOCK") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/weft-web/sock"))
}

pub struct Options {
    pub port: u16,
    /// The folders a browser may start sessions in, and the picker's list.
    pub root: PathBuf,
    /// The tailnet login allowed in; found from `tailscale status` if unset.
    pub owner: Option<String>,
    /// This Mac's tailnet name (`mac.tailnet.ts.net`); from `tailscale status`
    /// if unset. Under launchd the Tailscale app's CLI does not answer, so
    /// the service is given both.
    pub tailnet_name: Option<String>,
    /// Let a browser on this Mac in at 127.0.0.1 without a tailnet login.
    pub allow_loopback: bool,
    /// The weft to run: a path, or a name looked up on the session's PATH.
    pub weft: PathBuf,
    /// A config file for weft, when not its own default.
    pub config: Option<PathBuf>,
}

struct App {
    sessions: Sessions,
    root: PathBuf,
    weft: PathBuf,
    config: Option<PathBuf>,
    owner: Option<String>,
    /// Host names a request may name: loopback and this Mac's tailnet name.
    hosts: Vec<String>,
    allow_loopback: bool,
}

/// Who is on the other end: a terminal on this Mac (the Unix socket), or a
/// browser (TCP, past the guard).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trust {
    Local,
    Remote,
}

#[derive(Clone)]
struct Ctx {
    app: Arc<App>,
    trust: Trust,
}

pub async fn serve(opts: Options) -> Result<()> {
    let root = opts
        .root
        .canonicalize()
        .with_context(|| format!("the root folder {}", opts.root.display()))?;
    // Given both (the launchd service is), Tailscale is not asked at all.
    let tailnet = match (&opts.owner, &opts.tailnet_name) {
        (Some(_), Some(_)) => None,
        _ => tailnet(),
    };
    let owner = opts
        .owner
        .or_else(|| tailnet.as_ref().map(|t| t.login.clone()));
    let mut hosts = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    if let Some(name) = opts
        .tailnet_name
        .map(|n| n.trim_end_matches('.').to_string())
        .or_else(|| tailnet.as_ref().map(|t| t.dns_name.clone()))
    {
        hosts.push(name);
    }
    let app = Arc::new(App {
        sessions: Sessions::default(),
        root,
        weft: opts.weft,
        config: opts.config,
        owner,
        hosts,
        allow_loopback: opts.allow_loopback,
    });

    let sock = socket_path()?;
    let unix = bind_socket(&sock)?;
    let tcp = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], opts.port)))
        .await
        .with_context(|| format!("listening on 127.0.0.1:{}", opts.port))?
        // A keystroke and its echo are a few bytes each: sent at once, not
        // held back to be joined with the next (Nagle's algorithm).
        .tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });

    eprintln!(
        "weft-web · http://127.0.0.1:{} · terminals at {} · root {} · browsers as {}",
        opts.port,
        sock.display(),
        app.root.display(),
        app.owner
            .as_deref()
            .unwrap_or("(no tailnet login found: loopback only)")
    );
    if let Some(t) = &tailnet {
        eprintln!(
            "  over Tailscale: tailscale serve --bg --https=8446 http://127.0.0.1:{}  →  https://{}:8446",
            opts.port, t.dns_name
        );
    }

    let local = router(Ctx {
        app: app.clone(),
        trust: Trust::Local,
    });
    let remote = router(Ctx {
        app: app.clone(),
        trust: Trust::Remote,
    })
    .layer(middleware::from_fn_with_state(app.clone(), guard));

    let stop = shutdown_signal();
    let (tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let (mut s1, mut s2) = (tx.subscribe(), tx.subscribe());
    let unix_srv = axum::serve(unix, local).with_graceful_shutdown(async move {
        let _ = s1.recv().await;
    });
    let tcp_srv = axum::serve(tcp, remote).with_graceful_shutdown(async move {
        let _ = s2.recv().await;
    });
    let stopper = async move {
        stop.await;
        let _ = tx.send(());
    };
    let (a, b, ()) = tokio::join!(async { unix_srv.await }, async { tcp_srv.await }, stopper);
    app.sessions.kill_all();
    let _ = std::fs::remove_file(&sock);
    a?;
    b?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    eprintln!("weft-web · stopping; ending every session");
}

/// The Unix socket, private to this user. One server at a time: a socket
/// that answers belongs to a running one; one that does not is left over.
fn bind_socket(path: &Path) -> Result<tokio::net::UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("weft-web is already serving ({} answers)", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("listening at {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

struct Tailnet {
    login: String,
    dns_name: String,
}

/// This Mac's tailnet login and name, from `tailscale status --json`.
fn tailnet() -> Option<Tailnet> {
    let bins = [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
    ];
    let out = bins.iter().find_map(|b| {
        std::process::Command::new(b)
            .args(["status", "--json"])
            .output()
            .ok()
            .filter(|o| o.status.success())
    })?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let me = &v["Self"];
    let uid = me["UserID"].as_u64()?.to_string();
    let login = v["User"][&uid]["LoginName"].as_str()?.to_string();
    let dns_name = me["DNSName"].as_str()?.trim_end_matches('.').to_string();
    Some(Tailnet { login, dns_name })
}

fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/", get(|| async { asset("index.html") }))
        .route(
            "/{*file}",
            get(|UrlPath(f): UrlPath<String>| async move { asset(&f) }),
        )
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", delete(end_session))
        .route("/api/folders", get(list_folders))
        .route("/ws", get(ws))
        .layer(middleware::from_fn(headers))
        .with_state(ctx)
}

// ---- The guard (browsers only) -----------------------------------------------

/// The host name a request names, without its port.
fn host_name(h: &str) -> &str {
    match h.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => h,
    }
}

/// Whether a browser's request may go on: see the module note. Pure, for the
/// tests; [`guard`] applies it.
fn admit(app: &App, method: &Method, headers: &HeaderMap) -> Result<(), &'static str> {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let host = text("host").ok_or("no host")?;
    if !app.hosts.iter().any(|h| h == host_name(host)) {
        return Err("unknown host");
    }
    let upgrade = headers.contains_key(header::UPGRADE);
    if *method != Method::GET || upgrade {
        let origin = text("origin").ok_or("no origin")?;
        if origin != format!("https://{host}") && origin != format!("http://{host}") {
            return Err("another site");
        }
    }
    match (text("tailscale-user-login"), &app.owner) {
        (Some(who), Some(owner)) if who == owner => Ok(()),
        (Some(_), _) => Err("not this tailnet's owner"),
        (None, _) if app.allow_loopback => Ok(()),
        (None, _) => Err("not by way of tailscale serve"),
    }
}

async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    match admit(&app, req.method(), req.headers()) {
        Ok(()) => next.run(req).await,
        Err(why) => (StatusCode::FORBIDDEN, why).into_response(),
    }
}

/// Headers on every answer: the page may load only its own files, connect
/// only back here, and never sit in someone else's frame.
async fn headers(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .filter(|h| {
            h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        })
        .unwrap_or("127.0.0.1")
        .to_string();
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self' ws://{host} wss://{host}; \
         manifest-src 'self'; worker-src 'self'; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'none'"
    );
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    res
}

// ---- The page's files ------------------------------------------------------------

fn asset(name: &str) -> Response {
    let (body, kind): (&'static [u8], &str) = match name {
        "index.html" => (
            include_bytes!("../web/index.html"),
            "text/html; charset=utf-8",
        ),
        "app.js" => (include_bytes!("../web/app.js"), "text/javascript"),
        "style.css" => (include_bytes!("../web/style.css"), "text/css"),
        "sw.js" => (include_bytes!("../web/sw.js"), "text/javascript"),
        "manifest.webmanifest" => (
            include_bytes!("../web/manifest.webmanifest"),
            "application/manifest+json",
        ),
        "icon.svg" => (include_bytes!("../web/icon.svg"), "image/svg+xml"),
        "icon-180.png" => (include_bytes!("../web/icon-180.png"), "image/png"),
        "icon-512.png" => (include_bytes!("../web/icon-512.png"), "image/png"),
        "vendor/xterm.js" => (include_bytes!("../web/vendor/xterm.js"), "text/javascript"),
        "vendor/xterm.css" => (include_bytes!("../web/vendor/xterm.css"), "text/css"),
        "vendor/addon-fit.js" => (
            include_bytes!("../web/vendor/addon-fit.js"),
            "text/javascript",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // `WEFT_WEB_ASSETS=<repo>/web`: the same files, read from disk on each
    // request, so the page can be changed without restarting the server (and
    // ending its sessions). Only the names above.
    let fresh = std::env::var_os("WEFT_WEB_ASSETS").and_then(|dir| {
        let dir = PathBuf::from(dir);
        std::fs::read(dir.join(name)).ok()
    });
    // The vendored library never changes under its name; the page's own
    // files are checked each time (cheaply: the service worker answers first).
    let cache = if fresh.is_some() || !name.starts_with("vendor/") {
        "no-cache"
    } else {
        "max-age=31536000, immutable"
    };
    let headers = [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)];
    match fresh {
        Some(bytes) => (headers, bytes).into_response(),
        None => (headers, body).into_response(),
    }
}

// ---- The list, ending a session, the folders ---------------------------------------

#[derive(Serialize)]
struct SessionInfo {
    id: String,
    folder: String,
    name: String,
    started: String,
    alive: bool,
    cols: u16,
    rows: u16,
}

fn info(p: &Pty) -> SessionInfo {
    let (cols, rows) = p.size();
    SessionInfo {
        id: p.id.clone(),
        folder: p.folder.display().to_string(),
        name: folder_name(&p.folder),
        started: p.started.to_string(),
        alive: p.alive(),
        cols,
        rows,
    }
}

fn folder_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

async fn list_sessions(State(ctx): State<Ctx>) -> Response {
    let all: Vec<SessionInfo> = ctx.app.sessions.list().iter().map(|p| info(p)).collect();
    axum::Json(all).into_response()
}

async fn end_session(State(ctx): State<Ctx>, UrlPath(id): UrlPath<String>) -> StatusCode {
    if ctx.app.sessions.remove(&id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn list_folders(State(ctx): State<Ctx>) -> Response {
    let mut folders: Vec<_> = std::fs::read_dir(&ctx.app.root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.join(".git").exists())
                .map(|p| json!({ "path": p.display().to_string(), "name": folder_name(&p) }))
                .collect()
        })
        .unwrap_or_default();
    folders.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    axum::Json(folders).into_response()
}

// ---- The terminal stream ----------------------------------------------------------

/// The viewer's first message: which session, and its size.
#[derive(Deserialize, Default)]
pub struct Hello {
    /// Join this session.
    pub id: Option<String>,
    /// Or this folder's live one, started if there is none.
    pub folder: Option<String>,
    /// A new session even if the folder has a live one.
    #[serde(default)]
    pub new: bool,
    /// Start by resuming this saved conversation.
    pub resume: Option<String>,
    /// Terminal only: approve every tool call.
    #[serde(default)]
    pub yes: bool,
    #[serde(default)]
    pub think: bool,
    /// Terminal only: the config file and the environment to start with.
    pub config: Option<String>,
    pub env: Option<Vec<(String, String)>>,
    pub cols: u16,
    pub rows: u16,
    /// Send the recent output first (browsers, which keep a scrollback of their own).
    #[serde(default)]
    pub replay: bool,
}

/// Messages from a viewer after the hello.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Control {
    Resize([u16; 2]),
}

async fn ws(State(ctx): State<Ctx>, up: WebSocketUpgrade) -> Response {
    up.max_frame_size(256 * 1024)
        .max_message_size(256 * 1024)
        .on_upgrade(move |sock| stream(ctx, sock))
}

async fn stream(ctx: Ctx, mut sock: WebSocket) {
    let hello = match tokio::time::timeout(Duration::from_secs(10), sock.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Hello>(&t),
        _ => return,
    };
    let hello = match hello {
        Ok(h) => h,
        Err(e) => return refuse(sock, &format!("a bad hello: {e}")).await,
    };
    let (pty, mut rx, replay, joined) = match open(&ctx, &hello) {
        Ok(Opened::Joined(pty)) => {
            let j = pty.join(hello.replay && pty.size().0 == hello.cols);
            (pty, j.rx, j.replay, true)
        }
        Ok(Opened::Started(pty, rx)) => (pty, rx, bytes::Bytes::new(), false),
        Err(e) => return refuse(sock, &format!("{e:#}")).await,
    };
    let hi = json!({ "session": info(&pty) }).to_string();
    if sock.send(Message::Text(hi.into())).await.is_err() {
        return;
    }
    if !replay.is_empty() && sock.send(Message::Binary(replay)).await.is_err() {
        return;
    }
    let mut mine = (hello.cols, hello.rows);
    // A joiner's screen starts from weft drawing itself again for it.
    if joined && !pty.resize(mine.0, mine.1) {
        pty.repaint();
    }

    let mut exited = pty.exited();
    loop {
        tokio::select! {
            biased;
            out = rx.recv() => match out {
                Ok(b) => {
                    // Whatever else is already waiting goes in the same
                    // message: a redraw arrives in many small reads.
                    let mut batch = bytes::BytesMut::from(&b[..]);
                    while batch.len() < 256 * 1024 {
                        match rx.try_recv() {
                            Ok(more) => batch.extend_from_slice(&more),
                            Err(_) => break,
                        }
                    }
                    if sock.send(Message::Binary(batch.freeze())).await.is_err() { break }
                }
                // Too slow to keep up: start this viewer (and the rest) over.
                Err(RecvError::Lagged(_)) => pty.repaint(),
                Err(RecvError::Closed) => break,
            },
            // Rust: the watch's guard is not Send, so it is read and dropped
            // inside this future; only the number comes out.
            code = async { exited.wait_for(|c| c.is_some()).await.ok().and_then(|c| *c) } => {
                let code = code.unwrap_or(1);
                while let Ok(b) = rx.try_recv() {
                    let _ = sock.send(Message::Binary(b)).await;
                }
                let _ = sock.send(Message::Text(json!({ "exit": code }).to_string().into())).await;
                let _ = sock.send(Message::Close(None)).await;
                break;
            }
            msg = sock.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    // The server answers weft's cursor questions itself (see
                    // `pty`); a viewer's own answer is stale, and not typing.
                    if is_cursor_report(&b) {
                        continue;
                    }
                    // Whoever types sets the size: the screen follows them.
                    if pty.size() != mine {
                        pty.resize(mine.0, mine.1);
                    }
                    pty.write(b.to_vec());
                }
                Some(Ok(Message::Text(t))) => {
                    if let Ok(Control::Resize([c, r])) = serde_json::from_str(&t) {
                        mine = (c, r);
                        pty.resize(c, r);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
}

/// Whether the bytes are only a terminal's answer to "where is the cursor?":
/// one or more `ESC [ row ; col R`.
fn is_cursor_report(b: &[u8]) -> bool {
    let mut rest = b;
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let Some(after) = rest.strip_prefix(b"\x1b[") else {
            return false;
        };
        let Some(end) = after.iter().position(|&c| c == b'R') else {
            return false;
        };
        let (nums, tail) = (&after[..end], &after[end + 1..]);
        let mut parts = nums.split(|&c| c == b';');
        let ok =
            |p: Option<&[u8]>| p.is_some_and(|p| !p.is_empty() && p.iter().all(u8::is_ascii_digit));
        if !(ok(parts.next()) && ok(parts.next()) && parts.next().is_none()) {
            return false;
        }
        rest = tail;
    }
    true
}

async fn refuse(mut sock: WebSocket, why: &str) {
    let _ = sock
        .send(Message::Text(json!({ "error": why }).to_string().into()))
        .await;
    let _ = sock.send(Message::Close(None)).await;
}

enum Opened {
    Joined(Arc<Pty>),
    Started(Arc<Pty>, tokio::sync::broadcast::Receiver<bytes::Bytes>),
}

fn open(ctx: &Ctx, hello: &Hello) -> Result<Opened> {
    let app = &ctx.app;
    if let Some(id) = &hello.id {
        return match app.sessions.get(id) {
            Some(p) if p.alive() => Ok(Opened::Joined(p)),
            Some(_) => bail!("that session has ended"),
            None => bail!("no such session"),
        };
    }
    let folder = hello
        .folder
        .as_deref()
        .context("no session or folder named")?;
    let folder = allowed_folder(app, ctx.trust, Path::new(folder))?;
    if let Some(id) = &hello.resume
        && !valid_id(id)
    {
        bail!("a session id is letters, digits, - and _");
    }
    if !hello.new
        && hello.resume.is_none()
        && let Some(p) = app.sessions.live_in(&folder)
    {
        return Ok(Opened::Joined(p));
    }
    let local = ctx.trust == Trust::Local;
    let config = match (&hello.config, local) {
        (Some(c), true) => Some(PathBuf::from(c)),
        _ => app.config.clone(),
    };
    let mut args = Vec::new();
    if let Some(config) = config {
        args.extend(["--config".to_string(), config.display().to_string()]);
    }
    if hello.yes && local {
        args.push("--yes".into());
    }
    if hello.think {
        args.push("--think".into());
    }
    if let Some(id) = &hello.resume {
        args.extend(["resume".into(), id.clone()]);
    }
    let (pty, rx) = app.sessions.start(Spawn {
        program: app.weft.clone(),
        args,
        folder,
        env: if local { hello.env.clone() } else { None },
        cols: hello.cols,
        rows: hello.rows,
    })?;
    eprintln!(
        "weft-web · started {} in {} ({:?})",
        &pty.id[..8],
        pty.folder.display(),
        ctx.trust
    );
    Ok(Opened::Started(pty, rx))
}

/// The folder, resolved. A browser may only name one inside the root, with
/// links followed first, so a link cannot lead out of it.
fn allowed_folder(app: &App, trust: Trust, folder: &Path) -> Result<PathBuf> {
    let real = folder
        .canonicalize()
        .with_context(|| format!("no folder {}", folder.display()))?;
    if !real.is_dir() {
        bail!("{} is not a folder", real.display());
    }
    if trust == Trust::Remote && !real.starts_with(&app.root) {
        bail!(
            "only folders inside {} can be opened here",
            app.root.display()
        );
    }
    Ok(real)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(owner: Option<&str>, allow_loopback: bool, root: &Path) -> App {
        App {
            sessions: Sessions::default(),
            root: root.canonicalize().unwrap(),
            weft: "/bin/sh".into(),
            config: None,
            owner: owner.map(str::to_string),
            hosts: vec![
                "127.0.0.1".into(),
                "localhost".into(),
                "mac.tail.ts.net".into(),
            ],
            allow_loopback,
        }
    }

    fn hdrs(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn the_owner_through_tailscale_gets_in() {
        let a = app(Some("me@x"), false, &std::env::temp_dir());
        let h = hdrs(&[
            ("host", "mac.tail.ts.net:8446"),
            ("tailscale-user-login", "me@x"),
        ]);
        assert_eq!(admit(&a, &Method::GET, &h), Ok(()));
    }

    #[test]
    fn someone_else_on_the_tailnet_does_not() {
        let a = app(Some("me@x"), true, &std::env::temp_dir());
        let h = hdrs(&[
            ("host", "mac.tail.ts.net:8446"),
            ("tailscale-user-login", "them@y"),
        ]);
        assert!(admit(&a, &Method::GET, &h).is_err());
    }

    #[test]
    fn plain_loopback_needs_the_flag() {
        let h = hdrs(&[("host", "127.0.0.1:8510")]);
        let closed = app(Some("me@x"), false, &std::env::temp_dir());
        assert!(admit(&closed, &Method::GET, &h).is_err());
        let open = app(Some("me@x"), true, &std::env::temp_dir());
        assert_eq!(admit(&open, &Method::GET, &h), Ok(()));
    }

    #[test]
    fn a_rebound_host_name_is_refused() {
        let a = app(Some("me@x"), true, &std::env::temp_dir());
        let h = hdrs(&[("host", "evil.example:8510")]);
        assert_eq!(admit(&a, &Method::GET, &h), Err("unknown host"));
    }

    #[test]
    fn another_sites_socket_or_post_is_refused() {
        let a = app(Some("me@x"), false, &std::env::temp_dir());
        let ws_from = |origin: &str| {
            hdrs(&[
                ("host", "mac.tail.ts.net:8446"),
                ("tailscale-user-login", "me@x"),
                ("upgrade", "websocket"),
                ("origin", origin),
            ])
        };
        assert_eq!(
            admit(&a, &Method::GET, &ws_from("https://evil.example")),
            Err("another site")
        );
        assert_eq!(
            admit(&a, &Method::GET, &ws_from("https://mac.tail.ts.net:8446")),
            Ok(())
        );
        let delete_without_origin = hdrs(&[
            ("host", "mac.tail.ts.net:8446"),
            ("tailscale-user-login", "me@x"),
        ]);
        assert_eq!(
            admit(&a, &Method::DELETE, &delete_without_origin),
            Err("no origin")
        );
    }

    #[test]
    fn a_browser_stays_inside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("proj");
        std::fs::create_dir(&inside).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let a = app(None, false, root.path());
        assert!(allowed_folder(&a, Trust::Remote, &inside).is_ok());
        assert!(allowed_folder(&a, Trust::Remote, outside.path()).is_err());
        assert!(allowed_folder(&a, Trust::Remote, &root.path().join("link")).is_err());
        assert!(allowed_folder(&a, Trust::Remote, &inside.join("../..")).is_err());
        // A terminal on this Mac may open any folder, as `weft` could anyway.
        assert!(allowed_folder(&a, Trust::Local, outside.path()).is_ok());
    }

    #[test]
    fn session_ids_are_plain() {
        assert!(valid_id("20261103-abc_1"));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a b"));
        assert!(!valid_id(""));
    }

    #[test]
    fn cursor_reports_are_told_from_typing() {
        assert!(is_cursor_report(b"\x1b[12;1R"));
        assert!(is_cursor_report(b"\x1b[12;1R\x1b[3;40R"));
        assert!(!is_cursor_report(b"\x1b[A"));
        assert!(!is_cursor_report(b"y"));
        assert!(!is_cursor_report(b"\x1b[12;1Rx"));
        assert!(!is_cursor_report(b""));
    }

    #[test]
    fn host_names_lose_their_port() {
        assert_eq!(host_name("127.0.0.1:8510"), "127.0.0.1");
        assert_eq!(host_name("mac.tail.ts.net"), "mac.tail.ts.net");
    }
}
