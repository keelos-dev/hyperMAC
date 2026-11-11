# hyperMAC

Mac 真机的 RDP 服务端，替代 NoMachine（设计与决策：[docs/design.md](docs/design.md)）。
协议栈搬 keelOS 的 keel-rdp（IronRDP，同一 pin + ironrdp-keel.patch），画面来源换成
ScreenCaptureKit，键鼠换成 CGEvent。客户端：kRDPclient 或 mstsc。

同时是虚拟机主机（2026-11-10 和原 hyperMAC 合并）：主窗口分「主机 | 虚拟机」两个 tab；
kRDPclient 把这台 Mac 当兼容 keelOS 的主机添加，列表里在这台 Mac 之后列出各台虚拟机，
用 VM 名登录就进那台虚拟机（`src/vms.rs`）。虚拟机程序在 [vm/](vm/README.md)（Virtualization.framework，
Windows 11 ARM 的 EFI 补丁 `vm/tools/efiprobe`，自己的 virtio 显卡/键鼠），待办见 [vm/docs/todo.md](vm/docs/todo.md)。

## 现状（2026-11-09 真机验证）

- TLS（自签证书，首连比对指纹）；NLA/CredSSP（mstsc 的方式，`hypermac passwd` 设的
  账号+密码）和 TLS-only（kRDPclient 的方式，密码=token）都接受
- ScreenCaptureKit 抓主显示器（point 分辨率，30fps 上限）→ RemoteFX / 位图；
  mstsc 和 kRDPclient 实测能看到桌面
- **keelOS 主机兼容层**（web.rs）：443 上用同一张证书服务 keel API 的最小子集
  （`/v1/version`、`/v1/vms`），`hypermac token` 发 keel 格式的 token——kRDPclient
  按「添加 keelOS 主机」的老办法填地址+token 即可，树里出现这台 Mac 一个 guest。
  也可以直接填 **hyperMAC 的密码**（`/login`、`/v1/tokens`、`/logout`，keel 的登录流程里
  kRDPclient 用到的那部分）：hyperMAC 只有 `hypermac passwd` 设的那一个账号，所以和 keelOS 的
  管理员一样只凭密码认；5 次输错锁 5 分钟
- **自动发现**（l2/，keelOS docs/113 的 KL2 协议，主机一侧）：kRDPclient 的
  Connection > Discover keelOS 能找到同一网段的 Mac，双击输 hyperMAC 密码即加进主机树，
  kRDPclient 不用改。UDP 7385：DISCOVER 回 OFFER（uuid、主机名、网卡 MAC/IPv4、443 端口、
  证书公钥的 SHA-256）；OPEN 开一条二层会话（可靠流，`l2/stream.rs` 与 kRDPclient 同一份
  代码），接到本机 web 面，里面是 web 面自己的 TLS。和 keel 的区别：keel 用 BPF 写整帧（没有
  IP 也应答），Mac 上 BPF 要 root 且 Mac 总有 IP，所以这里是普通 UDP socket，应答单播回
  请求方（不在同一子网时改为广播到它的端口）。`MACRDP_L2=0` 关。只在同一广播域内有效。
  不服务 keelOS 的 Management Network（经二层改主机网络）。单独验证：
  `cargo test --release live_responder -- --ignored --nocapture`（ssh 里就能跑，不需要抓屏）
- **键鼠**（CGEvent）：全量 scancode→Mac 键码转发，Mac 端输入法/布局合成（中文输入法可用），
  Win=Cmd、Alt=Opt；需「辅助功能」TCC 权限（不会自动弹框，手动加终端）
- **H.264 硬编**（keelOS::Video 通道，codec_id 3）：kRDPclient 连上自动切——SCK 帧零拷贝
  喂 VideoToolbox（M4 媒体引擎）→ Annex B → kRDPclient DXVA 硬解；hypermac CPU ~6%。
  mstsc 不开这个通道，照旧 RemoteFX。`MACRDP_VIDEO=0` 关，`MACRDP_VIDEO_KBPS` 调码率（默认 10000）
- **声音**：ScreenCaptureKit 系统音频 → RDPSND，48k 立体声、20 ms 一包、静音不发。客户端支持 Opus
  （kRDPclient）就发 Opus（libopus 静态链接，默认 96 kbps，`MACRDP_OPUS_KBPS` 调），否则（mstsc）发
  PCM（约 1.5 Mbps）；`MACRDP_OPUS=0` 只给 PCM。日志每 10 s 一行 `RDPSND: 10 s` 带 codec 和 kbps。
  `MACRDP_SOUND=0` 关。连的是 Windows 虚拟机时，声音来自虚拟机的声卡（`vmsound.rs` 读 `<VM 目录>/sound.ring`，
  见 [vm/README.md](vm/README.md)），同一条 RDPSND（2026-11-11 kRDPclient 连 w3d 实测有声音，Opus）
