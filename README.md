# herdr-done-popup

A desktop popup for Herdr that tells you the moment an AI agent in any pane
finishes — or asks you a question.

It is independent of `herdr_right_click.ahk` and `herdr-agent-quota`. It has
no background process: every command exits as soon as it finishes.

## What you see

A 300×82 rounded "pill" appears at the right-bottom of your active monitor:

```
claude · herdr · 测试tab1            ×
等你的结果。
```

- **Title** — `agent · workspace_label · tab_label · pane_label` so you always
  know which pane finished.
- **Body** — the first useful sentence of the agent's last output (truncated).
- **Right-top ×** — dismiss.
- **Click anywhere else** — focus that Herdr pane (does not change the window's
  minimized / maximized / normal state).

If the agent is **asking you something** (blocked state) the pill turns brick
red and the title gets a `· 等待输入` suffix so it reads as "needs your answer"
at a glance.

If multiple agents complete at the same time, the pills stack vertically at
the right edge.

## When does the popup show / stay / go away?

The popup event fires on every `pane.agent_status_changed` for your Herdr
sessions. We **suppress** it only if all of these are true:

- The foreground window is a Herdr window
- It belongs to the same Herdr session as the completion
- The user's focused tab in that session is the same tab as the completion

Anything else (different Herdr / different session / different workspace /
different tab) → the popup shows.

If the popup is up and you start typing anywhere, it auto-dismisses in 10s.
If you switch focus into the originating tab, it dismisses immediately. If you
just leave it alone, it stays until you click × or focus the originating tab.

## Install

### One command, no daemon

```powershell
git clone https://github.com/<owner>/herdr-done-popup
cd herdr-done-popup
.\install.ps1
```

That's it. `./install.ps1` builds the release binary, then links it into every
currently-running Herdr session and enables it. No background process to
manage, no Windows service to install.

You can also do it from a fresh Herdr session:

```powershell
herdr plugin install <owner>/<repo>
```

`herdr` itself clones the repo, runs `cargo build --release` (via the
`[[build]]` entry in `herdr-plugin.toml`), and enables the plugin. Any
public GitHub repo works — no official marketplace needed.

## New Herdr sessions

When you create a new Herdr session later, run this from inside it:

```powershell
herdr plugin install <owner>/<repo>
```

That one command:

1. Clones (or just links) the plugin in the new session.
2. Builds and links it.
3. Triggers our `[[startup]]` action, which calls `herdr-done-popup start` to
   ensure the plugin is **enabled in every Herdr session**, not just the new
   one. So the new session is automatically wired up everywhere.

## Daily use

```powershell
herdr-done-popup stop       # pause: disable everywhere (plugin stays linked)
herdr-done-popup start      # resume: enable everywhere
```

Both are idempotent and instant. `start` is also auto-fired by `[[startup]]`,
so you rarely need to call it yourself.

## Uninstall

```powershell
.\uninstall.ps1
# then delete the cloned directory
Remove-Item -Recurse .\herdr-done-popup
```

Unlinks from every Herdr session, removes the build output, and tells you
how to delete the directory.

## Inspect

```powershell
# Which plugins are linked in the current session?
herdr plugin list

# How many sessions exist?
herdr session list
```

## Logging

`%TEMP%\herdr-done-popup.log` — one line per event with the decision inputs
and the final outcome (`SUPPRESS / AUTO10s / PERMANENT`). Open it to find out
why a popup did or didn't show.

## Plugin manifest (herdr-plugin.toml)

| Hook | Command | When |
|---|---|---|
| `[[build]]` | `cargo build --release` | `herdr plugin install` |
| `[[startup]]` | `herdr-done-popup start` | Plugin enabled in any session |
| `[[events]]` | `herdr-done-popup event` | `pane.agent_status_changed` fires |

## Files in this repo

```
Cargo.toml          Rust deps
herdr-plugin.toml   Herdr plugin manifest
src/main.rs         CLI dispatch + helpers (session, snippet, JSON, ANSI)
src/gui.rs          Win32 popup window, message loop, decision logic
install.ps1         Windows convenience: build + link + enable everywhere
uninstall.ps1       Windows convenience: unlink + clean
README.md           this file
```

## Supported platforms

- Windows (primary, tested)
- macOS / Linux (declared in `platforms = ["macos", "linux", "windows"]`;
  needs someone to verify the Win32 `gui.rs` ports to GTK or similar)
