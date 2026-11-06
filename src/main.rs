//! `weft-web`: weft sessions a phone can join.
//!
//! `weft-web serve` runs under launchd and keeps weft sessions in
//! pseudo-terminals; a browser on the tailnet draws them. `weft-web` in a
//! terminal joins this folder's session (starting one if there is none), so
//! the same run is on the Mac and on the phone. Plain `weft` is untouched and
//! never goes through any of this.

mod attach;
mod pty;
mod server;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// weft's config file, for a session this starts [default: weft's own]
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Approve every tool call without asking (a session this starts)
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    /// Let the worker think before each step (a session this starts)
    #[arg(long, global = true)]
    think: bool,

    /// A new session, even if this folder has a live one
    #[arg(long, global = true)]
    new: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Join a session: this folder's live one (started if there is none), or
    /// the one with this id. ctrl-] leaves it running.
    Attach { id: Option<String> },
    /// Start a session continuing a saved weft conversation (`weft sessions`
    /// lists them), and join it.
    Resume { id: String },
    /// The server: sessions for a browser over Tailscale and for `weft-web`.
    Serve {
        /// The port browsers reach, on 127.0.0.1
        #[arg(long, default_value_t = server::DEFAULT_PORT)]
        port: u16,
        /// The folders a browser may open sessions in [default: ~/Code]
        #[arg(long)]
        root: Option<PathBuf>,
        /// The tailnet login let in [default: this Mac's, from `tailscale status`]
        #[arg(long)]
        owner: Option<String>,
        /// This Mac's tailnet name [default: from `tailscale status`]
        #[arg(long)]
        tailnet_name: Option<String>,
        /// Let a browser on this Mac in at 127.0.0.1 without a tailnet login
        #[arg(long)]
        allow_loopback: bool,
        /// The weft to run [default: `weft` on the session's PATH]
        #[arg(long, default_value = "weft")]
        weft: PathBuf,
    },
}

/// `weft resume <id>` in place of this process, with the flags given.
fn resume_in_place(
    id: &str,
    config: Option<&std::path::Path>,
    yes: bool,
    think: bool,
) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("weft");
    if let Some(config) = config {
        cmd.arg("--config").arg(config);
    }
    if yes {
        cmd.arg("--yes");
    }
    if think {
        cmd.arg("--think");
    }
    // Rust: `exec` replaces this process; it only returns if it failed.
    let err = cmd.arg("resume").arg(id).exec();
    anyhow::anyhow!("could not run weft: {err}")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (id, resume) = match args.command {
        Some(Command::Serve {
            port,
            root,
            owner,
            tailnet_name,
            allow_loopback,
            weft,
        }) => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            return server::serve(server::Options {
                port,
                root: root.unwrap_or_else(|| home.join("Code")),
                owner,
                tailnet_name,
                allow_loopback,
                weft,
                config: args.config,
            })
            .await;
        }
        Some(Command::Attach { id }) => (id, None),
        Some(Command::Resume { id }) => (None, Some(id)),
        None => (None, None),
    };
    if let Some(within) = std::env::var_os(pty::SESSION_VAR) {
        // Already inside a session, so whatever runs here is on the phone
        // already. weft's `/handoff` lands here when it is run under
        // weft-web: the resume just carries on in place, as `/restart` would.
        let Some(id) = resume else {
            anyhow::bail!(
                "this terminal is already weft-web session {}; ctrl-] in the outer terminal leaves it",
                within.to_string_lossy()
            );
        };
        return Err(resume_in_place(
            &id,
            args.config.as_deref(),
            args.yes,
            args.think,
        ));
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!("weft-web joins a session in a terminal; this has none");
    }
    let folder = std::env::current_dir()?;
    let left = attach::attach(attach::Attach {
        id,
        folder: &folder,
        new: args.new,
        resume: resume.clone(),
        yes: args.yes,
        think: args.think,
        config: args.config.as_deref(),
    })
    .await
    // weft's `/handoff` has already left by now: say how to get back.
    .map_err(|e| match &resume {
        Some(id) => e.context(format!(
            "the session is saved; `weft resume {id}` picks it up here"
        )),
        None => e,
    })?;
    // Rust: stdin's reader thread is still blocked on a read; exiting here
    // does not wait for a key the runtime would otherwise need.
    std::process::exit(match left {
        attach::Left::Detached => 0,
        attach::Left::Ended(code) => code as i32,
    });
}