- **剪贴板**：文本和图片双向（NSPasteboard ↔ CLIPRDR，含中文；图片：Mac 的 PNG/TIFF ↔ CF_DIB 和注册格式 "PNG"，`clipimg.rs`）；`MACRDP_CLIP=0` 或 app 里的开关关。文件不做（待办：CLIPRDR 文件流，kRDPclient 也要做）
- **常驻服务**（2026-11-09 起 Mac mini 上就是这么跑的）：`hypermac install` 把二进制拷到
  `~/Library/Application Support/hyperMAC/bin/hypermac` 并装 LaunchAgent `dev.keelos.hypermac`
  （登录自启、崩溃重拉）——服务跑这个副本，不跑 target/release（重编会让运行中的二进制签名失效、
  TCC 当场拒绝）。build.sh 用 Apple Development 身份签名（identifier `dev.keelos.hypermac`）并刷新
  副本。TCC 对裸二进制按**路径**记：首次要在系统设置 → 隐私与安全性 → 屏幕录制、辅助功能
  里 `＋` 加 `bin/hypermac`（`~/Library` 隐藏：文件框里敲 `/` 或 ⌘⇧G 输路径，或先
  `chflags nohidden ~/Library`）；之后路径和签名都不变，不用再授。macOS 26 还会弹一次
  「hypermac 要采集屏幕和音频」确认，点許可。
  更新：`sh build.sh`（在 GUI 终端跑，ssh 里钥匙串锁着签不了）→ `launchctl kickstart -k gui/501/dev.keelos.hypermac`。
  日志 `~/Library/Application Support/hyperMAC/hypermac.log`。macOS 偶尔跳过自动登录停在登录窗时，
  用 TigerVNC 登一次，服务随即起。
- 还没有：客户端本地光标（指针形状 PDU）、VP9 软编兜底、动态分辨率

## 功能 × macOS 版本

✅ 真机实测通过 · ☑️ 代码不分版本、应能用但没在这个版本上测 · — 没测

| 功能 | 13–15 | 26（Tahoe） | 27.2（Golden Gate） |
|---|:-:|:-:|:-:|
| 启动、TLS/自签证书 | — | ✅ | ✅ |
| mstsc（NLA，Mac 账号+密码） | — | ✅ | ☑️ |
| kRDPclient（keelOS 兼容层、token/密码登录） | — | ✅ | ✅ |
| 自动发现（KL2，Discover keelOS） | — | ✅ | ✅（服务起来了） |
| 抓屏 ScreenCaptureKit → RemoteFX / 位图 | — | ✅ | ☑️ |
| H.264 硬编（VideoToolbox → keelOS::Video） | — | ✅ | ✅ |
| keelOS::Video v2（第二条 TCP） / UDP | — | ✅ | ☑️ |
| 键鼠（CGEvent，中文输入法、平滑滚轮） | — | ✅ | ✅ |
| 声音（系统音频 → Opus / PCM） | — | ✅ | ☑️（通道建立、Opus 协商通过） |
| 剪贴板 文本 | — | ✅ | ☑️ |
| 剪贴板 图片（PNG/TIFF ↔ DIB/PNG） | — | ✅ | ✅（客户端 → Mac） |
| 常驻服务（LaunchAgent 登录自启、崩溃重拉） | — | ✅ | ✅（升级重启后自己起来） |
| TCC 授权跨系统升级保留 | — | — | ✅（26 → 27.2 不用重授） |
| 会话期间不让显示器休眠（caffeinate） | — | ✅ | ☑️ |
| 关机/重启通知客户端 | — | ✅ | ✅ |
| hyperMAC.app、首次设置、检查更新 | — | ✅（MacBook Air，发布版） | — |

- 只支持 Apple Silicon（arm64）。H.264 硬编依赖媒体引擎；M4 实测编码器只有 H.264/HEVC/ProRes/JPEG。
- `LSMinimumSystemVersion` 是 13（ScreenCaptureKit 抓系统音频要 13+），但 13–15 一台都没测过。
- 26 起 ScreenCaptureKit 会弹「要采集屏幕和音频」的二次确认（大约每月一次），27.2 照旧。
- 27.2 一栏是 2026-11-09 Mac mini（M4）升级后，用 kRDPclient 实际连接、看服务日志确认的。

## 和主流方案比较

