#!/bin/sh
# Install weft-web and (re)start the persistent `weft-web serve`
# (launchd: com.amie.weft-web), the one Tailscale serves. weft itself is
# installed from its own repository; the sessions run whichever `weft` is on
# PATH.
#
# Restarting the server ends every session it runs, so this refuses while
# any are live. FORCE=1 scripts/deploy-web.sh restarts anyway.
#
# Once, by hand, to reach it from the phone (tailnet only; never `funnel`):
#   tailscale serve --bg --https=8446 http://127.0.0.1:8510
set -e
cd "$(dirname "$0")/.."
LABEL=com.amie.weft-web
PORT=8510
SOCK="${WEFT_WEB_SOCK:-$HOME/.local/state/weft-web/sock}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

if [ "${FORCE:-}" != 1 ] && [ -S "$SOCK" ]; then
  # The Unix socket is the terminal side: no tailnet login needed to ask.
  live=$(curl -sS -m 2 --unix-socket "$SOCK" http://localhost/api/sessions 2>/dev/null \
    | grep -o '"alive":true' | wc -l | tr -d ' ' || true)
  if [ "${live:-0}" -gt 0 ]; then
    echo "weft-web: $live session(s) running; a restart would end them." >&2
    echo "          Leave them (ctrl-c twice in each), or run: FORCE=1 scripts/deploy-web.sh" >&2
    exit 1
  fi
fi

# The tailnet login let in and this Mac's tailnet name, asked here, in a
# terminal: the Tailscale app's CLI does not answer the launchd job itself.
TS=$(command -v tailscale || echo /Applications/Tailscale.app/Contents/MacOS/Tailscale)
read -r OWNER NAME <<TS_EOF
$("$TS" status --json | python3 -c 'import json,sys; d=json.load(sys.stdin); s=d["Self"]; print(d["User"][str(s["UserID"])]["LoginName"], s["DNSName"].rstrip("."))')
TS_EOF
if [ -z "$OWNER" ] || [ -z "$NAME" ]; then
  echo "weft-web: could not read the tailnet login and name from \`$TS status\`" >&2
  exit 1
fi

cargo install --path . --locked

sed -e "s#@HOME@#$HOME#g" -e "s#@PATH@#$PATH#g" -e "s#@OWNER@#$OWNER#g" -e "s#@TAILNET_NAME@#$NAME#g" \
  scripts/$LABEL.plist > "$PLIST.new"
if [ -f "$PLIST" ] && cmp -s "$PLIST" "$PLIST.new"; then
  rm "$PLIST.new"
  launchctl kickstart -k "gui/$(id -u)/$LABEL"
else
  mv "$PLIST.new" "$PLIST"
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
fi

for _ in 1 2 3 4 5 6 7 8 9 10; do
  if [ -S "$SOCK" ] && curl -sfo /dev/null -m 2 --unix-socket "$SOCK" http://localhost/api/sessions; then
    echo "weft-web: deployed for $OWNER · \`weft-web\` joins from a terminal · phone: https://$NAME:8446"
    exit 0
  fi
  sleep 1
done
echo "weft-web: started, but not answering at $SOCK -- see ~/Library/Logs/$LABEL.log" >&2
exit 1
