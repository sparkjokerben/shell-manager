# shell-manager

让 Windows 上的 shell 熬过 SSH 断线的持久终端。

[English](README.md)

[![ci](https://github.com/sparkjokerben/shell-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/sparkjokerben/shell-manager/actions/workflows/ci.yml)
[![release](https://github.com/sparkjokerben/shell-manager/actions/workflows/release.yml/badge.svg)](https://github.com/sparkjokerben/shell-manager/releases)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`sm` 是你敲命令的客户端，`smd` 是常驻后台的小守护进程。守护进程持有一个由
Windows 伪终端（ConPTY）托管的 shell：关掉 SSH 窗口、网络断开，或者主动脱附，
shell 都继续跑；重新连上后 `sm continue <名字>` 就回到同一个 shell，回滚缓冲、
正在运行的程序、工作目录全都还在。

被托管的 shell 完全没有被改动：`pwsh.exe` / `wsl.exe` 照常启动，并使用你自己的
注册表环境变量，所以 oh-my-posh 之类的配置照常渲染。

## 为什么需要它

连到 Windows 的 SSH 会话随连接一起结束，里面启动的东西也一起没了。原生 Windows
控制台没有 tmux/screen；WSL 里的 tmux 也托管不了 PowerShell 或任何需要 Windows
桌面的程序。

## 环境要求

- Windows 10 1809+ 或 Windows 11（需要 ConPTY）
- 从源码构建需要 Rust 1.74+

## 安装

从 [Releases](https://github.com/sparkjokerben/shell-manager/releases) 下载
`shell-manager-vX.Y.Z-x64.zip`，把 `sm.exe` 与 `smd.exe` 放到 `PATH` 里的目录
（例如 `%USERPROFILE%\bin`）。

守护进程必须运行在**交互桌面会话**里——ConPTY 的子进程继承守护进程的会话，而且
只有交互桌面才能承载 GUI 窗口：

```powershell
schtasks /create /tn smd /tr "wscript.exe \"$env:USERPROFILE\bin\smd-launch.vbs\"" /sc onlogon /it /f
schtasks /run /tn smd
```

更简单的做法：把 `smd.exe` 的快捷方式放进 `shell:startup`。

从源码构建会同时安装两个可执行文件并注册登录任务：

```powershell
git clone https://github.com/sparkjokerben/shell-manager
cd shell-manager
.\deploy.ps1
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

## 工作原理

```text
终端 ── sm.exe ──┬── 控制管道 ──┐
                 └── 数据管道 ──┤
                                smd.exe ── ConPTY ── pwsh.exe / wsl.exe
```

- `smd` 维护会话表，每个会话持有一个 ConPTY、shell 进程，以及两条单向命名管道
  （`out`、`in`）供附着的客户端使用。
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

- `%TEMP%\smd.log`——守护进程启动、选中的 shell、每次建会话的结果
- `%TEMP%\sm-client.log`——客户端构建号、控制台模式、stdin 原始字节与脱附判定

## 许可证

MIT
