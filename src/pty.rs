//! Sessions the server owns: each one a weft (or, in tests, any program)
//! running in its own pseudo-terminal, outliving whoever is watching it.
//!
//! The output goes three ways at once, under one lock so no viewer misses or
//! repeats a byte: into a ring of recent output (replayed to a browser that
//! joins at the same width), and out to every viewer through a broadcast
//! channel. Input from any viewer goes straight to the terminal.
//!
//! The server is the terminal of record, as tmux is: it keeps its own model of
//! the screen and answers weft's "where is the cursor?" itself, and viewers
//! never see the question. Left to the viewers, the answer would depend on
//! who is watching, at what size, and whether anyone is at all; with none,
//! weft would wait, give up, and end.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, watch};

/// Recent output kept for a browser that joins later: enough for a few
/// screens of history, little enough to send to a phone at once.
pub const RING_BYTES: usize = 256 * 1024;
/// Sessions running at once.
pub const MAX_LIVE: usize = 8;

/// Set in every session's environment to the session's id.
pub const SESSION_VAR: &str = "WEFT_WEB_SESSION";

/// What to run, where, and at what size.
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub folder: PathBuf,
    /// The whole environment, when the caller brings one (a terminal on this
    /// Mac); otherwise the server's own.
    pub env: Option<Vec<(String, String)>>,
    pub cols: u16,
    pub rows: u16,
}

pub struct Pty {
    pub id: String,
    pub folder: PathBuf,
    pub started: jiff::Timestamp,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Input for the terminal, written in order by a thread of its own: a
    /// keystroke never waits for a thread to be found, and never blocks.
    input: std::sync::mpsc::Sender<Vec<u8>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The ring and the broadcast, behind one lock: see the module note.
    out: Mutex<Out>,
    size: Mutex<(u16, u16)>,
    /// The screen as weft drew it, at the terminal's size: where the cursor is.
    screen: Mutex<vt100::Parser>,
    alive: AtomicBool,
    /// The exit code once the program has ended.
    exit: watch::Sender<Option<u32>>,
}

struct Out {
    ring: Vec<u8>,
    tx: broadcast::Sender<Bytes>,
}

/// What a viewer gets on joining: the output so far (empty unless asked for),
/// then everything after it.
pub struct Joined {
    pub replay: Bytes,
    pub rx: broadcast::Receiver<Bytes>,
}