按各家公开资料整理（2026-11），除 NoMachine、Apple 屏幕共享（TigerVNC）以外都没有在这台 Mac 上实测。

| | hyperMAC | Apple 屏幕共享 | NoMachine | Chrome Remote Desktop | RustDesk | Parsec | Jump Desktop Connect |
|---|---|---|---|---|---|---|---|
| 协议 | RDP | VNC（RFB）+ Apple 扩展 | NX | WebRTC | 自有 | 自有 | Fluid / RDP / VNC |
| Windows 自带 mstsc 能直连 | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | 部分（RDP 模式） |
| 要账号/云服务 | 不要 | 不要 | 不要（局域网） | 要 Google 账号 | 不要（可自建中继） | 要 | 要 |
| 视频编码 | H.264 硬编（kRDPclient，keelOS::Video）/ RemoteFX、位图（mstsc）；声音 Opus / PCM | VNC 编码（Raw/ZRLE/Tight…）；高性能模式（两端都是 Apple Silicon Mac）HEVC | H.264、VP8 | VP8/VP9（WebRTC） | VP8/VP9/AV1，可选硬件 H.264/H.265 | H.264、H.265 | 有硬编 |
| Windows 客户端看 Mac 的流畅度 | 好：1080p 30 fps（局域网 `MACRDP_FPS=60`），实测解码 7.6 ms、6–10 Mbps、Mac CPU ~6%，外网 +23 ms | 差（TigerVNC 实测） | 好 | 中 | 中–好 | 很好（游戏级延迟） | 好 |
| 声音 | ✅ Opus | 只有高性能模式 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 剪贴板 | 文本、图片 | 文本 | ✅ | 文本 | ✅ | 文本 | ✅ |
| 文件传输 | ❌（待办） | ✅（Mac 客户端） | ✅ | ❌ | ✅ | ❌ | ✅ |
| 登录窗/锁屏 | 锁屏可解锁；登录窗进不去（自动登录，偶尔要用 VNC 登一次） | ✅ | ✅ | ✅ | ✅ | 部分 | ✅ |
| 价格 | 免费、开源 | 系统自带 | 10 起收费（个人也要订阅） | 免费 | 开源免费 | 个人免费 | 收费 |

hyperMAC 的定位：Windows 那头用 **mstsc 不装任何东西**就能连，用 kRDPclient 则走 H.264 硬编、和 keelOS 主机在同一棵树里管理；
不依赖任何云账号。短板是登录窗、文件传输、多显示器/动态分辨率还没做。

## 在 Mac 上构建、运行

```sh
# 一次性：rustup（stable）+ Xcode Command Line Tools
git clone git@github.com:keelos-dev/hyperMAC.git && cd hyperMAC
sh build.sh                      # vendor IronRDP（pin + patch）并 cargo build --release；连同 vm/ 一起编、装好 hypermac-vm

./target/release/hypermac passwd ben    # 密码从 stdin 读一行
./target/release/hypermac               # 监听 0.0.0.0:3389
```

首次运行会被 TCC 拦：系统设置 → 隐私与安全性 → **屏幕录制** 里允许 hypermac（终端里跑
则是允许终端），之后重新启动它。键鼠注入还要加一次**辅助功能**授权。

客户端连接：mstsc 或 kRDPclient 连 `mac地址:3389`，用户名/密码即 `passwd` 设的；证书
是自签的，首连核对指纹。日志级别用 `RUST_LOG` 控制（如 `RUST_LOG=debug`）。

## Release

