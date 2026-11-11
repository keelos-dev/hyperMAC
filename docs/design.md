# macRDP 设计

2026-11-09 起草。本文定下的决策不再重新论证；改决策先改这里。

## 目标

给 Mac 真机（首个目标：家里的 Mac mini M4）做一个 RDP 服务端，替代 NoMachine。
口号就一句：**只要 RDP 好用就行。**

- 客户端用现成的 **kRDPclient**（首选，能吃到 keelOS::Video、UDP、声音等扩展），
  mstsc / Windows App 也要能连（标准 RDP 路径）。
- 做法参考 keelOS：服务端骨架直接搬 keel-rdp（keelOS/rdp，IronRDP server），
  只把"画面从哪来、键鼠往哪去"这两头换成 macOS 的 API。

## 非目标

- 不做 NX 那种自有协议、文件传输、打印重定向、多人会话。协议就是 RDP + keelOS 已有扩展。
- 不做 Windows / Linux 服务端（Windows 自带；Linux 以后看需要）。
- 不做"新建虚拟会话"（Windows 服务端语义）。macRDP 接管的是**物理控制台桌面**，
  和 NoMachine / 苹果屏幕共享一样：连上看到的就是机器当前登录的那个桌面。
  （2026-11-10 起另有"跑虚拟机"：VM 控制台作为 guest 接进来，不是新建桌面会话，见下面「虚拟机」一节。）

## 总体结构

Rust。IronRDP server 与 keel-rdp 同一 pin（`8f9bf5b8` + `ironrdp-keel.patch`，
CredSSP 按用户名取密码/NT hash，Hybrid 同时接受 TLS-only 客户端），build.sh 照抄
keel-rdp 的 vendor 方式。协议侧代码从 keel-rdp 搬，尽量少改：

| 模块 | 来源 | 说明 |
|---|---|---|
| gfx（EGFX，RemoteFX Progressive） | 搬 keel-rdp `gfx.rs` | 标准路径，给 mstsc 和未开 Video 的 kRDPclient |
| video（keelOS::Video 通道，docs/71） | 搬 `video.rs` 框架 | 编码器换 VideoToolbox，见下 |
| pointer / clip / sound / auth / keymap | 搬相应文件 | 后端换 macOS 实现 |
| capture | 新写 | ScreenCaptureKit |
| input | 新写 | CGEvent 注入 |

keel-rdp 里纯 keelOS 的部分（guest.rs、agent.rs、usb.rs、web.rs、host.rs、dvmfeed.rs）不搬。
搬过来的文件头注明出处（同一作者的私有仓库之间复制，无许可问题；IronRDP 的 MIT
notice 规则与 kRDPclient 相同）。

## 画面：ScreenCaptureKit

- `SCStream` 抓主显示器，BGRA 的 `CMSampleBuffer`，附带 `SCStreamFrameInfo` 的
  dirty rects —— 正好对上 keel-rdp display 层"整帧 + 脏矩形"的输入形状。
- 帧率跟客户端 FRAME_ACK 走（keel-rdp 已有该流控），SCK 侧设 `minimumFrameInterval`
  上限 60fps。
- Retina：抓**物理像素**（pointer 坐标按 scale 换算）。客户端 1:1 显示由 kRDPclient 负责。

## 编码：VideoToolbox H.264 硬编（2026-11-09 定案）

两边极化原则（2026-10-09）：服务端用 M4 的硬编。

**为什么 H.264 不是 VP9**：Apple Silicon 的媒体引擎只有 H.264/HEVC/ProRes 硬*编*；
VP9 在 M3+ 只有硬*解*，没有硬编。Mac 上做 VP9 只能 libvpx 软编（吃 CPU、延迟差），
浪费 M4 的媒体引擎。而 H.264 两头都硬件：VideoToolbox 硬编 ↔ kRDPclient 的 Media
Foundation/DXVA 硬解（codec_id 3 现成）。用户 2026-11-09 拍板「先 H.264，之后补 VP9」。

- keelOS::Video 通道 codec_id **3 = H.264**（Annex B，一个 access unit，IDR 前带 SPS/PPS，
  docs/71 §4.4）。VideoToolbox `VTCompressionSession`，低延迟（`EnableLowLatencyRateControl`
  或 realtime），不用 B 帧，BT.709 limited。
- 协议 v1（keelOS docs/71 §4）：服务端建 DVC `keelOS::Video`；客户端 CAPS 报能力；
  服务端 START（codec 3）→ FRAME（关键帧先）→ STOP；客户端 FRAME_ACK/KEYFRAME_REQUEST。
  流控：只编最新帧不排队，在途到 max_in_flight 就不编；关键帧在 START 后和收到
  KEYFRAME_REQUEST 时发。
