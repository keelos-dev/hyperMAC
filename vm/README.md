# hyperMAC 的虚拟机程序（vm/）

在 Apple Silicon 的 Mac 上无头跑虚拟机：macOS、Linux、Windows 11 ARM guest。
宿主机上不开窗口；画面和键鼠经主服务的 RDP 给 kRDPclient（登录名 = VM 名，见仓库根目录 `src/vms.rs`），
macOS guest 里也可以装 hyperMAC 自己远程；安装、登录窗、救援时用控制台或 VNC。

这里是 VM 程序本身：每台 VM 一个 `hypermac run NAME` 进程。管理界面在主窗口的「虚拟机」tab
（根目录 `src/ui/vmui.rs`），它调用的就是这个程序。前期调研在根目录 `docs/vm-plan.md`、`docs/vm-notes.md`。

## 要求

- Apple Silicon，macOS 14 以上（开发机是 macOS 27.2；自己的 virtio 设备要 macOS 27）
- Rust（rustup stable）、Xcode Command Line Tools

## 编译

```sh
sh build.sh        # 仓库根目录：连同这个程序一起编译，装成 ~/Library/Application Support/hyperMAC/bin/hypermac-vm
sh vm/build.sh     # 只编这个：vm/target/release/hypermac，ad-hoc 签名
```

都带 `com.apple.security.virtualization` entitlement：不带的话 Virtualization.framework 不让启动 VM，所以别直接用 `cargo build` 出来的二进制。

## 用法

界面：hyperMAC 主窗口 →「虚拟机」。列出所有虚拟机（状态、规格、地址、网络），
「新建虚拟机…」选一个 `.ipsw`（macOS）或 `.iso`（Linux）再起名；每台有 启动 / 停止… / 控制台 / 拷贝地址 / 删除… / 桥接开关 / 弹出安装盘。
窗口不拥有虚拟机：每台是独立的 `hypermac-vm run NAME` 进程，窗口关了、服务重启了它们照样运行。

「控制台」是虚拟机自己的进程按需开的一个原生窗口（`src/console.rs`，`src/vm.rs` 的 `open_console`）：
画面、键盘、鼠标直接进虚拟机；关掉窗口虚拟机不受影响。窗口按比例缩放 guest 的画面，不改 guest 的分辨率。

命令行：

```sh
hypermac create mac1 --ipsw ~/Downloads/macOS_26.5.2.ipsw   # 建 VM 并安装 macOS，10–20 分钟
hypermac create fed1 --os linux --iso ~/Fedora-45-aarch64.iso  # 建 Linux VM（只建文件，秒回）
hypermac run mac1                                           # 运行（前台；无窗口，--console 同时开控制台）
hypermac ctl mac1 info      # pid、VNC 端口和密码、guest 的 IP
hypermac ctl mac1 console   # 打开控制台窗口
hypermac ctl mac1 stop      # 请求 guest 关机；关完进程退出
hypermac ctl mac1 kill      # 直接杀掉
hypermac ip mac1            # guest 的 IP
hypermac net mac1 bridged   # 桥接到宿主的默认网卡（bridged:en0 指定网卡）；nat 改回去。下次启动生效
hypermac list
```

`create` 的可选项：`--cpus N`（默认 4）、`--mem-gb G`（macOS 默认 6，Linux 默认 4）、`--disk-gb G`（macOS 默认 64，Linux 默认 32，稀疏文件）、`--w W --h H`（默认 1920x1080）。
Linux VM 每次启动都把 `vm.json` 里的 `iso` 当 U 盘挂上（装完系统后把这一项删掉）；`run NAME --iso PATH` 可以临时指定。
guest 的 macOS 版本不能比宿主新。

存储位置（主窗口「虚拟机」页底部可改，`settings.json` 的 `vm_dir`，默认是状态目录）照 keelOS 的数据集布局：`guests/NAME/`（一台 VM 一个目录）、`iso/`（安装盘）、`images/`（留给磁盘镜像）。旧布局的 `vms/` 在没有 `guests/` 时照用。一台 VM 的目录里：
`vm.json`（配置）、`disk.img`、`aux.img`、`hardware-model`、`machine-id`、`ctl.sock`。

## macOS guest 装好之后（2026-11-10 实测，guest macOS 26.5.2，宿主 MacBook Air M4 / macOS 27.2）

