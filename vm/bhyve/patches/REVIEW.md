# 提给 FreeBSD 的补丁：整理（B8）

2026-11-11。基线：FreeBSD head `1de93128e75a`。`patches/upstream/` 里每个补丁开头是英文的提交说明，可以直接当 review 的描述用。
这份文件是提之前的清单：怎么分组、先后顺序、每个补丁在什么环境里验过、还缺什么。

**还没有提交任何东西。** 所有补丁只在 macOS 的 Hypervisor.framework 上跑过；提之前要在 FreeBSD 自己的 arm64 vmm 上各跑一遍
（一台 GICv3 的 arm64 机器，或者 arm64 上的嵌套虚拟化），通用补丁还要在 amd64 上回归。这一步没有环境，没做。

## 第一组：和平台无关的缺陷（各自独立，最容易进）

FreeBSD 上看不出来，是因为默认的互斥锁是 errorcheck 类型、pthread 对象是指针；换个 libc 就露出来。每个都是一两行到十几行。

| 补丁 | 问题 | 怎么发现的 |
|---|---|---|
| 0001 block_if | 错误路径拿 `pthread_mutex_t` / `pthread_cond_t` 和 `NULL` 比较，只在它们是指针的系统上能编 | macOS 上编不过 |
| 0002 virtio ISR | `vi_interrupt()` 在已经持有 `vs_mtx` 的路径上再锁一次；FreeBSD 上第二次加锁返回 `EDEADLK` 被忽略，里层解锁把外层的锁提前放掉 | Linux 第一次读盘时卡死 |
| 0003 nvme AEN | `aen_cond` 没初始化就用 | 带 nvme 的来宾卡住 |
| 0005 rfb | 销毁一个没初始化过的互斥锁 | 读代码 |
| 0015 bhyvegc | `bhyvegc_set_fbaddr()` 把调用方给的帧缓冲 `free()` 掉 | ramfb 切换模式时崩溃 |
| 0006 PCI BAR | 来宾把 BAR 临时指到别的设备占着的地址时注册失败，之后再也不注册；改成那段地址空出来后重试 | virtio-gpu 的 BAR 不通 |
| 0021 xhci | `pci_xhci_handle_transfer()` 持有端点的传输锁时调 `pci_xhci_try_usb_xfer()`，后者再锁同一把；和 0002 同一类（`EDEADLK` 被忽略、里层解锁放掉外层的锁） | Linux 的 soft lockup；`compat/` 的重入报告只报这一处 |
| 0022 nvme INTx | NVMe 只有 MSI-X：不申请传统中断，INTMS/INTMC 的写入被忽略；用不了 MSI-X 的来宾挂不上盘。补上引脚中断（MSI-X 关着时按规范的电平语义）和两个屏蔽寄存器 | FreeBSD/arm64 以 ACPI 启动（GICv3 没有 ITS）：`unable to allocate shared interrupt` |

## 第二组：帧缓冲和 VNC 不限 amd64

| 补丁 | 内容 |
|---|---|
| 0004 fbuf/rfb | `pci_fbuf.c`、`rfb.c` 在 amd64 以外也能编（VGA 部分和 SSE4.2 的 CRC 探测留在 amd64 里） |
| 0010 ramfb | `fbuf` 提供 QEMU 的 ramfb（fw_cfg 文件 `etc/ramfb`）：固件在来宾内存里分配帧缓冲，UEFI 有 GOP。依赖第三组的 0008 |
| 0019 ramfb 大小 | 把 `w=`、`h=` 经 fw_cfg `opt/bhyve/ramfb-mode` 告诉固件。**要和 EDK2 那边一起谈**（`firmware/edk2-bhyve.patch` 里 QemuRamfbDxe 的改动）；文件名是我们自己起的，上游可能想换个办法 |

## 第三组：arm64 的 UEFI（这一组是主体）

顺序即依赖关系。合起来的效果：原版 EDK2 的 `ArmVirtQemuKernel` 作为 bootrom 能启动，系统经 UEFI 引导。

