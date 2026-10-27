# shell-manager

让 Windows 上的 shell 熬过 SSH 断线的持久终端。

[English](README.md)

[![ci](https://github.com/sparkjokerben/shell-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/sparkjokerben/shell-manager/actions/workflows/ci.yml)
[![release](https://github.com/sparkjokerben/shell-manager/actions/workflows/release.yml/badge.svg)](https://github.com/sparkjokerben/shell-manager/releases)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`sm` 是你敲命令的客户端，`smd` 是常驻后台的小守护进程。守护进程持有一个由
Windows 伪终端（ConPTY）托管的 shell：关掉 SSH 窗口、网络断开，或者主动脱附，
shell 都继续跑；重新连上后 `sm continue <名字>` 就回到同一个 shell，正在运行的
程序和工作目录都还在，当前屏幕会重绘。

被托管的 shell 使用你自己的注册表环境变量（额外设置 `TERM=xterm-256color`，
这与伪终端的实际能力一致），所以 oh-my-posh 之类的配置照常渲染。

## 为什么需要它

连到 Windows 的 SSH 会话随连接一起结束，里面启动的东西也一起没了。原生 Windows
控制台没有 tmux/screen；WSL 里的 tmux 也托管不了 PowerShell 或任何需要 Windows
桌面的程序。

## 环境要求

- Windows 10 1809+ 或 Windows 11（需要 ConPTY）
- 从源码构建需要 Rust 1.89+
- 从源码构建网页 UI 需要 Node.js 22.12+（`deploy.ps1` 会自动构建）

## 安装

从 [Releases](https://github.com/sparkjokerben/shell-manager/releases) 下载
`shell-manager-vX.Y.Z-x64.zip`，把 `sm.exe`、`smd.exe` 以及网页终端用的
`smw.exe` 放到 `PATH` 里的目录（例如 `%USERPROFILE%\bin`）。

守护进程必须运行在**交互桌面会话**里——ConPTY 的子进程继承守护进程的会话，而且
只有交互桌面才能承载 GUI 窗口：

```powershell
$vbs = "$env:USERPROFILE\bin\smd-launch.vbs"
schtasks /create /tn smd /tr ('wscript.exe "{0}"' -f $vbs) /sc onlogon /it /f
schtasks /run /tn smd
```

更简单的做法：把 `smd.exe` 的快捷方式放进 `shell:startup`。

从源码构建会构建前端、安装全部三个可执行文件，并为 `smd` 注册登录任务：

```powershell
git clone https://github.com/sparkjokerben/shell-manager
cd shell-manager
.\deploy.ps1
```

`deploy.ps1` 会立即替换 `sm.exe` 与 `smw.exe`，并重启 `smw`（只断开浏览器连接，
不影响会话）。只有当已安装的 `smd.exe` 没有在运行、或它没有任何会话时，脚本才会
替换守护进程；要重启正在运行的守护进程必须显式传 `-RestartDaemon`，它会结束所有
托管的 shell。

`-Force` 是无条件更新：它会先结束所有托管会话，连同 `smd.exe` 一起替换，然后重启
`smd`/`smw`。因为它会杀掉自己所在的 shell，必须从一条普通 SSH 命令、或不被 `sm`
托管的终端里执行：

```powershell
.\deploy.ps1 -Force
```

## 用法

```text
sm <名字>                  # 附着到该会话；不存在就新建
sm                         # 回到最近用过的会话
sm ls                      # 列出会话与附着状态
sm detach [名字]           # 断开当前客户端，shell 保持运行
sm stop [名字]             # 销毁会话及其 shell
sm start <名字> --wsl      # 只新建（已存在则报错），托管 wsl.exe
```

`a`/`attach`、`n`/`new`、`l`/`list`、`d`/`detach`、`k`/`kill` 都是缩写；`start`
必须给名字，其余省略名字表示“最近用过的会话”。

在附着的 shell 里按 `Ctrl+]` 或 `F12` 脱附，会话继续在后台运行；在另一个窗口里
执行 `sm detach <名字>` 效果相同，且不受附着客户端当前状态影响。

脱附时如果有程序正在运行（编译、测试、Claude Code 这类 TUI），它不会中断：守护
进程在没有客户端时仍持续排空伪终端输出，程序不会因为输出管道写满而卡死；重新
附着时客户端会让应用整屏重绘。

## 从 SSH 客户端粘贴图片

SSH 只传文本，而 Claude Code 读的是**运行它的那台机器**的剪贴板。`smpaste`
（`smpaste/` 下的客户端小工具）补上这一环：它监视本机剪贴板，把每张新图片
通过持久 SSH 连接推给发现到的主机。默认情况下它会扫描本机的进程列表，自动
发现你打开的交互式 `ssh` 会话，所以直接启动即可；传入 `--ssh-host <别名>`
则固定只推给这一台主机。

在 SSH 客户端上，从 `smpaste` 的 GitHub Actions 运行记录里下载对应平台的产物
（macOS arm64/x64、Linux x64/arm64、Windows x64），也可以自行构建：

```sh
cargo build --release -p smpaste
smpaste
```

若只想推给某一台主机，运行 `smpaste --ssh-host <你的 SSH 别名>`；自动发现的
扫描间隔可用 `--scan-ms` 调整（默认 5000ms）。自动发现仅在 macOS 和 Linux 上
可用；Windows 必须显式传 `--ssh-host`。

主机把最新图片暂存在 `%TEMP%\shell-manager-paste\latest.bmp`。

首选方式是在 Otty 里按 `Cmd+V`：Otty 会把复制的图片写到客户端的
`$TMPDIR/otty-paste/image-<数字>.png`，并把这个绝对路径当普通文本发出去。
当一次输入整块就是这样的路径时，`sm` 会丢弃该文本，等待约 400ms 让 watcher
把图片送到主机，再让守护进程把图片以 `CF_DIB` 写进 Windows 剪贴板，最后向
Claude Code 发送它原本的图片粘贴键，于是图片以 `[Image #N]` 附件出现，而不是
路径。若主机上还没有暂存图片，路径会照常作为文本转发。由于 `sm` 无法得知
前台是哪个程序，在普通 shell 提示符下粘贴的路径同样会被识别；只有在主机已有
暂存图片时才会吞掉这段文本。等待时间可用 `SM_PASTE_DELAY_MS` 调整（毫秒，
1-10000）。

手动兜底是在附着的 `sm` 会话里按 `Ctrl+'`（终端发送的是 `0x1c`；很多键盘上
`Ctrl+\` 也是同一个字节），它同样会让守护进程发布暂存图片并发送粘贴键。
守护进程和 Claude Code 必须在同一个 Windows 交互会话里；按下快捷键前，
客户端 watcher 需要已经把图片送到主机。

watcher 会转发**所有**剪贴板图片，不只是打算给主机的那些。只在你愿意发送
这些数据的客户端上运行。为了发现 SSH 会话，它还会读取本机的进程列表。自启
示例和 SSH 免交互认证见 `smpaste/README.md`。

## 网页终端（Web UI）

`smw` 是可选的第三个二进制，在同一个守护进程之上提供浏览器终端：

```text
浏览器 ── smw.exe（HTTP/WebSocket）── 命名管道协议 ── smd.exe ── ConPTY
```

它不改变 `smd` 或现有协议，使用的命名管道与 `sm` 相同。前端是
Vite + React + TypeScript + xterm.js 应用，静态资源在编译时整体内嵌进
`smw.exe`。

前端构建产物不提交仓库，因此编译 `smw` 前要先构建前端。不构建时
`cargo build` 仍能成功，但 `smw` 只会提供一张提示“前端未构建”的占位页：

```powershell
cd web
npm ci
npm run build
cd ..
cargo build --release --bin smw
```

前端有任何改动都要重新编译 `smw`，因为整个 `web/dist` 目录是在编译时内嵌的。

以与 `smd` 相同的 Windows 用户启动桥接进程（`smw` 本身不要求交互桌面），然后
打开 <http://127.0.0.1:27183>：

```text
> smw
smw 0.1.1 instance=prod listening on http://127.0.0.1:27183 (token off)
```

选项：

```text
smw [--bind <地址>] [--port <端口>] [--token <令牌>] [--instance <名字>]
    [--allow-insecure] [-h] [-V]
  --bind <地址>      监听地址（默认：127.0.0.1）
  --port <端口>      监听端口（默认：27183）
  --token <令牌>     API 令牌（默认取 $SMW_TOKEN；建议用环境变量）
  --instance <名字>  实例命名空间（覆盖 $SM_INSTANCE）
  --allow-insecure   允许非回环绑定且不设令牌（危险）
  -h, --help         显示帮助
  -V, --version      显示版本
```

### 登录自启与固定令牌

生产环境通常让 `smw` 随登录自动启动。`deploy.ps1` 会：

- 把隐藏控制台窗口的 `smw-launch.vbs` 复制到 `%USERPROFILE%\bin`；
- 注册登录任务 `smw`（`schtasks /create /tn smw /sc onlogon /it`）；
- 停止旧的 `smw` 并立即启动新的，让新内嵌界面马上生效。

令牌固定在**用户级环境变量** `SMW_TOKEN` 里。它持久化在 `HKCU\Environment`，
重启、注销、重新登录后仍然存在，而且不会出现在任务定义或进程命令行中：

```powershell
[Environment]::SetEnvironmentVariable('SMW_TOKEN', '<令牌>', 'User')
```

设置后只有新启动的进程能读到。查看、更换或删除：

```powershell
[Environment]::GetEnvironmentVariable('SMW_TOKEN','User')
[Environment]::SetEnvironmentVariable('SMW_TOKEN','新令牌','User')
[Environment]::SetEnvironmentVariable('SMW_TOKEN',$null,'User')
```

不要用空令牌监听非回环地址：`smw` 会拒绝这种组合，除非显式加
`--allow-insecure`。删除登录任务即可关闭自启：

```powershell
schtasks /delete /tn smw /f
```

实例规则与 `sm`/`smd` 相同：`SM_INSTANCE` 选择命名空间，位于本项目 `target/`
目录下的可执行文件自动使用 `dev`。`smw` 连接的是同一命名空间里的守护进程。

浏览器看到的就是 `sm` 使用的那些会话。关闭标签页或点击 Detach 等同于脱附，
shell 继续运行；“强制接管”（Force takeover）会先断开当前附着的客户端，再让
浏览器接入。

JSON API 与客户端命令一一对应：

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/health` | 存活状态、实例、版本、是否设置了令牌 |
| GET | `/api/sessions` | 会话列表及其附着状态 |
| POST | `/api/sessions` | 新建会话（`{"name": "...", "wsl": false}`） |
| POST | `/api/sessions/{name}/detach` | 让当前附着的客户端脱附 |
| DELETE | `/api/sessions/{name}` | 销毁会话及其 shell |
| GET | `/ws/{name}` | 该会话的 WebSocket 终端 |

### 安全

默认没有令牌时所有请求都会被接受。`smw` 默认只绑定 `127.0.0.1`，因此不在网络上
暴露——但**回环地址不是用户边界**：多用户机器上的其他本地账户仍能访问该端口，
网页也可以向它发起 WebSocket。共享机器上请设置令牌（`--token`，更推荐
`SMW_TOKEN` 环境变量）。

设置令牌后，HTTP 请求需要 `Authorization: Bearer <令牌>` 或
`X-SMW-Token: <令牌>`，WebSocket 升级还可以使用 `?token=`。静态页与
`/api/health` 不需要令牌，以便浏览器先加载页面并得知是否需要认证。浏览器
WebSocket 无法自定义请求头，所以令牌会出现在 URL 查询串里；请把这个 URL 当作
秘密保管，并避免让它进入访问日志。

`smw` 会校验每个 API/WebSocket 请求的 `Origin`/`Host`：浏览器来源必须与 `Host`
一致；无令牌的回环监听还会拒绝非回环主机名（防 DNS rebinding）。不带 `Origin`
的本地脚本允许通过。非回环绑定且没有令牌时，除非显式传 `--allow-insecure`，
否则拒绝启动。

不要把没有令牌的 `smw` 暴露到本机之外。转发端口（包括经 frp）属于使用者的责任。

### frp

`smw` 可以搭配 [frp](https://github.com/fatedier/frp) 使用。在本机运行 `frpc`，
把本地端口转发到你的 `frps`：

```toml
# frpc.toml
serverAddr = "frps.example.com"
serverPort = 7000

[[proxies]]
name = "smw"
type = "tcp"
localIP = "127.0.0.1"
localPort = 27183
remotePort = 18787
```

公网部署必须设置令牌（`--token`，更推荐 `SMW_TOKEN`）。注意 `transport.tls`
只加密 frpc 与 frps 之间的隧道；`type = "tcp"` 时，浏览器到 `frps` 公网端口
这一段仍是明文 HTTP/WebSocket，URL 中的令牌和全部终端流量都可能被读取。请在
`frps` 前面终止 TLS（例如 `vhostHTTPSPort` 配合 `https2http` 插件，或
nginx/Caddy 反向代理）并使用 `https://`/`wss://`，或把隧道放进 VPN。代理访问
日志需要脱敏查询串。不要转发没有令牌的 `smw` 端口。

### 限制

- **同一会话同时只允许一个客户端。** 原生 `sm` 附着与网页终端互斥：一方附着
  时，另一方会看到会话忙，直到对方脱附或被强制接管。
- **重连只恢复当前屏。** ConPTY 重绘的是应用主动重画的内容，脱附期间产生的
  输出不会作为历史回滚缓冲保留。
- **必须使用同一个 Windows 用户。** `smw` 与 `smd` 要在同一账户下运行；命名
  管道使用默认 DACL，其他用户无法打开。
- **`smd` 仍须运行在交互桌面会话中**，与使用 `sm` 时完全一致。

## 开发实例隔离

从本仓库直接构建的二进制与已安装的生产守护进程完全隔离，可以一边开发
`sm`/`smd`，一边继续使用日常的生产会话：

- 位于本项目 `target/` 目录下的可执行文件自动使用 `dev` 实例。
- 其他位置的可执行文件使用 `prod`，保持原有的命名不变。
- 设置 `SM_INSTANCE=<名字>` 可选择其他命名空间。取值会先 trim 并转小写，必须是
  1-32 个 `A-Z a-z 0-9 - _` 字符。非法值会被忽略（打印一次警告）并继续按上面的
  规则判断，因此写错也不会让开发版误连生产。

每个实例都有独立的控制管道、数据管道、单实例互斥量和日志
（`%TEMP%\smd-<实例>.log`、`%TEMP%\sm-client-<实例>.log`）。生产实例继续使用
`\\.\pipe\shell-manager\...`、`Global\smd-single-instance` 和 `%TEMP%\smd.log`，
升级不会打扰正在运行的守护进程。

开发守护进程和客户端可以和已安装的生产版同时运行：

```powershell
cargo run --bin smd      # 提供 "dev" 命名空间
cargo run --bin sm -- ls # 连接这个开发守护进程
```

`sm --version` 会打印解析出的实例名。

## 工作原理

```text
终端 ── sm.exe ──┬── 控制管道 ──┐
                 └── 数据管道 ──┤
                                smd.exe ── ConPTY ── pwsh.exe / wsl.exe
```

- `smd` 维护会话表，每个会话持有一个 ConPTY、shell 进程，以及两条单向命名管道
  （`out`、`in`）供附着的客户端使用。
- 被托管的 shell 会看到 `TERM=xterm-256color`。ConPTY 本身就是 VT 终端，而依赖
  该变量的程序（例如 Claude Code）据此决定渲染 `○`/`□`/`☒` 还是 ASCII 回退符号
  （`()`/`[]`/`[x]`）。
- 没有客户端附着时，排空线程持续读取伪终端输出，保证程序不会因管道写满而阻塞。
- 重新附着时客户端把伪终端高度先减一、再恢复，触发应用整屏重绘。
- shell 选择顺序：非打包版 PowerShell 7（Program Files 或用户级安装）→ `PATH`
  里的 `pwsh.exe` → Microsoft Store 别名 → Windows PowerShell 5.1，逐个尝试，
  取第一个真正能启动的。

## 已知限制

- **守护进程运行在你的交互会话里。** 注销或重启会结束 `smd` 和它托管的所有会话；
  SSH 断线不受影响。
- **GUI 相关的工作依赖那个桌面会话。** Electron 应用、屏幕自动化等能正常工作，
  正是因为宿主 shell 在交互会话中。
- **Microsoft Store 版 PowerShell 7 无法从非交互会话启动**（它的 `WindowsApps`
  别名是 MSIX 应用）。会话 0 下客户端会退到下一个候选；交互会话下正常。
- **屏蔽了 Win32 输入模式。** PowerShell 7 会向终端请求该模式（`DECSET 9001`），
  支持的终端随后不再发送普通按键字节，detach 键识别与鼠标过滤都会失配。客户端
  因此隐藏该请求并主动关闭，代价是 PSReadLine 的扩展组合（`Shift+Enter`、
  `Ctrl+Enter`）在会话里退化为普通 `Enter`。
- **纯悬停的鼠标移动会被丢弃**，避免它们被打进普通提示符；点击、滚轮、拖拽仍
  原样转发给 TUI 应用。

## 排查

- `%TEMP%\smd.log`（非生产实例为 `%TEMP%\smd-<实例>.log`）——守护进程启动、
  选中的 shell、每次建会话的结果
- `%TEMP%\sm-client.log`（非生产实例为 `%TEMP%\sm-client-<实例>.log`）——
  客户端构建号、控制台模式、stdin 原始字节与脱附判定
- `%TEMP%\smw.log`（非生产实例为 `%TEMP%\smw-<实例>.log`）——网页桥接启动、
  会话操作、附着/脱附与错误（不记录令牌或按键内容）

## 许可证

MIT