1. `hypermac run mac1 --vnc-port 5901`，用 VNC 客户端连 `127.0.0.1:5901`（密码在 `hypermac ctl mac1 info`），
   走完首次设置：建本地账号，跳过 Apple Account，**不开 FileVault**（开了就不能自动登录）。
2. guest 的终端里开 SSH：`sudo launchctl load -w /System/Library/LaunchDaemons/ssh.plist`，之后宿主可以 `ssh 账号@$(hypermac ip mac1)`。
3. guest 里装 macRDP：下载 hyperMAC 的 dmg，把 `macRDP.app` 拷进 `/Applications`，从 guest 的终端 `open /Applications/macRDP.app`
   （从 ssh 里 `open` 找不到 app）。输入登录密码，在系统设置里给「画面收录」和「辅助功能」两个权限，回到 macRDP 点 Restart Service。
4. 宿主上 `sdl-freerdp /v:$(hypermac ip mac1) /u:账号 /p:密码 /cert:ignore` 能登录并看到桌面
   （guest 的 macrdp.log：`NLA login`、`capturing id=1 w=1920 h=1080`）。

5. 自动登录（macRDP 是 LaunchAgent，要有人登录才起）：系统设置 → 用户与群组 → 自动登录的账号。
   `sysadminctl -autologin set` 在 guest 里报 error:22，只能走界面。关睡眠：`sudo pmset -a sleep 0 displaysleep 0 disksleep 0`。
6. `hypermac net mac1 bridged` 后重启 VM：guest 拿到局域网地址（实测 192.168.100.28），宿主用这个地址 RDP 连通；
   重启后停在登录窗时用 VNC 控制台输密码也试过，可用。

guest 里看到的硬件：`VirtualMac2,1`，显卡 `AppleParavirtGPU`（Metal 插件 `AppleParavirtGPUMetalIOGPUFamily`），
内核扩展 `AppleVideoToolboxParavirtualization` 已加载（硬件视频编码应该可用；H.264 通道要 kRDPclient 连上才用得到，还没实测）。

VNC 控制台的已知毛病：大写字母和 Shift 符号会变成小写（路径不分大小写所以命令照样能跑）；Command 组合键没试通，用鼠标点菜单栏代替。

## Linux guest（2026-11-10 实测，Fedora 45 Workstation Live aarch64）

2026-11-10 起新建的 Linux VM 默认用我们自己的 virtio 显卡和键鼠（vm.json `own_devices`；`--vz-devices` 用 VZ 自带的）：
控制台是我们的窗口，经 kRDPclient 远程时注入的鼠标它收得到（VZ 自带的画面视图收不到），kRDPclient 也能直接连这台 VM。
用自己的显卡时 Fedora Live 停在 GDM（不自动登录）：「Not listed?」→ liveuser、空密码。

`create --os linux --iso …` 后 `run`，约 35 秒到 GNOME 桌面（1280x800，4 核 4 GB）。VNC 控制台里鼠标点击、键盘输入都正常
（搜索框里敲 terminal 回车能开终端）。串口输出在 VM 目录的 `console.log`。
显卡是 Virtualization.framework 自带的 virtio-gpu，只有 2D：guest 的桌面和 OpenGL 都是 CPU 软件渲染（llvmpipe）。
还没做：装到硬盘、guest 里的远程桌面（gnome-remote-desktop / xrdp）、3D。

## Windows 11 ARM guest：bhyve 引擎（2026-11-11 起的默认）

Windows 不再走 Virtualization.framework + efiprobe，改跑在 `vm/bhyve`（FreeBSD 的 bhyve 移植到 Hypervisor.framework，见
[bhyve/README.md](bhyve/README.md) 和 [docs/bhyve-plan.md](../docs/bhyve-plan.md)）上：微软原版 ISO 直接当光驱，启动链不改，不需要 wimlib 和 efiprobe。

```sh
sh build.sh        # 仓库根目录：编 vm/bhyve，把 bhyve 和 QEMU_EFI.fd 装到 hypermac-vm 旁边（要有 ~/Developer/keelOS）
HYPERMAC_WIN_PASSWORD=密码 hypermac create win1 --os windows --iso Win11_arm64.iso --virtio virtio-win.iso --user win [--net bridged]
hypermac win-resume win1     # 中途停了从断点接着装
```

