# herdr-done-popup

Standalone Herdr plugin that pops a desktop notification whenever an AI agent
in any Herdr pane finishes a task or asks a question. Independent of
`herdr_right_click.ahk` and `herdr-agent-quota`.

## What you get

- A 300x82 rounded "pill" window at the right-bottom of your active monitor.
- Title: `agent · tab_label · pane_label` (so you always know which pane).
- Body: the first sentence of the agent's last output, truncated.
- Right-top × closes the popup. Click anywhere else to focus that Herdr pane.
- Multiple pane completions stack vertically.
- **Blocked** state (agent asking a question) is shown in brick red with
  `· 等待输入` suffix so it reads as "needs your answer".
- Auto-dismiss after 10s if you've been typing in the last 5s; otherwise it
  stays until you click × or switch into the originating tab.

## Suppression: only when you can already see the completion

When the popup event fires, we suppress only if all of these are true:
- The foreground window is a Herdr window
- It belongs to the same Herdr session as the completion
- The user's focused tab in that session is the same tab as the completion

Anything else (different Herdr, different session, different workspace,
different tab) → popup shows.

If the popup stays and you start typing in any window, it upgrades to
"10s auto-dismiss". If you switch focus into the originating tab, it
auto-dismisses immediately.

## Install / start / stop / uninstall (no daemon)

```powershell
cd D:\herdr_done_popup
.\install.ps1      # one-time: build + link in every Herdr session
herdr-done-popup start    # turn on the plugin (enable everywhere)
herdr-done-popup stop     # turn off (disable; still linked)
herdr-done-popup uninstall  # unlink everywhere + clean
```

`install` is permanent: it builds the binary and registers it with Herdr.
`start` / `stop` are the daily-use toggle. `uninstall` removes everything.

There is **no background process**. Each command exits as soon as it's done.

## New Herdr sessions

When you create a new Herdr session, run from inside it:

```powershell
herdr plugin install D:\herdr_done_popup
```

This triggers our `[[startup]]` action, which calls `herdr-done-popup start`
to enable the plugin in every session (including the new one). One command
per new session.

## GitHub install (after publishing)

```powershell
herdr plugin install <owner>/<repo>
```

Herdr clones the repo, runs `cargo build --release` (via the `[[build]]` entry
in `herdr-plugin.toml`), and enables the plugin. No official marketplace
required — any public GitHub repo works.

## Per-event model

Each Herdr `pane.agent_status_changed` event spawns
`herdr-done-popup.exe event` once. That process:
1. Decides SUPPRESS / AUTO10s / PERMANENT based on foreground window + tab focus.
2. Fetches the pane's first useful output line (via `herdr pane read
   --source visible`).
3. Counts existing popups via EnumWindows to pick a vertical slot.
4. Creates the rounded Win32 window and runs its own message loop.
5. Exits when the user dismisses it.

## Logs

`%TEMP%\herdr-done-popup.log` — one line per event with the decision inputs
and the final `SUPPRESS / AUTO10s / PERMANENT` outcome.

## Files

```
Cargo.toml          Rust deps
herdr-plugin.toml   herdr plugin manifest (build, startup, events)
src/main.rs         CLI dispatch + session/content helpers
src/gui.rs          Win32 popup, message loop, decision
install.ps1         one-time setup: build + link
uninstall.ps1       unlink + clean
```