开发在私有仓库 `keelos-dev/hyperMAC-dev`；每次发版用 `tools/publish.sh` 把当时的代码作为**一个提交**放进公开仓库
`keelos-dev/hyperMAC`（MIT，`vm/bhyve` 是 BSD-2-Clause；不带开发历史，`.gitattributes` 里 export-ignore 的开发记录不放；
公开仓库的 README 是 `README.public.md`，这份 README 在那边是 `docs/DEVELOPING.md`）。别人的 pull request 在公开仓库合并后，
马上搬进这里并把它的提交号记进 `tools/public-imported.txt`，否则下次导出会被拒绝。编译好的 dmg 发在公开仓库 [keelos-dev/hyperMAC](https://github.com/keelos-dev/hyperMAC) 的 Releases 里。
不上 App Store、不做 Homebrew；更新只在用户点 app 里的「检查更新…」时做（`src/update.rs`，照 kRDPclient：
看 hyperMAC 最新的 release，dmg 按 GitHub 给的 SHA-256 校验，新 app 要是本团队的
`dev.keelos.hypermac` 签名且 Gatekeeper 接受，替换 `/Applications` 里的 app、重启服务和窗口）。

**打包**：在 Mac 的 **GUI 终端**里（ssh 里钥匙串锁着，`codesign` 报 `errSecInternalComponent`）：

```sh
sh tools/package.sh      # -> dist/hypermac-<major.minor>-<commit>-<UTC时间>-macos-arm64.dmg + sha256
```

一条命令做完：检查（工作区干净、HEAD 已推到 origin，否则名字带 `-dirty`、提示别发布；
`THIRD_PARTY-crates.txt` 对得上 `Cargo.lock`）→ 编译（arm64，vendor 和 build.sh 共用
`tools/vendor-ironrdp.sh`）→ 组 `hyperMAC.app`（`Info.plist` 版本号取 `Cargo.toml`，`LSUIElement`，
`LSMinimumSystemVersion` 13）→ Developer ID 签名（hardened runtime，没有 entitlements——抓屏、
注入、VideoToolbox 都不需要；以后真要加，在脚本里写明原因）→ app 单独公证 + staple → dmg（app、
`/Applications` 快捷方式、三个声明文件、中英 README.txt）签名、公证、staple → `spctl` 验 dmg 和
app 都是 `accepted` / `Notarized Developer ID`。脚本不碰现役服务。

一次性准备：
- 登录钥匙串里有 **Developer ID Application** 证书（Xcode → Settings → Accounts → Manage
  Certificates → `+`；只有团队的 Account Holder 能建）。有多张时 `MACRDP_RELEASE_IDENTITY` 指定。
- 公证凭据存成 keychain profile `hypermac-notary`。这台 Mac 用的是 App Store Connect API 密钥：
  `xcrun notarytool store-credentials hypermac-notary --key <AuthKey_….p8> --key-id … --issuer …`
  （也可以用 Apple ID + App 专用密码）。脚本只引用 profile 名，**任何密码都不进仓库、不进脚本**。
- Python 3.11+（`tomllib`）和 keelOS 仓库（`~/Developer/keelOS` 或 `$KEELOS_DIR`；只有重新生成第三方声明时要，编译不需要）：
  `Cargo.lock` 一变就跑 `python3.11 tools/third-party.py` 重新生成 `THIRD_PARTY-crates.txt` 并提交。
  它用 keelOS 的 `tools/licenses/check.py`（allowlist、exceptions、读 crate 的代码）；许可证不在
  allowlist 里的算 finding，不写文件、不发布。`Cargo.lock` 因此入库。

**虚拟机部分**（1.1 起）：`package.sh` 还编 `vm/`（`hypermac-vm`）和 `vm/bhyve`，放进
`Contents/MacOS/`，固件 `QEMU_EFI.fd` 放 `Contents/Resources/`；两个程序各自带 entitlement 签名
（`hypermac-vm`：virtualization；`bhyve`：hypervisor + virtualization），主程序仍然没有。efiprobe 不打包
（发布版的 Windows 只走 bhyve）；TPM 在 bhyve 进程里（libtpms + OpenSSL 的 libcrypto 静态链接，
用 Homebrew 的 `libtpms`、`openssl`；`package.sh` 检查 bhyve 确实带了它、且不依赖 Homebrew 的动态库），不需要 swtpm。
声明：`THIRD_PARTY-vm.txt`（`tools/third-party-vm.py` 打包时生成）、`THIRD_PARTY-vm-crates.txt`（`third-party.py` 一并生成，入库）。

**第三方声明**：`THIRD_PARTY.md`（IronRDP 上游、pin `8f9bf5b8`、`ironrdp-keel.patch`）、
`LICENSE-IronRDP-MIT`、`THIRD_PARTY-crates.txt`，放在 `hyperMAC.app/Contents/Resources/` 和 dmg 根目录。

**发布**：每个版本一个 release，tag = dmg 文件名去掉 `.dmg`，附件是 dmg 和 `dist/changelog.json`
（「检查更新」列改动用），正文写这一版的说明和 sha256（`gh release create <tag> dist/<tag>.dmg
dist/changelog.json -R keelos-dev/hyperMAC ...`）。发布前在
Mac 上从 release 页**下载**（带隔离属性，才是用户的真实路径）走一遍：Gatekeeper 不拦 → 首次设置
→ kRDPclient 有画面、声音、键鼠、剪贴板。Developer ID 签名和开发签名的 TCC 授权不通用，
换签名后两项授权要重给。验证要停现役服务时先跟用户说（他要换 TigerVNC 进来）。

**app 的行为**（`src/ui.rs` 窗口，`src/gui.rs` 检查和小对话框）：可执行文件在 `.app` 里、又没带参数
（= 双击）时开一个「系统设置」风格的窗口（分组卡片、NSSwitch 开关、SF Symbols 图标）；被 launchd
拉起的服务本体总带 `--state`，不弹任何东西，**也不能碰任何会向 LaunchServices 登记的 AppKit**
（见 `src/cursor.rs` 的 `bitmap`：服务一登记，系统就把它当成「正在运行的 hyperMAC」，双击打不开窗口）。
- **菜单栏图标**（`src/menubar.rs`）：`hypermac menu`，第二个 LaunchAgent `dev.keelos.hypermac.menu`
  （装自启时一起装，卸载时一起卸）。它是 app 唯一常驻的界面进程：设置窗口在它里面开，没有 Dock 图标；
  双击 hyperMAC.app 由系统转成对它的 reopen，开窗口。图标：等待连接 / 已暂停 / 服务没在跑；有人连着时
  隐藏，让位给 macOS 自己的紫色录屏指示器（它的「停止共享」= 断开所有连接，`src/capture.rs` 的 SCStreamDelegate）。
  菜单：打开 hyperMAC…、断开当前连接（给服务发 SIGUSR1，`RpcInitiatedDisconnect`，kRDPclient 不自动重连）、
  暂停 / 恢复远程（`settings.json` 的 `paused`：服务拒绝新连接，已连着的不断；有连接时先确认）。
  状态来自 `<state>/service.json`（pid）和 `status.json`（连接数，服务每次变化时写），每秒读一次。
  菜单栏进程没在跑时双击：写 `<state>/open-window`，装好（1.0 升级上来时还没有）并 kickstart 它，自己退出。
  「检查更新」装好后的「立即重启」：重启服务，再让 launchd 在新可执行文件上重启菜单栏进程并开窗口。
- 不在 `/Applications`：只提示先拖进去（不装自启）。
- 第一次（没有 `credentials.json`）三页：输一次这台 Mac 的登录密码（系统核对）→ 两项授权（触发
  系统授权框，状态每 2 秒刷新）→ 装自启 → 远程登录没开时问要不要开（开关默认关；开 = 带去
  「系统设置 → 共享」，hyperMAC 不经手管理员密码、不改 sshd 配置）→ 主页。
- 主页：连接（地址、证书指纹、token）、设置（登录时自动运行、H.264、声音、剪贴板、远程登录）、
  权限、账号（Mac 密码改过时提示重新输入）、重启服务、卸载。视频/声音/剪贴板写进
  `<state>/settings.json`，服务每个新连接读一次（环境变量 `MACRDP_VIDEO` 等仍优先）。卸载不关远程登录。
- 看界面：`HOME=<临时目录> MACRDP_UI_PREVIEW=welcome|permissions|ssh|main MACRDP_UI_LANG=zh|en
  hyperMAC.app/Contents/MacOS/hypermac`（直接开那一页，不碰真实设置）。
- 升级：新 app 拖进 `/Applications` 覆盖后双击，服务还在跑旧的可执行文件（`service.json` 记的
  mtime 不同）就 `kickstart -k`；自启指向别处（开发副本）时先问再换。
- `install` 在 `.app` 里时不拷贝，LaunchAgent 直接指向 app 里的可执行文件（授权列表里显示
  hyperMAC.app）；开发构建照旧拷到 `bin/hypermac`。

**账号**（`src/account.rs`，用户 2026-11-09："就是用户自己的密码"）：登录用的是这台 Mac 的账号和
登录密码，不另设密码。系统不给密码（只有 root 能读的加盐哈希），但能核对输入的密码：TLS / web
登录每次用 OpenDirectory `verifyPassword` 现核，Mac 改了密码立刻生效。mstsc 的 NLA 只能用
密码的 NT 哈希，macOS 不存，所以 app 设置时让用户输一次登录密码（系统核对后）只存 NT 哈希；
之后任何一次经系统核对成功的密码登录会顺手更新它。哈希旁边记着当时账号的
`passwordLastSetTime`（`dscl . -read /Users/<user> accountPolicyData`，用户自己可读）：Mac 密码一改，
留着的哈希立刻作废，NLA 拒绝，旧密码不会继续对 mstsc 有效——任何时候能登录的只有当前的 Mac 密码。`hypermac passwd USER` 仍可设一个单独的密码
（开发用；旧安装的 `credentials.json` 带 `password` 字段时照旧比对）。`hypermac check-password`
用来确认系统接受某个密码（不写任何东西）。

**待办**：日文界面以后补。