impl Pty {
    /// Start the program. The first viewer is subscribed before the first
    /// byte is read, so it sees everything, including weft's question about
    /// where the cursor is, which that viewer's terminal answers.
    pub fn spawn(id: String, spec: Spawn) -> Result<(Arc<Pty>, broadcast::Receiver<Bytes>)> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows.max(2),
                cols: spec.cols.max(2),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pseudo-terminal")?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.folder);
        if let Some(env) = &spec.env {
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // So a `weft-web` run inside the session knows it is already in one.
        cmd.env(SESSION_VAR, &id);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {}", spec.program.display()))?;
        // Rust: the slave end must close here, or the reader never sees the
        // end of the output when the program exits.
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (input, typed) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for bytes in typed {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let killer = child.clone_killer();

        let (tx, rx) = broadcast::channel(1024);
        let (exit, _) = watch::channel(None);
        let pty = Arc::new(Pty {
            id,
            folder: spec.folder,
            started: jiff::Timestamp::now(),
            master: Mutex::new(pair.master),
            input,
            killer: Mutex::new(killer),
            out: Mutex::new(Out {
                ring: Vec::new(),
                tx,
            }),
            size: Mutex::new((spec.cols.max(2), spec.rows.max(2))),
            screen: Mutex::new(vt100::Parser::new(nz(spec.rows), nz(spec.cols), 0)),
            alive: AtomicBool::new(true),
            exit,
        });

        // Rust: plain threads, since both calls block; each holds its own Arc.
        let p = pty.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let mut asked = Asked::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let shown = p.answer_cursor_queries(&mut asked, &buf[..n]);
                        if !shown.is_empty() {
                            p.emit(&shown);
                        }
                    }
                }
            }
        });
        let p = pty.clone();
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
            p.alive.store(false, Ordering::SeqCst);
            // Output still in flight is read before the end is announced: the
            // reader stops once the terminal has nothing more to give.
            std::thread::sleep(std::time::Duration::from_millis(100));
            let _ = p.exit.send(Some(code));
        });
        Ok((pty, rx))
    }

    /// Feed the output to the screen model, answering each cursor question
    /// as it comes, at the point in the output where it was asked. Returns the
    /// output without the questions, for the viewers.
    fn answer_cursor_queries(&self, asked: &mut Asked, bytes: &[u8]) -> Vec<u8> {
        let mut screen = self.screen.lock().unwrap();
        let mut shown = Vec::with_capacity(bytes.len());
        let mut fed = 0;
        for &b in bytes {
            if b == CURSOR_QUERY[asked.0] {
                asked.0 += 1;
                if asked.0 == CURSOR_QUERY.len() {
                    asked.0 = 0;
                    screen.process(&shown[fed..]);
                    fed = shown.len();
                    let (row, col) = screen.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                    let _ = self.input.send(reply.into_bytes());
                }
                continue;
            }
            if asked.0 > 0 {
                // Not the question after all: what was held back is output.
                shown.extend_from_slice(&CURSOR_QUERY[..asked.0]);
                asked.0 = usize::from(b == CURSOR_QUERY[0]);
                if asked.0 == 1 {
                    continue;
                }
            }
            shown.push(b);
        }
        screen.process(&shown[fed..]);
        shown
    }

    fn emit(&self, bytes: &[u8]) {
        let mut out = self.out.lock().unwrap();
        out.ring.extend_from_slice(bytes);
        cap(&mut out.ring, RING_BYTES);
        let _ = out.tx.send(Bytes::copy_from_slice(bytes));
    }

    /// Join as a viewer. `replay` hands over the recent output, minus the
    /// cursor questions in it, which the joining terminal would otherwise
    /// answer long after anyone asked.
    pub fn join(&self, replay: bool) -> Joined {
        let out = self.out.lock().unwrap();
        let replay = if replay {
            Bytes::from(strip_cursor_queries(&out.ring))
        } else {
            Bytes::new()
        };
        Joined {
            replay,
            rx: out.tx.subscribe(),
        }
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn exited(&self) -> watch::Receiver<Option<u32>> {
        self.exit.subscribe()
    }

    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    /// Typed input from any viewer, queued for the writer thread.
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    /// Take a viewer's size. Returns whether it changed. weft lays its
    /// screen out again on a new size, clearing it first, so the recent
    /// output starts over from there: what came before was for another width.
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(2, 1000));
        let mut size = self.size.lock().unwrap();
        if *size == (cols, rows) {
            return false;
        }
        *size = (cols, rows);
        self.set(cols, rows);
        self.out.lock().unwrap().ring.clear();
        true
    }

    /// Have weft draw its screen again at the size it has: one row less and
    /// straight back, inside the time weft waits for a size to settle.
    pub fn repaint(&self) {
        let (cols, rows) = self.size();
        self.set(cols, rows.saturating_sub(1).max(2));
        self.set(cols, rows);
        self.out.lock().unwrap().ring.clear();
    }

    fn set(&self, cols: u16, rows: u16) {
        self.screen.lock().unwrap().set_size(nz(rows), nz(cols));
        let _ = self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().unwrap().kill();
    }
}

const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// How much of a cursor question the output has spelled out so far: it can
/// arrive split across two reads.
#[derive(Default)]
struct Asked(usize);

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n.max(2)).expect("at least 2")
}

