# 跑虚拟机：坑和实测记录

vm-plan.md 的实施过程中踩到的坑。按时间往下记。

## 2026-11-10 S1（spike：`spikes/s1-vz/`（已删，在 129b1ac 之前的历史里））

**带标题栏的 VM 窗口被 SCK 抓时 AppKit 会崩。** SCK 开始/停止抓某个窗口时，AppKit 收到"选择性共享状态变了"的通知，
去更新标题栏按钮（`NSThemeFrame _updateButtons`），重入后断言失败：
`_needsGeometryInWindowDidChangeNotificationCount > 0`（`-[NSView disableGeometryInWindowDidChangeNotification]`）。
抓/停抓几次就会撞上，VM 进程直接 abort，VM 跟着没了。改用无边框窗口（`NSWindowStyleMask::Borderless`）就没有这个问题，
抓到的画面也不带标题栏。VM 窗口一律无边框。

**抓窗口的进程要先连 WindowServer。** 纯命令行进程调 `SCContentFilter(desktopIndependentWindow:)` 会断言
`CGS_REQUIRE_INIT`（`SLSGetDisplaysWithRect`）。先 `NSApplication.sharedApplication` 一下。macRDP 本身有 AppKit，不受影响。

**窗口完全看不见就不出帧。** 见 vm-plan.md §8：被盖住、移出屏幕、隐藏、透明度 0 都不行，判断在 WindowServer 层，
覆盖 `occlusionState` 没用。

**无头 Mac mini 上建 CGVirtualDisplay 会顶掉系统占位显示器。** 这台 Mac mini 没插显示器，系统给了一块占位屏
（vendor `unkn` / product `virt`，1920x1080，displayID 4）。建一块 CGVirtualDisplay 后 NSScreen 只剩新建的那一块，
占位屏没了；撤掉虚拟屏后占位屏回来，但 **displayID 变了（4 → 6）**。

后果：macRDP 的抓屏流被系统停掉（"取り込みを行うディスプレイまたはウインドウが見つかりませんでした"）→ 断开用户的连接；
而 macRDP 在启动时缓存了主屏的 displayID（`main.rs` 的 `capture::primary()`），之后每次登录都是 `capture: display gone`，
重启服务（`launchctl kickstart -k gui/$(id -u)/dev.keelos.macrdp`）才恢复。

- 实测时用户正连着，被断了一次，重启服务后要手动重连。用户连着的时候别碰显示器配置（vm-plan.md §2）。
- macRDP 该修：每次连接（或者抓屏流被系统停掉后）重新取主屏，不要用启动时缓存的 id。
  插拔显示器、显示器配置改变都会触发同样的问题，不只是虚拟机。

**合成的鼠标按下/抬起太快会丢点击。** 几乎同时发出的 down/up 有时 guest 收不到点击，隔 80 ms 稳定。
