# hyperMAC

[English](#english) · [中文](#中文)

## English

hyperMAC is a free RDP server for your Mac's own desktop: connect from another computer and you
see and control the desktop that is logged in on the Mac, with its sound and clipboard. It also
runs virtual machines on the same Mac (macOS, Windows 11 ARM, Linux, FreeBSD). Free and open
source: MIT, the bhyve port in `vm/bhyve` BSD-2-Clause.

New in 1.1, as a preview: **virtual machines** on the same Mac (see [Virtual machines](#virtual-machines-preview)).

**Requirements**: a Mac with Apple Silicon. Tested on macOS 26 and 27.2 (it asks for macOS 13 or
later; 13–15 not tested). Intel Macs are not supported.

### Features by macOS version

✅ tested on a real Mac · ☑️ should work, not yet tested on this version · — not tested

| Feature | 13–15 | 26 | 27.2 |
|---|:-:|:-:|:-:|
| kRDPclient: login, H.264 hardware video | — | ✅ | ✅ |
| mstsc / standard RDP clients (RemoteFX) | — | ✅ | ☑️ |
| Keyboard and mouse (input methods work) | — | ✅ | ✅ |
| Sound (Opus to kRDPclient, PCM to mstsc) | — | ✅ | ☑️ |
| Clipboard: text | — | ✅ | ☑️ |
| Clipboard: pictures | — | ✅ | ✅ |
| Found by kRDPclient's "Discover keelOS" | — | ✅ | ☑️ |
| Runs in the background, starts at login | — | ✅ | ✅ |
| Permissions kept across a macOS upgrade | — | — | ✅ (26 → 27.2) |
| App, first-run setup, Check for Updates | — | ✅ | ☑️ |

### Compared with other remote desktops for the Mac

From each product's public description (2026-11); not all of them tested side by side.

| | hyperMAC | Apple Screen Sharing | NoMachine | Chrome Remote Desktop | RustDesk | Parsec |
|---|---|---|---|---|---|---|
| Protocol | RDP | VNC + Apple extensions | NX | WebRTC | own | own |
| Video codec | H.264 hardware (kRDPclient); RemoteFX / bitmaps (mstsc); sound Opus / PCM | VNC encodings (Raw/ZRLE/Tight…); HEVC in high-performance mode (Mac to Mac, Apple Silicon) | H.264, VP8 | VP8 / VP9 (WebRTC) | VP8 / VP9 / AV1, optional hardware H.264 / H.265 | H.264, H.265 |
| Windows' built-in mstsc connects | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Account / cloud service needed | no | no | no (LAN) | Google account | no (self-hosted relay possible) | yes |
| Hardware video to a Windows client | ✅ H.264 (kRDPclient) | ❌ | ✅ | — | ✅ | ✅ |
| Smoothness (Windows client) | smooth: 1080p 30 fps (60 on a LAN), measured 7.6 ms decode, 6–10 Mbps, ~6 % Mac CPU | choppy (tested with TigerVNC) | smooth | fair | fairly smooth | very smooth (game-grade latency) |
| Sound | ✅ | high-performance mode only (Mac clients) | ✅ | ✅ | ✅ | ✅ |
| File transfer | ❌ (planned) | ✅ (Mac clients) | ✅ | ❌ | ✅ | ❌ |
| Works at the login window | ❌ | ✅ | ✅ | ✅ | ✅ | — |
| Price | free | built in | free for personal use | free | open source | free for personal use |

Pick hyperMAC when the other side is Windows and you want plain RDP (mstsc with nothing to install,
or kRDPclient for hardware H.264), with no cloud account in between.

### Install

1. Download the newest `.dmg` from [Releases](https://github.com/keelos-dev/hyperMAC/releases)
   and open it.
2. Drag **hyperMAC** into **Applications**.
3. Open hyperMAC from Applications and follow the dialogs:
   - type your Mac's login password once: clients log in with your Mac account (user name and
     login password), there is no separate password to remember;
   - allow **Screen Recording** and **Accessibility** in System Settings → Privacy & Security.
     Accessibility is not switched on for you: turn on hyperMAC's switch in that list yourself.
     macOS 26 asks once more, on the first connection, whether hyperMAC may capture the screen and
     audio: allow it;
   - optionally turn on Remote Login (SSH), so kRDPclient can open a terminal on this Mac.
4. hyperMAC now runs in the background and starts at every login. Open the app again at any time
   for its status, a kRDPclient token, password change or uninstall.

### Virtual machines (preview)

The window's **Virtual Machines** tab makes and runs machines on this Mac; kRDPclient lists them
after the Mac itself and connects to their screens the same way.

| Guest | Runs on | State |
|---|---|---|
| macOS | Apple's Virtualization.framework | installs by itself from a restore image (.ipsw) |
| Windows 11 (ARM) | bhyve (from FreeBSD) on Hypervisor.framework | installs from Microsoft's own ISO, by hand in the console window or unattended; network, 1920x1080; Windows Update worked |
| Linux, FreeBSD (arm64) | Virtualization.framework / bhyve | installs from the distribution's ISO |

What you need to bring: the system's installer (ISO / IPSW); for Windows also the
[virtio-win driver ISO](https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/) (network driver).
A Windows machine has a TPM 2.0 and Secure Boot, so Microsoft's installer runs as it is, with its
hardware checks on.

Known gaps: no graphics acceleration in Windows (basic display driver); no sound from
virtual machines; bridged networking, sleep/wake and long runs are not tested yet. This part is
new: expect rough edges and keep copies of anything important.

### Which client

**kRDPclient is the recommended client**: https://github.com/keelos-dev/kRDPclient-releases
(Windows). In kRDPclient choose "Add keelOS host", enter the Mac's address and your Mac's
login password (kRDPclient trades it for a token and does not keep the password), or a token made
in the hyperMAC app. It gets the picture as hardware-encoded H.264, plus sound and the clipboard.

mstsc (Remote Desktop Connection) and other standard RDP clients can connect too
(`mac-address:3389`, your Mac user name and login password), **but they have no H.264 channel**:
the picture goes as RemoteFX / bitmaps and is noticeably less smooth. If mstsc feels slow, try
kRDPclient before judging hyperMAC.

### Known limitations

- A user must be logged in on the Mac: at the login window (after a restart without automatic
  login) nobody can connect. Turn on automatic login in System Settings → Users & Groups.
- The Mac needs a display, or a dummy plug (HDMI headless adapter); with nothing attached macOS
  has no screen to capture.
- Clipboard: text and pictures (screenshots, copied images), no files yet.
- The mouse pointer is part of the picture; there is no client-side pointer yet.
- The login is your Mac account, so anyone who knows your Mac password can connect: use a good
  one, and keep port 3389/443 inside your network. hyperMAC does not store the password; for mstsc
  it keeps the password's NT hash in a file only your account can read. After changing the Mac
  password, the old one stops working at once; mstsc needs one login with the new one from
  kRDPclient (or open hyperMAC and type it), kRDPclient works right away.
- Updates are not checked by themselves: use "Check for Updates…" in the app.

### Update

Open hyperMAC and click "Check for Updates…" (under About). If there is a newer version it shows
what changed; "Update" downloads it, checks that it is signed by hyperMAC's developer and notarized
by Apple, replaces the app and restarts the service (a session in progress drops for a few
seconds). The permissions carry over.

By hand: quit hyperMAC's window first (⌘Q; Finder does not replace an app whose window is open:
"hyperMAC is in use"), drag the new hyperMAC over the old one, then open it. The background service
may keep running.

### Uninstall

1. Open hyperMAC and choose "Uninstall the login item".
2. Delete hyperMAC from Applications and the folder `~/Library/Application Support/hyperMAC`.
3. Optionally remove hyperMAC from Screen Recording and Accessibility in System Settings.

Remote Login (SSH) is a macOS setting and is left as it is; turn it off in System Settings →
General → Sharing if you no longer need it.

### Building from source

Needs Xcode's command line tools, Rust (rustup, stable) and Python 3; for the TPM of Windows
machines also `brew install libtpms openssl` (without them bhyve builds without a TPM).

```sh
sh build.sh        # target/release/hypermac, vm/target/release/hypermac (the VM program), vm/bhyve/build/bhyve
```

`build.sh` signs with the first "Apple Development" identity in the keychain when there is one
(the two permissions then survive a rebuild), else ad hoc. `target/release/hypermac install` sets
it up to run at login. How the parts fit together, and the release packaging, are in
[docs/DEVELOPING.md](docs/DEVELOPING.md) (in Chinese); the virtual machines in
[vm/README.md](vm/README.md) and [vm/bhyve/README.md](vm/bhyve/README.md).

This repository receives a snapshot per release from the development repository, one commit
each. Pull requests are welcome: a merged one is carried into the development repository and
is part of the next snapshot.

### License

hyperMAC's own code: MIT ([LICENSE](LICENSE)). `vm/bhyve`, the port of FreeBSD's bhyve to
Hypervisor.framework: BSD-2-Clause ([vm/bhyve/LICENSE](vm/bhyve/LICENSE)); FreeBSD's files in
`vm/bhyve/freebsd` keep their own headers.

### Third-party software

hyperMAC is built on [IronRDP](https://github.com/Devolutions/IronRDP) (MIT / Apache-2.0) and
other open-source Rust crates; its virtual machines use [bhyve](https://bhyve.org) from FreeBSD
(BSD licenses) and the [EDK2](https://www.tianocore.org) UEFI firmware (BSD-2-Clause-Patent).
Their notices ship with every copy: `THIRD_PARTY.md`, `LICENSE-IronRDP-MIT`,
`THIRD_PARTY-crates.txt` and `THIRD_PARTY-vm.txt` are at the top of the dmg and in
`hyperMAC.app/Contents/Resources/`.

## 中文

hyperMAC 是一个免费的 Mac 用 RDP 服务端：从别的电脑连上来，看到、操作的就是这台 Mac 上当前登录
的那个桌面，带声音和剪贴板；还能在同一台 Mac 上跑虚拟机（macOS、Windows 11 ARM、Linux、FreeBSD）。
免费、开源：MIT，`vm/bhyve` 里的 bhyve 移植是 BSD-2-Clause。

1.1 新增（预览）：在同一台 Mac 上跑**虚拟机**，见[虚拟机](#虚拟机预览)。

**系统要求**：Apple Silicon 的 Mac。在 macOS 26 和 27.2 上测过（最低要求 macOS 13，13–15 没测）。不支持 Intel Mac。

### 功能 × macOS 版本

✅ 真机实测 · ☑️ 应该能用，这个版本还没测 · — 没测

| 功能 | 13–15 | 26 | 27.2 |
|---|:-:|:-:|:-:|
| kRDPclient：登录、H.264 硬件画面 | — | ✅ | ✅ |
| mstsc 等标准 RDP 客户端（RemoteFX） | — | ✅ | ☑️ |
| 键鼠（输入法可用） | — | ✅ | ✅ |
| 声音（kRDPclient 用 Opus，mstsc 用 PCM） | — | ✅ | ☑️ |
| 剪贴板：文字 | — | ✅ | ☑️ |
| 剪贴板：图片 | — | ✅ | ✅ |
| kRDPclient「Discover keelOS」自动发现 | — | ✅ | ☑️ |
| 后台运行、登录自启 | — | ✅ | ✅ |
| macOS 升级后授权不丢 | — | — | ✅（26 → 27.2） |
| app、首次设置、检查更新 | — | ✅ | ☑️ |

### 和其他 Mac 远程桌面比较

按各家公开说明整理（2026-11），没有全部并排实测。

| | hyperMAC | Apple 屏幕共享 | NoMachine | Chrome 远程桌面 | RustDesk | Parsec |
|---|---|---|---|---|---|---|
| 协议 | RDP | VNC + Apple 扩展 | NX | WebRTC | 自有 | 自有 |
| 视频编码 | H.264 硬编（kRDPclient）；RemoteFX / 位图（mstsc）；声音 Opus / PCM | VNC 编码（Raw/ZRLE/Tight…）；高性能模式 HEVC（两端都是 Apple Silicon Mac） | H.264、VP8 | VP8 / VP9（WebRTC） | VP8 / VP9 / AV1，可选硬件 H.264 / H.265 | H.264、H.265 |
| Windows 自带 mstsc 能直连 | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| 要账号/云服务 | 不要 | 不要 | 不要（局域网） | 要 Google 账号 | 不要（可自建中继） | 要 |
| 给 Windows 客户端的硬件视频 | ✅ H.264（kRDPclient） | ❌ | ✅ | — | ✅ | ✅ |
| 流畅度（Windows 客户端） | 流畅：1080p 30 fps（局域网可 60），实测解码 7.6 ms、6–10 Mbps、Mac CPU 约 6% | 卡（TigerVNC 实测） | 流畅 | 一般 | 较流畅 | 很流畅（游戏级延迟） |
| 声音 | ✅ | 只有高性能模式（Mac 客户端） | ✅ | ✅ | ✅ | ✅ |
| 文件传输 | ❌（计划中） | ✅（Mac 客户端） | ✅ | ❌ | ✅ | ❌ |
| 停在登录窗口时能连 | ❌ | ✅ | ✅ | ✅ | ✅ | — |
| 价格 | 免费 | 系统自带 | 个人免费 | 免费 | 开源 | 个人免费 |

适合的场景：对面是 Windows，想用标准 RDP（mstsc 什么都不用装，或者 kRDPclient 走 H.264 硬件编解码），
中间不经过任何云账号。

### 安装

1. 从 [Releases](https://github.com/keelos-dev/hyperMAC/releases) 下载最新的 `.dmg` 并打开。
2. 把 **hyperMAC** 拖进「**应用程序**」。
3. 从「应用程序」打开 hyperMAC，按对话框走：
   - 输一次这台 Mac 的登录密码：客户端用的就是你的 Mac 账号（用户名 + 登录密码），不用另记密码；
   - 在「系统设置 → 隐私与安全性」里允许「**屏幕录制**」和「**辅助功能**」。
     「辅助功能」系统不会替你打开：要在列表里自己把 hyperMAC 的开关打开。
     macOS 26 第一次有人连上时还会再问一次「要采集屏幕和音频」，选允许；
   - 可选：打开远程登录（SSH），这样 kRDPclient 能直接开这台 Mac 的终端。
4. 之后 hyperMAC 在后台运行，每次登录自动启动。随时再打开 app 可以看状态、生成 kRDPclient
   token、改密码、卸载。

### 虚拟机（预览）

窗口的「**虚拟机**」页可以在这台 Mac 上新建、运行虚拟机；kRDPclient 里它们列在这台 Mac 后面，连法一样。

| 系统 | 跑在 | 状态 |
|---|---|---|
| macOS | 苹果的 Virtualization.framework | 用恢复映像（.ipsw）全自动安装 |
| Windows 11（ARM） | bhyve（来自 FreeBSD）+ Hypervisor.framework | 用微软原版 ISO 安装，可以在控制台窗口里手动装，也可以无人值守；有网络、1920x1080；Windows Update 跑通过 |
| Linux、FreeBSD（arm64） | Virtualization.framework / bhyve | 用发行版的 ISO 安装 |

需要自备：系统的安装盘（ISO / IPSW）；Windows 还要
[virtio-win 驱动 ISO](https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/)（网卡驱动）。
Windows 虚拟机带 TPM 2.0 和 Secure Boot，微软的安装程序原样运行，硬件检查不用关。

已知的缺口：Windows 里没有显卡加速
（基本显示驱动）；虚拟机没有声音；桥接网络、睡眠唤醒、长时间运行还没测。这部分是新的：会有毛糙的地方，重要的东西请留备份。

### 用什么客户端

**推荐 kRDPclient**：https://github.com/keelos-dev/kRDPclient-releases （Windows）。
在 kRDPclient 里「添加 keelOS 主机」，填 Mac 的地址和 Mac 的登录密码（kRDPclient 拿它换
一个 token，不保存密码），也可以填 hyperMAC app 里生成的 token。画面走硬件
编码的 H.264，声音、剪贴板也都有。

mstsc（远程桌面连接）等标准 RDP 客户端也能连（`Mac地址:3389`，用 Mac 的用户名和登录密码），
**但它们没有 H.264 通道**，画面走 RemoteFX / 位图，流畅度差一截。用 mstsc 觉得卡的话，
请先换 kRDPclient 试试。

### 已知限制

- Mac 上要有用户登录着：停在登录窗口（比如重启后没开自动登录）时连不上。建议在「系统设置 →
  用户与群组」里打开自动登录。
- Mac 要接着显示器，或者插一个假负载（HDMI 欺骗器）；什么都不接时 macOS 没有屏幕可抓。
- 剪贴板支持文字和图片（截图、复制的图片），文件还不支持。
- 鼠标指针是画在画面里的，还没有客户端本地光标。
- 登录用的就是你的 Mac 账号，知道 Mac 密码的人就能连：密码要设好，3389/443 端口别暴露到公网。
  hyperMAC 不保存密码本身，只为 mstsc 存一个密码的 NT 哈希（只有你的账号能读）。改了 Mac 密码后旧密码
  立刻失效；kRDPclient 马上能用新密码，mstsc 要先用新密码从 kRDPclient 登录一次（或者打开 hyperMAC 输一次）。
- 不会自己检查更新：在 app 里点「检查更新…」。

### 更新

打开 hyperMAC，点「关于」里的「检查更新…」。有新版本时会列出改了什么；点「更新」会下载新版本、
核对它是 hyperMAC 开发者签名并经苹果公证的，替换 app 并重启服务（正在连着的会话会断开几秒），授权
不用重新给。

手动覆盖：先退出 hyperMAC 的窗口（⌘Q；窗口开着时 Finder 不让覆盖，会说「hyperMAC 正在使用」），
再把新的 hyperMAC 拖进去覆盖，然后打开它。后台服务不用停。

### 卸载

1. 打开 hyperMAC，选「卸载自启」。
2. 删掉「应用程序」里的 hyperMAC 和文件夹 `~/Library/Application Support/hyperMAC`。
3. 可选：在「系统设置」的「屏幕录制」「辅助功能」里把 hyperMAC 删掉。

远程登录（SSH）是 macOS 自己的设置，卸载时不动它；不需要了就在「系统设置 → 通用 → 共享」里关掉。

### 从源码编译

需要 Xcode 命令行工具、Rust（rustup，stable）和 Python 3；Windows 虚拟机的 TPM 还要
`brew install libtpms openssl`（没有的话 bhyve 照常能编，只是不带 TPM）。

```sh
sh build.sh        # target/release/hypermac、vm/target/release/hypermac（虚拟机程序）、vm/bhyve/build/bhyve
```

钥匙串里有 "Apple Development" 证书时 `build.sh` 用它签名（重新编译后两项授权不丢），否则是 ad hoc 签名。
`target/release/hypermac install` 装成登录自启。各部分怎么配合、发布怎么打包见
[docs/DEVELOPING.md](docs/DEVELOPING.md)；虚拟机部分见 [vm/README.md](vm/README.md) 和 [vm/bhyve/README.md](vm/bhyve/README.md)。

这个仓库每次发版从开发仓库收到一份快照，一版一个提交。欢迎提 pull request：合并后会带进开发仓库，下一份快照里就有。

### 许可证

hyperMAC 自己的代码：MIT（[LICENSE](LICENSE)）。`vm/bhyve`（把 FreeBSD 的 bhyve 移植到 Hypervisor.framework）：
BSD-2-Clause（[vm/bhyve/LICENSE](vm/bhyve/LICENSE)）；`vm/bhyve/freebsd` 里 FreeBSD 的文件保留各自的文件头。

### 第三方软件

hyperMAC 基于 [IronRDP](https://github.com/Devolutions/IronRDP)（MIT / Apache-2.0）和其他开源
Rust crate；虚拟机部分用了 FreeBSD 的 [bhyve](https://bhyve.org)（BSD 许可）和 [EDK2](https://www.tianocore.org)
UEFI 固件（BSD-2-Clause-Patent）。它们的声明随每份副本一起发：dmg 根目录和 `hyperMAC.app/Contents/Resources/` 里的
`THIRD_PARTY.md`、`LICENSE-IronRDP-MIT`、`THIRD_PARTY-crates.txt`、`THIRD_PARTY-vm.txt`。
