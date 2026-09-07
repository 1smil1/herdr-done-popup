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

## Install (no daemon)

```powershell
cd D:\herdr_done_popup
.\install.ps1
```

That's it. `./install.ps1` runs `cargo build --release` and then
`herdr-done-popup start` which links the plugin to every currently running
Herdr session. There's no background process to manage.

If you create a new Herdr session later, run from inside it:

```powershell
herdr plugin install D:\herdr_done_popup
```

This triggers our `[[startup]]` action, which calls `herdr-done-popup start`
to link to every session (including the new one). One command per new session.

For a clean GitHub install (no `install.ps1` needed once it's published):

```powershell
herdr plugin install <owner>/<repo>
```

`herdr` itself will clone the repo, run `cargo build --release` (via the
`[[build]]` entry in `herdr-plugin.toml`), and enable the plugin. No
official marketplace required — any public GitHub repo works.

## CLI

```
herdr-done-popup install    one-time: cargo build --release (local dev)
herdr-done-popup start      link to every current session
herdr-done-popup stop       unlink from every session
herdr-done-popup uninstall  stop + cargo clean + dir hint
herdr-done-popup event      per-agent-event popup (herdr [[events]] only)
```

`event` and `start` are also fired by herdr itself — you don't normally
call them by hand.

## Uninstall

```powershell
.\uninstall.ps1
```

Unlinks from every Herdr session and removes `target/`. To fully delete the
plugin, `Remove-Item -Recurse D:\herdr_done_popup`.

## Logs

`%TEMP%\herdr-done-popup.log` — one line per event with the decision inputs
and the final `SUPPRESS / AUTO10s / PERMANENT` outcome.

## Files

```
Cargo.toml          Rust deps
herdr-plugin.toml   herdr plugin manifest (build, startup, events)
src/main.rs         CLI dispatch + session/content helpers
src/gui.rs          Win32 popup, message loop, decision
install.ps1         build + start
uninstall.ps1       stop + clean
```