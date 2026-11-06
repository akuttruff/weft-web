//! `weft-web`: this terminal joins a session the server owns (this folder's,
//! or a new one), so the run goes on when the terminal closes and a browser
//! can watch and drive the same one.
//!
//! The terminal goes raw and the bytes pass straight through both ways; the
//! weft drawing the screen is the one in the server's pseudo-terminal. ctrl-]
//! leaves it running.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_tungstenite::tungstenite::Message;

use crate::server::socket_path;

/// ctrl-]: leave the session running.
const DETACH: u8 = 0x1d;

pub struct Attach<'a> {
    /// A session id; otherwise this folder's live session (or a new one).
    pub id: Option<String>,
    pub folder: &'a Path,
    pub new: bool,
    pub resume: Option<String>,
    pub yes: bool,
    pub think: bool,
    pub config: Option<&'a Path>,
}

/// How the attachment ended.
pub enum Left {
    Detached,
    /// The session's weft exited, with this code.
    Ended(u32),
}

pub async fn attach(a: Attach<'_>) -> Result<Left> {
    let path = socket_path()?;
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| {
            format!(
                "weft-web is not serving ({} does not answer); start it with scripts/deploy-web.sh, or just run weft",
                path.display()
            )
        })?;
    let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", stream)
        .await
        .context("opening the session stream")?;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let hello = json!({
        "id": a.id,
        "folder": a.folder.display().to_string(),
        "new": a.new,
        "resume": a.resume,
        "yes": a.yes,
        "think": a.think,
        "config": a.config.map(|c| c.display().to_string()),
        // The session runs with this terminal's environment (PATH above all),
        // not the background service's.
        "env": std::env::vars().collect::<Vec<_>>(),
        "cols": cols,
        "rows": rows,
        "replay": false,
    });
    ws.send(Message::Text(hello.to_string().into())).await?;

    // The first answer names the session, or says why there is none.
    match ws.next().await {
        Some(Ok(Message::Text(t))) => {
            let v: serde_json::Value = serde_json::from_str(&t)?;
            if let Some(e) = v["error"].as_str() {
                bail!("{e}");
            }
        }
        _ => bail!("the server closed the session stream"),
    }

    crossterm::terminal::enable_raw_mode()?;
    let result = pump(&mut ws).await;
    crossterm::terminal::disable_raw_mode()?;
    let left = result?;
    let mut out = std::io::stdout();
    if let Left::Detached = left {
        // weft's screen modes belong to the session, not to this terminal;
        // and the cursor sits in weft's input box, so the note goes below it.
        let _ = write!(
            out,
            "\x1b[?2004l\x1b[?25h\x1b[999;1H\r\n\x1b[2mdetached · it keeps running · `weft-web` rejoins it\x1b[0m\r\n"
        );
    }
    let _ = out.flush();
    Ok(left)
}

async fn pump<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Result<Left>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::signal::unix::{SignalKind, signal};
    let mut winch = signal(SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            n = stdin.read(&mut buf) => {
                let n = n?;
                if n == 0 {
                    return Ok(Left::Detached);
                }
                let typed = &buf[..n];
                if let Some(at) = typed.iter().position(|&b| b == DETACH) {
                    if at > 0 {
                        ws.send(Message::Binary(typed[..at].to_vec().into())).await?;
                    }
                    let _ = ws.close(None).await;
                    return Ok(Left::Detached);
                }
                ws.send(Message::Binary(typed.to_vec().into())).await?;
            }
            _ = winch.recv() => {
                if let Ok((c, r)) = crossterm::terminal::size() {
                    ws.send(Message::Text(json!({ "resize": [c, r] }).to_string().into())).await?;
                }
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    stdout.write_all(&b)?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Text(t))) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                    if let Some(code) = v["exit"].as_u64() {
                        return Ok(Left::Ended(code as u32));
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Ok(Left::Ended(0)),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
        }
    }
}
