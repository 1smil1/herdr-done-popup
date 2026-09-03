# Herdr Done Popup

独立于 `herdr_right_click.ahk` 的 Windows Herdr 完成提醒。

## 功能

- 监听 `pane.agent_status_changed`。
- 只在同一 `(session, pane, agent)` 观察到 `working -> idle/done` 时提醒。
- 提示窗口置顶、不会自动消失，出现在鼠标所在屏幕右下角。
- **打开**：恢复对应 Herdr 窗口到前台，并定位到完成 agent 的 pane。
- **忽略**：只关闭提示。
- 支持当前 `default` 和 `dse` 两个独立 Herdr server。

## 安装

在 PowerShell 中执行：

```powershell
cd 'D:\herdr done popup'
.\install.ps1
```

插件按 Herdr server 分别注册；安装脚本不会修改 `herdr_right_click.ahk` 或 `herdr-agent-quota`。

## 手动启动接收器

```powershell
Start-Process '.\target\release\herdr-done-popup.exe' -ArgumentList receiver -WindowStyle Hidden
```

## 卸载

```powershell
.\uninstall.ps1
```

## 事件测试（不需要真实 AI 请求）

```powershell
$env:HERDR_SOCKET_PATH = "$env:APPDATA\herdr\herdr.sock"
$env:HERDR_PLUGIN_EVENT_JSON = '{"event":"pane_agent_status_changed","data":{"pane_id":"w1:p99","agent":"claude","status":"working"}}'
.\target\release\herdr-done-popup.exe event
$env:HERDR_PLUGIN_EVENT_JSON = '{"event":"pane_agent_status_changed","data":{"pane_id":"w1:p99","agent":"claude","status":"idle"}}'
.\target\release\herdr-done-popup.exe event
```

第二条事件应产生持久弹窗。测试后可点击“忽略”。
