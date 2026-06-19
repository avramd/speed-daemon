# Speed Daemon

A compact home-network probe dashboard: continuously pings a set of tagged targets
(LAN hosts, gateway, ISP, internet) and shows a dense per-target strip-chart — 1px columns,
height = log(RTT), expectation-aware colors, grey where the app wasn't running. Rust core +
Tauri v2; the probing, SQLite history, and client/server networking all run inside the one
app process (no separate daemon).

## Requirements

- macOS (Apple Silicon), Xcode Command Line Tools
- Node + npm
- Rust (rustup). cargo is expected at
  `~/.rustup/toolchains/stable-aarch64-apple-darwin/bin` — the scripts add it to `PATH`.

## Run / build manually

```sh
npm install
npm run tauri dev      # run in development (needs cargo on PATH)
npm run tauri build    # produce a release .app + .dmg under src-tauri/target/release/bundle/
```

If `cargo` isn't on your shell PATH, prefix commands with:
`env PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" …`

## Deploy to /Applications — `bin/deploy`

```sh
bin/deploy              # build, stage, then install with ~1s downtime (handoff)
bin/deploy --swap-only  # skip the build; install an already-built bundle
bin/deploy --build-only # just build and print the .app/.dmg paths (installs nothing)
```

The compile and copy happen while the currently-installed app keeps polling, so only the
final swap is downtime. Once a handoff-aware build is installed it uses a **near-zero-downtime
handoff** (launches the new instance with `open -n`; it takes over in ~1s and the old one
quits itself). The **first** deploy of the handoff build can't hand off, so `bin/deploy`
detects that and does a quick quit+swap that one time.

The app installs by copy (not from the DMG), so there's no Gatekeeper quarantine prompt.

## Background app

The release app runs in the background: a menu-bar (tray) icon, hide-on-close (closing the
window keeps it collecting), autostart at login, and a macOS *reopen* handler so ⌘-tab /
LaunchBar / Dock / a relaunch bring the window back.

## Dev viewer (no disruption to the real collector) — `bin/dev-mode`

A read-only dev instance shares the installed app's real config + history (so it shows your
real targets and history) but never polls, writes, or networks.

```sh
bin/dev-mode start     # launch the read-only viewer (your real data), backgrounded
bin/dev-mode hide      # macOS-hide it (⌘-tab / LaunchBar / Dock unhide it natively)
bin/dev-mode show      # reveal + focus it
bin/dev-mode status    # running? hidden?
bin/dev-mode restart
bin/dev-mode stop
```

The dev window is labeled with a red **DEV** badge and a "(dev)" title. `hide` survives the
dev server's rebuild-relaunches, so backend churn won't pop a window in front of you.

## Data, config, and logs

Stored in the app config dir (override with `SPEED_DAEMON_DIR` for an isolated instance):

```
~/Library/Application Support/org.est.speeddaemon/
  config.toml     targets, tag thresholds, node identity/mode, paired peers, theme
  history.db      SQLite sample history (WAL), retained ~1 week
  poller.sem      the active poller's "<pid> <checkin_ms>" (handoff)
  takeover.sem    a newly-launched instance waiting to take over (handoff)
  handoff.log     append-only record of every handoff transition (+ echoed to stderr)
```

Environment variables:
- `SPEED_DAEMON_DIR` — use a different data dir (dev isolation).
- `SPEED_DAEMON_READONLY` — share the real data dir but don't poll/write/network (viewer).

### Debugging a handoff

`handoff.log` records each transition as `<epoch-ms> pid=<pid> <event>`, e.g.
`claimed active poller (cold start)`, `waiting to take over from pid 1234`,
`handing off to pid 5678; releasing and quitting`. Tail it to inspect a handover after the
fact without reproducing it:

```sh
tail -f "$HOME/Library/Application Support/org.est.speeddaemon/handoff.log"
```

## Client / server (distributed monitoring, phase 1)

In the **Network** (🛰) panel, switch a machine to **server** mode to discover clients on the
LAN (UDP broadcast), invite one (custom message + 5-minute timeout), and — once paired (a
shared secret is exchanged and checked on every message) — assign it your targets. Clients
merge assigned hosts into their own probing (de-duping what they already poll) and report
results every 10s; the server shows them as source-tagged rows with filter/sort.
