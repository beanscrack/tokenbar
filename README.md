# TokenBar

Local, low-resource token-usage meter for **Hermes + Claude Code + Codex CLI**, shown
in the **system tray tooltip** and as **Discord Rich Presence** ("Playing TokenBar").

Rust, single .exe (~1.4 MB), idle RSS ~12 MB, steady-state cycle ~20 ms (files are
byte-offset cached; each JSONL byte is parsed exactly once). All data stays on your
machine — no network I/O except the local Discord IPC pipe.

## Get it running (fresh machine, ~3 min)

1. Download `tokenbar.exe` from the latest [Release](../../releases) (Windows x64,
   self-contained, no installer, no runtime).
2. Create your own Discord application (free, 60 s):
   <https://discord.com/developers/applications> → **New Application** → name it
   `TokenBar` (this name is what shows as "Playing TokenBar") → copy the
   **Application ID**.
3. Run: `tokenbar.exe --app-id <YOUR_ID>` — this saves the ID, connects to the
   Discord IPC pipe, and starts the tray + presence loop.
4. Upload an App Icon on the portal (General Information → App Icon) so the popout
   row shows art instead of a grey bar — `tools/make_icon.ps1` generates matching
   art (dark tile + t3 heat-bar, 512px). Ctrl+R in Discord afterwards.
5. Open `http://127.0.0.1:53123` in a browser for the live t3-style dashboard
   (tray → Open Dashboard), or run `tokenbar.exe --once` for the console report.

Sources you don't use are auto-detected (missing logs are silent), or disable
explicitly: `--claude 0`, `--codex 0`, `--hermes 0`.

## Sources (verified exact)

| Source   | Buyer of record                                        |
|----------|--------------------------------------------------------|
| Hermes   | `%LOCALAPPDATA%\hermes\state.db` → `session_model_usage` (read-only) |
| Claude Code | `~/.claude/projects/**/*.jsonl` `message.usage`, deduped by `message.id` (stream/sidechain re-emissions) |
| Codex CLI | `~/.codex/sessions/**/rollout-*.jsonl` `total_token_usage` cumulative snapshots (last = session total) |

Claude Desktop / ChatGPT desktop-app have no local token ledger (server-side) — not
supported. ChatGPT **web** is a planned phase 2 via a small browser-extension bridge.

## Use

```
tokenbar.exe                       # tray app (starts loop, Discord if app_id set)
tokenbar.exe --once                # print report to console, update caches, exit
tokenbar.exe --install             # copy to %LOCALAPPDATA%\tokenbar + HKCU Run autorun
tokenbar.exe --uninstall           # remove autorun
tokenbar.exe --app-id <ID>         # Discord Application ID (saved to config)
tokenbar.exe --interval 60         # cycle period, seconds
tokenbar.exe --claude 0|1          # toggle sources (also --codex, --hermes)
tokenbar.exe --verbose             # append cycle log to app.log
```

Tray right-click menu: **Quit** (also clears the Discord presence).

## Discord Rich Presence (one-time, ~60 s)

1. Open https://discord.com/developers/applications (log in).
2. **New Application** → name `TokenBar` → **Create**.
3. Copy the **Application ID** from General Information.
4. Run: `tokenbar.exe --app-id 1234567890123456789`

Your status then shows `Playing TokenBar` with t3-style lines — word labels, compact K/M/B/T, no glyphs:

```
Today · 46.75M in · 700.87M cache read · 2.47M out · 92% hit
30d · 3.59B in · 32.3M out · 50% hit · Claude Code 34% · Codex 35% · Hermes 31%
```

(Details = today, State = 30 days + per-source share; cost appears when > $0.01.
The app name shown is the portal app's name; upload/brand assets are optional.)

## Dashboard (t3-style, local)

The tray serves a dark t3code-style dashboard at **http://127.0.0.1:53123**
(config: `panel = true`, `panel_port = 53123`). Tray menu → *Open Dashboard*.

- Tabs Today / Last 7d / Last 30d / All time; hero = total tokens, four-category
  heat bar (Input / Cache read / Cache write / Output), per-source share bars,
  cache hit % and cost by source — refreshed from `status.json` every 30 s.
- `tokenbar.exe --panel-only 53134` runs the headless server on another port
  (no tray, ignores the single-instance mutex) — used by the test harness.
- Live payload: `%LOCALAPPDATA%\tokenbar\status.json`
  (`windows.{today,d7,d30,all}` → `inp,cr,cw,out,cost,tok,hit,by[]`).

## Files

- exe + config + caches: `%LOCALAPPDATA%\tokenbar\` (`config.toml`, `state-cc.json`,
  `state-cx.json`, `app.log`, `status.json`)
- build: `cargo build --release` (needs MSVC toolchain)

## Semantics

- Day windows follow your local timezone (today = local midnight).
- "today / 7d / 30d" windows day-bucketed; heat: files older than the 45-day retention
  roll into an "older" bucket that only feeds the all-time total.
- Hermes cost columns (when the provider reports them) show as `≈$n` in the tooltip.
- Deleting `%LOCALAPPDATA%\tokenbar\state-*.json` forces a full re-scan (safe).
