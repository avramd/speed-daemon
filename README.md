# Speed Daemon

A compact home-network probe dashboard. A headless background **poller** (`speedd`)
continuously pings a set of tagged targets (LAN hosts, gateway, ISP, internet) and records
RTT/loss to a local SQLite database; a separate **viewer app** renders a dense per-target
strip-chart — 1px columns, height = log(RTT), expectation-aware colors, grey where the poller
wasn't running.

Rust core + Tauri v2. The probing lives in a small **headless daemon, not the GUI** — and that
split is deliberate: macOS deprioritizes a GUI process's sockets under Wi-Fi power-save by
~40ms, which silently inflates every reading. A headless CLI process whose thread blocks in
`recvmsg` stays in the fast path and measures accurately.

## Architecture

- **`speedd`** — the poller. A headless daemon (one blocking thread per target) managed by
  launchd. Owns all probing and writes `history.db`. Reloads its config on `SIGHUP`.
- **Speed Daemon.app** — a read-only viewer/controller. Reads `history.db` to draw; editing
  targets/thresholds writes `config.toml` and signals `speedd` to reload. It never polls.

## Requirements

- macOS (Apple Silicon), Xcode Command Line Tools
- Node + npm
- Rust (rustup). `cargo` is expected at
  `~/.rustup/toolchains/stable-aarch64-apple-darwin/bin`; the scripts add it to `PATH`. If it
  isn't on your shell PATH, prefix commands with
  `env PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" …`.

## The poller — `bin/speedd-ctl`

Install once. It then runs at every login (a LaunchAgent with `RunAtLoad` + `KeepAlive`: it
restarts itself if it dies, and comes back after a reboot once you log in — login-start so it
has your user session's Wi-Fi).

```sh
bin/speedd-ctl install     # build (release), install the binary + LaunchAgent, start it
```

Control it:

| Action | Command |
|---|---|
| **Pause** (stop polling) | `bin/speedd-ctl stop` |
| **Resume** | `bin/speedd-ctl start` |
| **Restart** (kill + relaunch) | `bin/speedd-ctl restart` |
| Reload config without a gap | `bin/speedd-ctl reload` |
| Status | `bin/speedd-ctl status` |
| Tail the log | `bin/speedd-ctl logs` |
| Rebuild + reinstall the binary | `bin/speedd-ctl build` |
| Remove the LaunchAgent (keeps data) | `bin/speedd-ctl uninstall` |

`stop` pauses until you `start` again — or until your next login, since the agent auto-loads
then. For a pause that persists across reboots, use `uninstall` (and `install` to put it back).
While paused, the viewer shows a grey gap for that stretch.

Set `SPEED_DAEMON_DIR` to run against an isolated data dir (dev/testing).

## The viewer app

```sh
npm install
npm run tauri dev      # run the viewer in development
npm run tauri build    # produce a release .app + .dmg under src-tauri/target/release/bundle/
```

To install or update the Dock app, build it and copy the bundle into place (copy install, so
no Gatekeeper quarantine prompt):

```sh
npm run tauri build
ditto "src-tauri/target/release/bundle/macos/Speed Daemon.app" "/Applications/Speed Daemon.app"
```

It runs in the background with a menu-bar (tray) icon and hide-on-close. It's only a viewer, so
closing it doesn't stop data collection — `speedd` keeps polling regardless.

## Data, config, and logs

Stored in the app data dir (override with `SPEED_DAEMON_DIR`):

```
~/Library/Application Support/org.est.speeddaemon/
  speedd          the installed poller binary
  config.toml     targets, tag thresholds, node identity, theme
  history.db      SQLite sample history (WAL), retained ~1 week
  speedd.log      the daemon's stdout/stderr
```

## Deferred

Cross-machine/distributed monitoring (the **Network** panel) is shelved while the poller lives
in `speedd`; that code is dormant and the panel is currently inactive.