- 没开 Video 通道的客户端（mstsc）走 GFX（RemoteFX，keel-rdp 现成）。
- VP9 libvpx 软编：以后作为兜底可选（CAPS 里按客户端偏好选）。AV1/HEVC 更以后。

**实现分三步**：①协议骨架+通道挂载+CAPS 握手 ②VideoToolbox 硬编封装 ③接采集帧+流控。

## 键鼠：CGEvent

- 键盘：RDP scancode → CGKeyCode 映射表（keel-rdp 的 keymap 反着用；物理键语义，
  布局由 mac 侧决定，与 kRDPclient"scancode 优先"的哲学一致）。
  修饰键、Win 键→Command 的映射要一张明确的表写进文档。
- 鼠标：`CGEventCreateMouseEvent` 绝对坐标（按 Retina scale 换算回 pt），
  滚轮 pixel-precise（`CGEventCreateScrollWheelEvent`）。
- 注入需要 TCC 的 **辅助功能（Accessibility）** 权限；抓屏需要 **屏幕录制** 权限。
  首次装好后要人到机器前点两次授权（或 MDM 配置），README 写清楚。

## 声音

CoreAudio process tap（macOS 14.2+ 的 CATapDescription，系统混音 tap）→ RDPSND，
PCM 16-bit 立体声起步（keel-rdp sound.rs 的通道代码照搬，源换掉）。Opus 以后再说。
Mac mini 是 macOS 15+，不用考虑旧系统兜底。

2026-11-09 实况：先上的是 ScreenCaptureKit 的 `capturesAudio`（同一条 SCStream），它只复制混音、
不静音，Mac 和客户端两边都响。改为 tap.rs：全局立体声 tap，`CATapMutedWhenTapped`
（只在被读时静音，进程崩了 HAL 自动放开）→ 私有聚合设备（时钟=默认输出）→ IOProc → sound.rs；
两个开关互相独立（用户：Mac 放家里，远程放东西家里不该响）：macRDP「远程时 Mac 静音」
（settings.json `mute_mac`，默认开；MACRDP_MUTE_MAC=0）——有连接就静音，每秒重读、立即生效，
与客户端收不收声音无关；客户端出不出声由 kRDPclient 自己的声音开关决定。tap 起不来（<14.2）或「系统录音」权限被拒
（tap 只给静音、不报错：SCK 听得到而 tap 3 s 无声即判定）就退回 SCK，两边都响。
权限提示需要 NSAudioCaptureUsageDescription：app 的 Info.plist + build.rs 嵌进裸二进制。
mstsc 式按连接选择（ClientInfo 的 REMOTE_CONSOLE_AUDIO）未做：ironrdp-server 不暴露
ClientInfo flags，kRDPclient 也没有这个选项。

## 剪贴板

NSPasteboard ↔ cliprdr（keel-rdp clip.rs）。M1 只做文本（UTF-8/UTF-16），图片、文件以后。

## keelOS 主机兼容层（2026-11-09 定案并实现）

用户拍板「可以将 macRDP 等同 keelOS 的角色」：macRDP 在 443（`--web`）上用 RDP 同一张
证书服务 keel API 的最小子集——`GET /v1/version`（无需 token，kRDPclient 借它看证书）、
`GET /v1/vms`（Bearer token，返回一个 guest = 这台 Mac：uuid 存 state/uuid、alias=短主机
名、state 恒为 running、rdp.port）。`macrdp token` 生成 keel 原格式 token
（`keel_<12hex>_<43base64url>`，kRDPclient 的 looks_like_token 才会当 token 直用而不是
当管理员密码走 join 流程——join/login 这里不实现）。RDP 登录同时接受：NLA（passwd 设的
账号）和 TLS-only 的「任意用户名+token 当密码」（keelOS docs/19 的习惯）。电源等写操作
一律 403。

## 虚拟机（2026-11-10 定案，计划在 [vm-plan.md](vm-plan.md)）

用户：macRDP 加跑虚拟机的能力，主要 Linux 和 macOS，Windows 以后再说。第一期全用
Virtualization.framework（macOS guest 只有这条路）；VM 是独立进程 `macrdp-vm`（每台一个，per-VM
LaunchAgent），macRDP 把 VM 列进 `/v1/vms`，RDP 连 VM 控制台照 keel-rdp 的约定。第二期
（Hypervisor.framework + keelOS bhyve 设备模型）待用户拍板。

S1（2026-11-10）：VZ 窗口 + SCK 按窗口抓 + 合成 NSEvent 能用，但窗口必须真的显示在某块屏上，
完全被盖住/移出屏幕就不出帧；所以 VM 窗口要放在 macRDP 自己管的虚拟屏上（vm-plan.md §8，坑在 vm-notes.md）。
S1b（同日）：改走 macOS 27 的 `VZCustomVirtioDevice` 自己实现 virtio-gpu，Linux guest 无头运行，画面直接在我们进程里，
宿主机上不显示任何东西（用户要求）。这是 Linux 的显示方案；macOS guest 另议。