- 实测（2026-11-11，Mac mini M4，日文 26300 ISO）：8 分钟到桌面，1920x1080，NetKVM 已装，来宾的 3389 可连。
- `vm.json` 里 `"engine": "bhyve"`。`hypermac run` 是监管进程：来宾重启时重新起 bhyve，`ctl NAME stop` = 电源键。
  画面、键鼠、`ctl.sock`、`screen.shm`、VNC 口和别的 VM 一样用（`src/bhyve.rs`）。来宾的串口输出在 VM 目录的 `console.log`。
- 分辨率在创建时定（`--w --h`，最大 1920x1200），运行中不能改；声卡见下一条；没有 TPM 和 Secure Boot（安装时关掉了检查）。
- 声卡（2026-11-11）：HD Audio（`-s 16,hda,play=keel:<VM 目录>/sound.ring`，keelOS 的 hda，`bhyve/keel/`），Windows 用自带的
  `hdaudio.sys`，不用装驱动；`HYPERMAC_HDA=0` 不挂。来宾播放的声音写进 `sound.ring`（16 位立体声，来宾的采样率，布局在
  `bhyve/keel/audio.h`）。VM 的窗口开着时本进程用 AudioQueue 在 Mac 上播（`src/sound.rs`），窗口关了不出声；
  RDP 连着这台 VM 时服务读同一个环走 RDPSND（仓库根 `src/vmsound.rs`）。实测：设备管理器三项正常、440 Hz 的音在环里是 440 Hz、
  本机听到 Windows 的开机声；kRDPclient 连上这台 VM 听到声音（Opus）。mstsc（PCM）和 44.1 kHz 的转换没实测。只有 Windows 来宾有，没有录音。
- `create … --engine vz` 仍走下面的老办法；没装 bhyve 引擎的机器自动走老办法。

### 3D 加速（Direct3D 11，2026-11-11 实测：MacBook Air M4 / macOS 27.2，guest 26300 日文版）

`create … --os windows … --gpu3d`，或已有的虚拟机在 `vm.json` 里加 `"gpu3d": true`。guest 的 Direct3D 调用在宿主的 GPU 上执行：
桌面合成（DWM）、Edge、WebGL 都走它。实测 WebGL Aquarium 500 条鱼 57 帧（1024x768）。

要先造出 3D 引擎（不是默认构建的一部分，要 LLVM 15、meson，约十分钟）：`vm/bhyve/gpu3d.sh`，得到 `vm/bhyve/build/bhyve-3d` 和
`build/gpu3d/`（它旁边要有的东西，全在这个目录里）。之后根目录的 `build.sh` 会把它们装到 hyperMAC 旁边。没有引擎时带 `gpu3d` 的虚拟机照常启动，只是没有 3D。

怎么通的：

```
guest 进程 → d3d11.dll → Triton（viogpu3d，UTM 作者的 WDDM 驱动，微软签名）
   → virtio-gpu（vm/bhyve/hvf/pci_virtio_gpu_3d.c：上下文、blob 资源映射进 guest、fence、blob 扫描输出）
   → virglrenderer 的 Neptune（每个 guest 进程一个 virgl_render_server 进程）→ DXMT（Direct3D 11 → Metal）
```

- 3D 显卡是 fbuf 之外的第二块显示设备（`-s 9,virtio-gpu,3d=on,hostmem=4096`，和 QEMU 的布局一样：驱动只认这种）。固件、安装程序、
  没装驱动的系统仍然在 fbuf 上；驱动点亮画面后，fbuf 的 VNC（也就是控制台窗口、RDP、`vshot`）给出的就是它的画面。
- 驱动在新建虚拟机时随 setup.iso 进去（`gpu3d/driver/`），安装期间不挂 3D 设备，装完第一次启动 Windows 自己绑上。
- guest 启动后的分辨率是 `vm.json` 的 `w`×`h`（设备在驱动出第一帧后 5、20、60 秒各提醒一次，驱动包里的 viogpuap.exe 来改）。
- `hostmem` 是 guest 和渲染器共享内存的窗口，一个 64 位 BAR：主桥为此有了 64 位的 PCI 内存窗口（256 GB 处 32 GB，
  `patches/macos/0024`、`src/acpi_arm64_dsdt.asl`）。
- 调试：环境变量 `VTGPU3D_DEBUG=1`（每条 3D 命令）、`VTGPU_DEBUG=3`（寄存器读写），输出在虚拟机的 `run.log`；
  `VIRGL_LOG_LEVEL=debug` 是渲染器自己的。
