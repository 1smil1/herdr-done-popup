# herdr-done-popup

Standalone Herdr plugin that pops a desktop notification whenever an AI agent
in any Herdr pane finishes a task. Independent of `herdr_right_click.ahk`
and `herdr-agent-quota`.

## What you get

- A 300x82 rounded "pill" window at the right-bottom of your active monitor.
- Title: `agent · tab_label · pane_label` (so you always know which pane).
- Body: the first sentence of the agent's last output, truncated.
- Right-top × closes the popup. Click anywhere else to focus that Herdr pane.
- Multiple pane completions stack vertically.
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

## Install (one command)

```powershell
cd D:\herdr_done_popup
.\install.ps1
```

This builds the binary, then runs `herdr-done-popup.exe start` which:
- discovers every existing Herdr session via `herdr session list`
- runs `herdr plugin link` for each
- polls every 10s for new sessions and links them too

The install script keeps running (the watcher). Leave it running in a
terminal, or background it with `Start-Process` from another script.

## Run / Stop

```powershell
# Watcher (run once after install, or to recover from restart)
D:\herdr_done_popup\target\release\herdr-done-popup.exe start

# Inspect which sessions have us linked
D:\herdr_done_popup\target\release\herdr-done-popup.exe status

# Unlink from every session
D:\herdr_done_popup\target\release\herdr-done-popup.exe stop
```

To stop the watcher, kill the process:

```powershell
Get-Process herdr-done-popup | Stop-Process -Force
```

## Per-event model

Each Herdr `pane.agent_status_changed` event spawns
`herdr-done-popup.exe event` once. That process:
1. Decides suppress-or-show based on foreground window + tab focus.
2. Fetches the pane's first useful output line (via `herdr pane read
   --source visible`).
3. Counts existing popups to pick a vertical slot.
4. Creates the rounded Win32 window and runs its own message loop.
5. Exits when the user dismisses it.

No TCP, no shared state, no separate long-running receiver.

## Logs

`%TEMP%\herdr-done-popup.log` — one line per event with the decision inputs
and the final `SUPPRESS / AUTO10s / PERMANENT` outcome.

## Files

```
Cargo.toml          Rust deps
herdr-plugin.toml   herdr plugin manifest
src/main.rs         CLI dispatch + session/content helpers
src/gui.rs          Win32 popup, message loop, decision
install.ps1         build + start
uninstall.ps1       stop + unlink
```