/// Keep the last `max` bytes, cut at a line's start where one is near, so a
/// replay does not begin in the middle of an escape sequence.
fn cap(ring: &mut Vec<u8>, max: usize) {
    if ring.len() <= max {
        return;
    }
    let mut from = ring.len() - max;
    if let Some(nl) = ring[from..].iter().take(4096).position(|&b| b == b'\n') {
        from += nl + 1;
    }
    ring.drain(..from);
}

fn strip_cursor_queries(bytes: &[u8]) -> Vec<u8> {
    const Q: &[u8] = b"\x1b[6n";
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(Q) {
            i += Q.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Every session, by id.
#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Arc<Pty>>>,
}

impl Sessions {
    pub fn get(&self, id: &str) -> Option<Arc<Pty>> {
        self.map.lock().unwrap().get(id).cloned()
    }

    /// The newest live session in this folder.
    pub fn live_in(&self, folder: &Path) -> Option<Arc<Pty>> {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.alive() && p.folder == folder)
            .max_by_key(|p| p.started)
            .cloned()
    }

    pub fn list(&self) -> Vec<Arc<Pty>> {
        let mut all: Vec<_> = self.map.lock().unwrap().values().cloned().collect();
        all.sort_by_key(|p| std::cmp::Reverse(p.started));
        all
    }

    pub fn live(&self) -> usize {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.alive())
            .count()
    }

    pub fn start(&self, spec: Spawn) -> Result<(Arc<Pty>, broadcast::Receiver<Bytes>)> {
        if self.live() >= MAX_LIVE {
            bail!("{MAX_LIVE} sessions are already running; end one first");
        }
        let (pty, rx) = Pty::spawn(new_id(), spec)?;
        self.map.lock().unwrap().insert(pty.id.clone(), pty.clone());
        Ok((pty, rx))
    }

    /// End a session (if it is still running) and forget it.
    pub fn remove(&self, id: &str) -> bool {
        match self.map.lock().unwrap().remove(id) {
            Some(p) => {
                if p.alive() {
                    p.kill();
                }
                true
            }
            None => false,
        }
    }

    pub fn kill_all(&self) {
        for p in self.map.lock().unwrap().values() {
            if p.alive() {
                p.kill();
            }
        }
    }
}