- 还没有：Direct3D 12（DXMT 只有 11）、OpenGL/Vulkan、窗口缩放时改 guest 分辨率、主窗口里的开关；画面每帧整屏拷一次。
- 没查明：把 virtio-gpu 并进 fbuf 那个设备（`fbuf,virtio=on`，`patches/macos/0023`，keelOS 的 virtio-vga）时 Triton 显示「已启动」
  却从不访问寄存器，之后重启、关机都卡住。所以现在是两块设备。

## Windows 11 ARM guest：老办法，VZ + efiprobe（2026-11-10，Mac mini M4 / macOS 27.2：**能装到桌面**，guest 26300 日文版）

Virtualization.framework 不支持 Windows；靠自写的 EFI 程序 `tools/efiprobe` 补上 VZ 缺的东西。实测：全自动安装到桌面、自动登录、
网卡（virtio-win NetKVM）拿到地址、RDP 3389 可连（账号 `win` / `hypermac`）。

### 怎么装（2026-11-10 起全自动：主窗口「新建虚拟机…」→ Windows 11，或命令行）

```sh
HYPERMAC_WIN_PASSWORD=密码 hypermac create win1 --os windows --iso Win11_arm64.iso --virtio virtio-win.iso --user win [--net bridged]
hypermac win-resume win1     # 中途停了（失败、宿主重启）从断点接着装
```

`src/winsetup.rs` 做下面这些手工步骤：做安装盘（setup.img）和驱动盘（drivers.img），语言取 ISO 的 `sources/lang.ini`、
装 Pro 版（wimlib-imagex info）、电脑名 = VM 名；WinPE 换成跑 `tools/win11-usb/hm-setup.cmd`（boot.wim 里的 winpeshl.ini），
它装好驱动后 `setup.exe /noreboot`、再关机（VZ 的客户机重启宿主看不到，关机能看到）。宿主接着把 efiprobe 放进硬盘 EFI 分区、
换驱动盘、再开机；首次登录后 `hypermac.cmd done` 写标记并关机，宿主收尾、删两块盘、正式启动。
需要 Homebrew 的 wimlib（`brew install wimlib`）、virtio-win.iso、build.sh 装的 efiprobe.efi。实测：日文 26300 ISO，约 15 分钟到桌面。

以下是原来的手工步骤（自动流程照着它做的）：


1. `hypermac create win1 --os windows --iso <启动 U 盘镜像>`：硬盘走 VZ 的 NVMe（Windows 自带驱动），默认 8 GB 内存；
   Windows VM 不挂 virtio-console（virtio-win 的 vioser.sys 绑上 VZ 的会蓝屏 0x7E），挂一个 `fbdev.rs` 的空 virtio 设备（宿主借它读 guest 内存）。