| 补丁 | 内容 |
|---|---|
| 0008 fw_cfg MMIO | `qemu_fwcfg` 加内存映射接口、DMA、可写文件（原来只有 x86 的 I/O 端口；源码注释里写着其他平台需要 MMIO 接口） |
| 0009 fw_cfg 进设备树 | 设备树里加 `qemu,fw-cfg-mmio` 节点；设备树不再放在 bootrom 占的那段内存里 |
| 0012 PSCI | PSCI 节点同时声明 `arm,psci-0.2`（EDK2 只认这个） |
| 0013 PCI I/O | 64K 的 PCI I/O 空间加地址转换（EDK2 的 PCI 主桥驱动要求 I/O 窗口从 0 开始） |
| 0014 + `src/pflash.c` | CFI NOR 闪存，存 UEFI 变量（`-o bootvars=FILE`） |
| 0016 basl | 来宾 4 GB 以下没有内存时不生成 RSDT，只给 XSDT（否则 EDK2 把表分配到 4 GB 以下，一张都装不上） |
| `src/acpi_arm64.c`、`src/acpi_arm64_dsdt.asl` + macos/0017 | arm64 的 ACPI 表：FADT、MADT、GTDT、MCFG、SPCR、DBG2、DSDT、CPU 的 SSDT，电源键走 GED |

提之前要改的地方：

- **设备地址**：`macos/0011` 把 UART、RTC、fw_cfg 挪到 EDK2 ArmVirt 固件假定的窗口里（`0x09000000` 起）。上游的地址是 FreeBSD 自己定的，
  要么上游也挪，要么给 EDK2 加一个 bhyve 平台。这是和上游要商量的第一件事，决定第三组长什么样。
- **MSI**：`macos/0007` 和 MADT 里的 GICv2m frame 是因为 Hypervisor.framework 的 GIC 没有 MBI；FreeBSD 的 vmm 用 MBI，ACPI 表里对应的部分要按上游的方式写。
- **DSDT**：现在是离线用 `iasl` 编好放进仓库的；上游 amd64 是运行时调 `iasl`，arm64 用哪种要问。
- **0017 归类**：它在 `patches/macos/` 里只是因为上下文依赖 0007/0011，内容本身是通用的，提的时候要基于上游的文件重做。
- SMBIOS（经 fw_cfg 的 `etc/smbios/*`）还没做；IORT 表没有，FreeBSD 来宾在 ACPI 下拿不到 MSI（B5 的记录）。

## 第四组：virtio 1.0

| 补丁 | 内容 |
|---|---|
| 0018 virtio modern | 在 legacy 接口旁边加 virtio 1.0 的寄存器接口（内存 BAR 里的四个区 + PCI capability），设备变成 transitional；virtio-net 协商到 `VERSION_1` 时用 12 字节的头。`virtio.modern=false` 关掉 |

单独成组，和 arm64 无关但对 arm64 最有用（ARM64 的 Windows 驱动用不了 I/O BAR）。343 行，是最需要仔细审的一个：
packed virtqueue 没做；只有 virtio-net 按新旧接口区分了头的长度，其余设备（blk、rnd、console、scsi、9p）的配置空间在两种接口下相同，
但只有 blk/net/rnd/console 实测过。印象里上游有人提过 bhyve 的 virtio 1.0（没核实），提之前要先查现有的 review，避免重复。

## 不提的

- `compat/`、`hvf/`、`patches/macos/0007`、`0011`：macOS 专属。
- keelOS 的设备（USB 键盘、数位板、xhci 的改动、virtio-gpu）：属于 keelOS 的补丁队列，不在这里。
- `hvf/` 里补的符号扩展 MMIO 读（`LDRSB` 一类）：FreeBSD 内核的 `vmm_emulate_instruction()` 有同样的问题，应该另写一个内核补丁（§5a）。

## 建议的顺序

1. 第一组八个小补丁，各自一个 review——不依赖任何讨论，先建立往来。
2. 在邮件列表（freebsd-virtualization）上问第三组的两个设计问题：设备地址跟不跟 QEMU `virt`、DSDT 怎么生成。
3. 第二组的 0004（不依赖 fw_cfg），然后 0008 → 0009/0012/0013 → 0014 → 0010 → ACPI。
4. 0018 单独走。