/// 128 random bits, as hex: an id nobody can guess.
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // Never expected on a Mac; still unguessable enough to be unique.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        b.copy_from_slice(&t.to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sh(script: &str, cols: u16, rows: u16) -> Spawn {
        Spawn {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            folder: std::env::temp_dir(),
            env: None,
            cols,
            rows,
        }
    }

    async fn read_until(rx: &mut broadcast::Receiver<Bytes>, want: &str) -> String {
        let mut got = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !got.contains(want) {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(b)) => got.push_str(&String::from_utf8_lossy(&b)),
                _ => break,
            }
        }
        got
    }

    #[tokio::test]
    async fn output_reaches_the_first_viewer_and_the_end_is_announced() {
        let (pty, mut rx) = Pty::spawn(new_id(), sh("echo hi; exit 3", 80, 24)).unwrap();
        assert!(read_until(&mut rx, "hi").await.contains("hi"));
        let mut exited = pty.exited();
        let code = tokio::time::timeout(Duration::from_secs(5), exited.wait_for(|c| c.is_some()))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(code, 3);
        assert!(!pty.alive());
    }

    #[tokio::test]
    async fn the_program_sees_the_size_and_the_terminal() {
        let (_pty, mut rx) = Pty::spawn(
            new_id(),
            sh(
                "stty size; echo \"term=$TERM in=${WEFT_WEB_SESSION}\"",
                91,
                33,
            ),
        )
        .unwrap();
        let out = read_until(&mut rx, "term=").await;
        let out = read_until_more(out, &mut rx, "256color").await;
        assert!(out.contains("33 91"), "{out:?}");
        assert!(out.contains("term=xterm-256color in="), "{out:?}");
        assert!(!out.contains("in=\r") && !out.contains("in=\n"), "{out:?}");
    }

    async fn read_until_more(
        mut got: String,
        rx: &mut broadcast::Receiver<Bytes>,
        want: &str,
    ) -> String {
        if !got.contains(want) {
            got.push_str(&read_until(rx, want).await);
        }
        got
    }

    #[tokio::test]
    async fn a_resize_reaches_the_program() {
        let (pty, mut rx) = Pty::spawn(new_id(), sh("read _; stty size; read _", 80, 24)).unwrap();
        assert!(pty.resize(50, 20));
        assert!(!pty.resize(50, 20), "the same size again is no change");
        pty.write(b"\n".to_vec());
        let out = read_until(&mut rx, "20 50").await;
        assert!(out.contains("20 50"), "{out:?}");
        pty.kill();
    }

    #[tokio::test]
    async fn input_reaches_the_program_and_a_late_viewer_gets_the_replay() {
        let (pty, mut rx) =
            Pty::spawn(new_id(), sh("read x; echo \"got $x\"; read _", 80, 24)).unwrap();
        pty.write(b"ping\n".to_vec());
        assert!(read_until(&mut rx, "got ping").await.contains("got ping"));
        let late = pty.join(true);
        assert!(String::from_utf8_lossy(&late.replay).contains("got ping"));
        assert!(pty.join(false).replay.is_empty());
        pty.kill();
    }

    #[tokio::test]
    async fn the_server_answers_where_the_cursor_is_and_viewers_never_see_the_question() {
        // Move to row 5, column 10, ask, and print the answer the terminal gave.
        let script = r#"stty raw -echo; printf '\033[5;10H\033[6n'; dd bs=1 count=7 2>/dev/null | od -c | head -1; read _"#;
        let (pty, mut rx) = Pty::spawn(new_id(), sh(script, 80, 24)).unwrap();
        let out = read_until(&mut rx, "R").await;
        assert!(out.contains("5   ;   1   0   R"), "{out:?}");
        assert!(!out.contains("\x1b[6n"), "the question leaked: {out:?}");
        pty.kill();
    }

    #[test]
    fn a_question_split_across_reads_is_still_caught() {
        let (pty, _rx) = Pty::spawn(new_id(), sh("sleep 5", 80, 24)).unwrap();
        let mut asked = Asked::default();
        let a = pty.answer_cursor_queries(&mut asked, b"ab\x1b[");
        let b = pty.answer_cursor_queries(&mut asked, b"6ncd\x1b[1m");
        assert_eq!([a, b].concat(), b"abcd\x1b[1m");
        pty.kill();
    }

    #[test]
    fn the_ring_stays_capped_and_starts_on_a_line() {
        let mut ring = b"aaaa\nbbbb\ncccc\n".to_vec();
        cap(&mut ring, 8);
        assert_eq!(ring, b"cccc\n");
        let mut big = vec![b'x'; RING_BYTES * 2];
        cap(&mut big, RING_BYTES);
        assert_eq!(big.len(), RING_BYTES);
    }

    #[test]
    fn cursor_questions_are_left_out_of_a_replay() {
        assert_eq!(strip_cursor_queries(b"a\x1b[6nb\x1b[6n"), b"ab");
    }

    #[test]
    fn ids_are_long_and_differ() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn sessions_are_found_by_folder_and_capped() {
        let sessions = Sessions::default();
        let (a, _) = sessions.start(sh("sleep 30", 80, 24)).unwrap();
        let found = sessions.live_in(&std::env::temp_dir()).unwrap();
        assert_eq!(found.id, a.id);
        for _ in 1..MAX_LIVE {
            sessions.start(sh("sleep 30", 80, 24)).unwrap();
        }
        assert!(sessions.start(sh("sleep 30", 80, 24)).is_err());
        assert!(sessions.remove(&a.id));
        assert!(sessions.get(&a.id).is_none());
        sessions.kill_all();
    }
}
