# <img src="web/icon-180.png" width="32" alt=""> weft-web

[weft](https://github.com/akuttruff/weft) on a phone. `weft-web serve` keeps weft sessions running on the
Mac, each in its own pseudo-terminal, and a browser on the tailnet draws them
with xterm.js. `weft-web` in a terminal joins the same session, so a run you
start at the desk can be watched, typed into and approved from the phone, and
the run goes on when you close the terminal or lock the phone.

weft knows nothing of any of this. Plain `weft` runs as it always has, never
through the server; `weft-web` is how a run goes on the phone.

## Use

```bash
weft-web                    # join this folder's session, or start one; ctrl-] leaves it running
weft-web --new              # a second session in this folder
weft-web resume <id>        # start a session continuing a saved weft conversation (`weft sessions`)
weft-web attach <id>        # join a session by its id
weft-web -y / --think / --config FILE   # passed to the weft it starts
```

On the phone, open `https://<mac>.<tailnet>.ts.net:8446` and Add to Home
Screen. The first page lists the running sessions and the folders under
`~/Code`. Tap one to join or start it. A key bar above the keyboard has the
keys a phone lacks: esc, shift-tab, y / a / n, the arrows, `/`, ctrl-c and
ctrl-j. A− and A+ set the text size.

Whoever typed last sets the screen's size, and weft redraws for them.

To move a session that is already running in plain `weft`, type `/handoff`
in it. weft compacts, saves and becomes `weft-web resume <id>` in the same
terminal, given this in weft's config:

```toml
[handoff]
command = ["weft-web", "--config", "{config}", "resume", "{id}"]
```

Inside a weft-web session, `/handoff` just carries on in place: the session is
already on the phone. Each session has `WEFT_WEB_SESSION` set to its id, so a
`weft-web` run inside it knows.

## Setup

```bash
scripts/deploy-web.sh                                     # install, and run `weft-web serve` under launchd on 127.0.0.1:8510
tailscale serve --bg --https=8446 http://127.0.0.1:8510   # once; tailnet only
```

Run `scripts/deploy-web.sh` again after a change. A restart ends every session
the server runs, so the script refuses while any are live; `FORCE=1`
overrides that. To try a change to the page without a restart (and without
ending sessions), start the server with `WEFT_WEB_ASSETS=<this repo>/web`, and
it reads the page's files from disk.

Sessions run whichever `weft` is on PATH. A terminal brings its own
environment; a session started from the phone gets the PATH the service was
deployed with.

## Who can get in

A session is a shell running as you.

- A terminal joins over a Unix socket only you can open
  (`~/.local/state/weft-web/sock`, mode 0600).
- A browser comes in through `tailscale serve`. The server checks the tailnet
  login that serve vouches for against yours, which the deploy script reads
  once. A request straight to 127.0.0.1 is refused unless the server was
  started with `--allow-loopback`.
- The server refuses any host name it doesn't answer to, which blocks DNS
  rebinding.
- A socket or a change has to come from the page's own origin, so another
  site open in the same browser can't drive a session.
- A browser can start sessions only in folders under `--root` (default
  `~/Code`), after links are resolved, and can never turn on `-y`.
- The page loads nothing from elsewhere: it has a strict content security
  policy and never inserts session data as HTML.
- Never `tailscale funnel` it: that would put it on the internet.

## How it works

- **`src/pty.rs`** is a session.
  - The program runs in a pseudo-terminal. The output is read on a thread and
    sent to every viewer through a `broadcast` channel, with a ring of recent
    output kept under the same lock for a browser that joins later.
  - Input goes to a writer thread.
  - The server keeps a model of the screen (`vt100`) and answers weft's
    "where is the cursor?" itself, as tmux does. The answer never depends on
    which viewer is watching, or on anyone watching at all.
- **`src/server.rs`** is an axum server on two listeners: the Unix socket, and
  TCP behind the guard.
  - Output that has piled up goes to a viewer in one message.
  - TCP sends small writes at once (`TCP_NODELAY`).
- **`src/attach.rs`** is the terminal side: raw mode, the bytes passed through
  both ways, and SIGWINCH sent on as a resize.
- **`web/`** is the page.
  - The terminal changes size only when the width or the text size changes.
    When the keyboard comes up the terminal shrinks once, and after that the
    keyboard going down and up again leaves it alone, since every size change
    makes weft redraw.
  - A reconnect keeps what is on the screen instead of replaying it.
  - The service worker answers from its cache and refreshes behind it, so
    changes to the page show on the next open.