2. 启动 U 盘：原始文件 + MBR + 一个 FAT32 分区（`mkfile -n 12g` → `hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount` →
   `diskutil partitionDisk diskN MBR FAT32 WIN11 100%`），拷微软 ISO 的内容（ISO 只有光盘引导记录，VZ 只能当 U 盘挂，直接挂 ISO 起不来）；
   `install.wim` 超过 FAT32 上限：`wimlib-imagex split install.wim /Volumes/WIN11/sources/install.swm 3800`。
   `\EFI\BOOT\BOOTAA64.EFI` 改名 `WINBOOT.EFI`，换成 efiprobe（`cd tools/efiprobe && cargo build --release --target aarch64-unknown-uefi`）。
   再放 `tools/win11-usb/` 的 `autounattend.xml`、`hypermac.cmd` 和 virtio-win 的 `NetKVM`、`viogpudo`、`viorng`（`w11/ARM64`）到 `\drivers\`。
   `.img` 启动盘以可写方式挂（efiprobe 往上写 `hmprobe.txt`）。
3. 第一阶段（WinPE 展开映像）完成、第一次重启时：停掉 VM，把 efiprobe 也装进硬盘的 EFI 分区——
   `\EFI\Microsoft\Boot\bootmgfw.efi` 改名 `winboot.efi`、换成 efiprobe（`\EFI\Boot\bootaa64.efi` 同样），
   不然固件直接启动没打补丁的启动管理器。efiprobe 在 U 盘上时也会优先启动已装好的系统（找 `winboot.efi` / `bootmgfw.efi`）。
4. 把 U 盘换成只放 `hypermac.cmd` + `\drivers` 的小镜像（vm.json 的 `iso`）：完整的安装介质还插着时，安装程序会停在
  「アップグレードを開始し、インストール メディアから起動したようです」对话框。之后 specialize 装驱动、OOBE 跳过、自动登录。

### efiprobe 补的东西（一关一关的原因）

1. **PMU**：Windows 到处读 `PMCCNTR_EL0` 等 PMU 寄存器（启动管理器 410 处、winload 418、内核 476+422、ntdll 82……），
   VZ 不给 guest PMU，是未定义指令。
   - 启动管理器、winload、内核、启动驱动：运行前改内存里的指令（读周期计数 → `CNTVCT_EL0`，其他读 → 0，写 → NOP，`exc.rs`）。
     启动管理器自己加载的映像靠钩住启动服务（`trace.rs`）记下它要的内存、周期扫描里面的 PE 映像：winload 只改加载后的映像（原始文件缓冲要算校验和），
     内核/hal/驱动在 `ExitBootServices` 前改（winload 已校验完）。改之前用 `EFI_MEMORY_ATTRIBUTE_PROTOCOL` 去掉只读（启动管理器让固件把代码页设成只读）。
   - 之后从磁盘加载的驱动和用户态程序：内核 `KiSynchronousException` 开头跳到一段模拟程序（`kpatch.rs`，放在内核 `.text` 末页的空位），
     EL0/EL1 的 EC 0 异常里，PMU 访问和 EL0 的 `mrs Xt, CurrentEL`（`_InterlockedDetectSupport` 拿它探测，靠 SEH 跳过）当场模拟。
     X19–X28 在调用方存在 `x1` 处（`KiEmulateReadRegister` 也从那里读）。
2. **显示**：VZ 的 EFI 只有 BltOnly 的 GOP，Windows 要线性帧缓冲。`shim.rs` 在内存里给一块（前一页是给宿主找的头），EFI 阶段定时器 Blt 到屏幕；
   之后 Windows 一直画在这块内存上，宿主 `hypermac ctl NAME fbshot` 存成 `fb.bmp`。控制台窗口和 VNC 还没接（还是黑的）。
3. **校验**：启动管理器的 `ImgpFilterValidationFailure` 改成返回 0。
- 打补丁的位置（`ImgpFilterValidationFailure`、`KiSynchronousException`、陷阱帧布局）是按这个版本的公开 PDB 写死的、先核对函数开头再改；
  **Windows 更新后会对不上**，要改成按特征找或自动取 PDB。Windows 更新也可能覆盖 EFI 分区里的 `bootmgfw.efi`。

### 自己的 virtio 显卡和键鼠（`HYPERMAC_VIRTIO=1`，2026-11-10 实测 Windows）

不用 VZ 自带的显卡和 USB 键鼠，换成我们自己实现的 virtio 设备（macOS 27 `VZCustomVirtioDevice`，从 macRDP 的 spike 搬来）：

- `vgpu.rs`：virtio-gpu 2D。Linux 用内核的 virtio_gpu，Windows 用 virtio-win 的 viogpudo（ARM64，装系统时 pnputil 装进驱动库了）。画面在 hyperMAC 进程里：`ctl NAME vshot` 存 `vscreen.bmp`。
- `vinput.rs`：virtio-input（绝对坐标指针 + 键盘）。`ctl NAME "vkey 键码 1|0"`、`"vabs X Y"`（0..1）、`"vbtn 0|1|2 1|0"`。
  自定义 virtio 设备收不到 guest 写配置区的通知（macOS 27.2 头文件里也没有），virtio-input 的 select/subsel 查询只能用一份固定字节回答。
  Windows 的 vioinput 把这份字节同时当 EV_KEY/EV_REL/EV_ABS/EV_MSC，依次探测鼠标、游戏杆、数位板、键盘；剩下的绝对轴不是恰好 X/Y 就整个设备失败
  （键盘那份全 1 的位图因此被拒、反复重置）。所以 Windows 上只建一个设备：指针的位图（REL_X/Y 成相对鼠标、ABS_X/Y 成数位板、顺带的键成键盘），
  再加几个 HID 用法码最大的键（ろ、変換、無変換、LANG1/2）把 vioinput 按键数组撑大——它按位图里最大的用法码分配数组、没有越界检查。键盘事件也发给这个设备。
- `rfb.rs`：自己的 VNC 服务（RFB 3.3/3.7/3.8，VNC 密码，Raw，DesktopSize），画面取 vgpu、输入送 vinput；VZ 自带的 VNC 只认它自己的显卡和 USB 键鼠。
  macOS「屏幕共享」能连：`open "vnc://:密码@127.0.0.1:端口"`。
- 实测 Windows 11：viogpudo 驱动我们的显卡（1024x768，Windows 自己选的）、开始菜单点得开、键盘打字进得去。
- 还没做：控制台窗口、改分辨率、只传变化区域、做成 vm.json 的设置。VZ 自带的私有 USB 光驱也试过：同样被权限门挡住。

### 调试手段

- `hypermac ctl NAME log`：efiprobe 的日志（guest 内存里的一段，相当于串口；`hmlog.rs`），卡死了也能读。异常时记全部寄存器、页表基址、前后指令。
- `hypermac ctl NAME "peek ADDR LEN"`：读 guest 物理内存；`pause` / `resume`：冻住 VM（比如在蓝屏那一刻）。
- 内核里的记录区（`kpatch.rs`，内核 `.data` 末尾）：模拟了多少次、没处理的指令（地址、指令、特权级）。
- 蓝屏参数：内核 `KiBugCheckData`（物理地址 = 内核加载地址 + RVA，内核在 `0x100000000`，日志里有）。
  内核虚拟地址 → 物理：`KiInitialProcess` 的 `DirectoryTableBase` + 0x800 当 TTBR1，4 KiB 页、顶层 256 项；模块表 `PsLoadedModuleList`。
- 微软符号服务器的公开 PDB（bootmgfw、winload_prod、ntkrnlmp）找函数和全局变量。
- VZ 自带的私有 PL011/16550 串口、GDB 调试桩、线性帧缓冲设备：一挂上 VM 就起不来（"Internal Virtualization error"，要内部权限）。

## 现状和限制

- **图形**：macOS guest 用苹果的半虚拟化显卡，guest 里有 Metal（宿主的 GPU）。没有 GPU 直通（Mac 上不存在）。
- **VNC 控制台**（给别的机器和自动化用；本机用上面的原生控制台）：Virtualization.framework 私有的 `_VZVNCServer`（`src/vnc.rs`），不需要窗口。它监听所有网卡，
  只靠 RFB 密码保护（`vm.json` 里随机生成的 8 位）；只用于安装和救援。私有接口，系统大版本升级后可能要改。
- **网络**：默认 NAT（guest 在 192.168.64.x，只有宿主能直接访问）。`net NAME bridged`（或 `create … --net bridged`）后 guest 直接在宿主的局域网上，
  用自己的 MAC 向局域网的 DHCP 要地址，别的机器可以直接连（kRDPclient 把它当一台独立的 Mac 加）。
  做法（`src/vmnet.rs`）：vmnet.framework 的桥接模式起一块接口，和 VM 之间用一对数据报 socket 转发以太帧
  （`VZFileHandleNetworkDeviceAttachment`）。VZ 自带的桥接要 `com.apple.vm.networking` 授权，ad-hoc 签名带上它会被系统直接杀掉（实测 exit 137），所以不用。
  macOS 27.2 实测普通用户就能起桥接的 vmnet（进程带 virtualization entitlement）；更老的系统要 root，
  那时用 `sudo hypermac netd-install` 装的 root 助手代起（这条路写了没实测）。Wi-Fi（en0）上桥接实测可用。
  桥接时 guest 的 IP 是从它发出的帧里学的（查 ARP 表不行：没有「本地网络」授权的进程拿到的是空表），所以 `ip` 要 VM 在运行。
- **声音**：guest 有一块输出到空的声卡（宿主不出声），给 guest 里的 macRDP 采系统声音用。
- 苹果限制同时最多跑 2 台 macOS guest。
- **Linux 的 3D**：没有。要做得自己实现带 Venus 的 virtio-gpu（macOS 27 的 `VZCustomVirtioDevice` 有共享内存区和 feature 位的接口，见 macRDP 仓库 `docs/vm-plan.md`）。
- 还没有：开机自启（不登录也跑的 LaunchDaemon，2026-11-10 用户说先不做）。keel API（`/v1/vms`）在主服务里。
