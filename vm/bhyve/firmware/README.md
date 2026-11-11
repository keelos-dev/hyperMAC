# UEFI 固件

`QEMU_EFI.fd`（release）和 `QEMU_EFI.debug.fd`（debug：串口上打印每个驱动的加载和断言）是 EDK2 的 `ArmVirtQemuKernel` 平台，
标签 `edk2-stable202608`（提交 `2970e5699ba6`）加上 `edk2-bhyve.patch`，GCC 14.2，在 Debian 13 arm64 里原生编的（那台 Debian 就跑在这套 bhyve 上）。
许可：BSD-2-Clause-Patent。

编译时带 `-D SECURE_BOOT_ENABLE=TRUE`（2026-11-11 起）：固件校验启动映像的签名。密钥在 UEFI 变量里（bhyve 的 `bootvars` 文件）：

- `QEMU_VARS.secboot.fd`：预置好密钥、Secure Boot 打开的变量文件，hypermac 新建 Windows VM 时拷成它的 `vars.fd`。
  用编译产物里的空模板 `QEMU_VARS.fd` 和 Debian 的 `python3-virt-firmware` 生成：
  `virt-fw-vars -i QEMU_VARS.fd -o QEMU_VARS.secboot.fd --enroll-generate "hyperMAC Platform Key" --secure-boot`
  （PK 是当场生成的自签证书，私钥不保留；KEK 和 db 是微软的证书，含 Windows 和第三方 UEFI CA；dbx 是工具自带的初始值）。
- 变量文件是空的（旧 VM、FreeBSD / Linux 的 VM）时固件处在 setup mode，Secure Boot 不生效，行为和以前一样。
- 这个平台没有受保护的变量存储（没有 SMM / StandaloneMM）：客人系统的内核能直接改写闪存。够让 Windows 认为 Secure Boot 开着，不等于真机的强度。
- 固件没有开 `TPM2_ENABLE`：不做度量启动，TPM 只由系统自己用（ACPI 的 TPM2 表是 bhyve 给的）。

`edk2-bhyve.patch` 两处改动：

- 平台里加进 SATA（AHCI）驱动：bhyve 的光驱是 AHCI 上的 ATAPI 设备（Windows 的安装盘），QEMU 的 `virt` 机型没有 SATA，所以原版平台不带。
- `QemuRamfbDxe` 读 fw_cfg 文件 `opt/bhyve/ramfb-mode`（宽、高，各 32 位小端）：原版只有 640x480、800x600、1024x768 三种模式，
  Windows 没有显卡驱动时沿用固件的模式，最大只能到 1024x768。bhyve 把 `fbuf` 的 `w=`、`h=` 写在这个文件里（`patches/upstream/0019`），
  固件把它加成一个模式并作为默认。

启动画面的图标：`Logo.bmp` 换掉 EDK2 的 `MdeModulePkg/Logo/Logo.bmp`（`LogoDxe` 用它，原来是 TianoCore 的图标）。
它由 `assets/hypermac-1024.png` 缩到 256x256、合成到黑底上（固件画面是黑的）、存成不压缩的 24 位 BMP（PIL：`resize` + `alpha_composite` + `save`）。
平台本来就带 `BootGraphicsResourceTableDxe`（ACPI 的 BGRT 表），所以 Windows 启动时转圈的上方显示的也是这个图标。
图标是二进制文件，不放进补丁，编译前拷过去（见下面）。

```sh
# 在一台 aarch64 的 Debian 里：
apt-get install -y build-essential git python3 python-is-python3 uuid-dev acpica-tools nasm
bash build-edk2.sh edk2-stable202608 -p ArmVirtPkg/ArmVirtQemuKernel.dsc     # RELEASE（第一次：取源码）
(cd edk2 && git apply --ignore-whitespace ../edk2-bhyve.patch) && cp Logo.bmp edk2/MdeModulePkg/Logo/Logo.bmp && bash build-in-tree.sh -D SECURE_BOOT_ENABLE=TRUE   # 打补丁后重编；B=RELEASE 编 release
# （补丁在仓库里是 LF 换行，EDK2 的文件是 CRLF，所以要 --ignore-whitespace）
# debug 版：把脚本里的 -b RELEASE 换成 -b DEBUG
# 产物：edk2/Build/ArmVirtQemuKernel-AArch64/{RELEASE,DEBUG}_GCC/FV/QEMU_EFI.fd
```

为什么是这个平台：它是"像内核一样加载"的可重定位固件——从 x0 拿设备树、设备从设备树里找，正好是 bhyve arm64 的 bootrom 约定。
它对机器的假设来自 QEMU 的 `virt` 机型，bhyve 这边为它补的东西见 `docs/bhyve-plan.md` 的 B4。

用法：

```sh
build/bhyve ... -o bootrom=firmware/QEMU_EFI.fd -o bootvars=/path/to/vars.fd \
    -s 2,nvme,disk.img -s 3,xhci,tablet,kbd -s 5,virtio-rnd -s 7,fbuf,rfb=127.0.0.1:5900 ...
```

- `bootvars`：UEFI 变量（启动项等）存在这个文件里，没有就新建；不给这个选项则变量只在内存里。
- `fbuf`：固件用 ramfb 给出 GOP 的线性帧缓冲，VNC 看到的就是它；`virtio-rnd` 给固件提供随机数（EFI_RNG_PROTOCOL）。