## 认证与 TLS

- 照 keel-rdp 的 auth.rs：配置文件里存用户名 + NT hash（装机时 `macrdp passwd` 生成），
  CredSSP/NLA 和 TLS-only 都接。**不**接 mac 本机账户密码（OpenDirectory 验证会把
  远程登录和本机口令绑死，且 NLA 拿不到明文没法过 PAM；没必要）。
- TLS：自签证书，首连比对指纹（kRDPclient 已有这套 UI）。证书和 hash 放
  `~/Library/Application Support/macRDP/`。
- 端口默认 3389。只在家庭网/keelGW 隧道里用，不暴露公网。

## 会话与生命周期（macOS 的三个难点）

1. **必须跑在用户的 GUI 会话里**：SCK 和 CGEvent 都要 Aqua session。所以是
   **LaunchAgent**（登录后起），不是 LaunchDaemon。前提：Mac mini 设自动登录。
2. **锁屏/登录窗**：loginwindow 的画面第三方抓不到（苹果私有接口）。决策：
   不解决。要求目标机自动登录 + 不锁屏（或连上后由人手输密码解锁——键盘注入在
   锁屏下仍可用，画面黑屏问题实测后记录）。这是和 NoMachine 的已知差距，写进 README。
3. **无头（不插显示器）**：Mac mini 无显示器时 WindowServer 只给低分辨率。
   方案按顺序试：(a) HDMI dummy plug（零代码，先用这个）；(b) CGVirtualDisplay
   私有 API 造虚拟显示器（BetterDisplay 的做法），配合 Display Control 动态跟随
   客户端窗口大小——这是 M3 的正经目标，RDP"好用"的关键一环。

## Display Control（动态分辨率）

kRDPclient 计划里有 per-tab 动态分辨率；服务端这边：有虚拟显示器时直接改虚拟
显示器 mode；物理显示器时用 CGDisplayConfiguration 挑最接近的 mode（不强求）。

## 里程碑

- **M1 能用**：TLS + NLA 登录，SCK 抓屏 → GFX，CGEvent 键鼠，mstsc 和 kRDPclient
  都能连上操作。手动前台跑。
- **M2 好用**：keelOS::Video H.264（VideoToolbox 硬编 + FRAME_ACK 流控），声音，
  剪贴板文本，远程指针形状。日常可替代 NoMachine 的主观标准在这档。
- **M3 顺手**：LaunchAgent 自启动、虚拟显示器 + Display Control、`macrdp` CLI
  （passwd/状态/日志）、UDP（MS-RDPEUDP2，服务端侧，IronRDP 的 udp feature）。
- **M4 收尾**：打包（签名 + notarize 的 .pkg，不走 App Store）、README、
  kRDPclient 侧需要的小配合（如有）。

## 构建与仓库

- 仓库 `keelos-dev/macRDP`（私有），master 直推，不开分支/PR。
- 在 Mac 上构建（Apple Silicon，rustup stable + Xcode CLT）；`build.sh` vendor
  IronRDP（同 keel-rdp）。本 Windows 机器只改代码，构建/测试在 Mac mini 上跑
  （ssh 过去，或列出命令由用户执行）。
- ObjC/Swift 桥：能用 `objc2` 系 crate（objc2-screen-capture-kit、objc2-core-graphics
  等，MIT）就不手写 bridge；VideoToolbox 用 FFI 直接调 C API。

## 登录窗（2026-11-09 实测结论与备选方案，用户：先不做）

macOS 偶尔会跳过自动登录（两次快速重启后出现过一次），Mac 停在登录窗 → 没有用户会话 →
LaunchAgent 不起 → RDP 不通。现状：用 TigerVNC（Apple Screen Sharing，系统服务，登录窗也在跑）
登录一次，服务随即自起。

备选（没做）：把 macRDP 拆成 LaunchDaemon（root，开机即起：web API 443 + `GET /v1/vms/{id}/vnc`
WebSocket 透传 localhost:5900 的 RFB 字节，不实现任何 VNC 认证）+ LaunchAgent（RDP+抓屏，现状）。
kRDPclient 现成的 VNC Console（右键 guest）就能在登录窗登录，`consoles.running` = 3389 是否在听，
体验等同 keelOS guest；VNC 走 443 TLS+token，可收掉裸 5900。约半天工作量。

## 开放问题

- 锁屏画面黑屏的实际行为（连接时已锁 vs 连接中锁定），实测后定补救（比如注入
  键盘唤醒 + 提示）。
- CGVirtualDisplay 私有 API 在新 macOS 大版本的稳定性；备选是 dummy plug 常驻。
- CoreAudio tap 的回声/采样率坑，实测再说。
- kRDPclient 连 macRDP 时 Command/Option 键的期望映射（Win=Command？需要用户用一阵定）。
