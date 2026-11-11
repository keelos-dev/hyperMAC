# bhyve on Hypervisor.framework

FreeBSD 的 bhyve（head，提交号在 `UPSTREAM`）跑在 macOS 的 Hypervisor.framework 上，给 hyperMAC 跑 Windows 11 ARM 用。
计划、每一步的验收和实测记录在 [docs/bhyve-plan.md](../../docs/bhyve-plan.md)。

要求：Apple Silicon，macOS 26 以上（4K 粒度的 stage-2 映射和不用 root 的 vmnet 都是 26 才有的），Xcode 命令行工具。

平时不直接用这里的命令行：`hypermac create … --os windows` 和 `hypermac run`（`vm/src/bhyve.rs`）会起它，仓库根目录的 `build.sh` 会编它、
签名并装到 `hypermac-vm` 旁边。下面是单独构建、调试和测试用的。

## 目录

| | |
|---|---|
| `freebsd/` | 上游文件原样，**不手改**。`MANIFEST` 是清单，`./import.sh SRCTOP` 从 FreeBSD 的源码树重新取 |
| `patches/upstream/` | 对上游文件的改动里打算提给 FreeBSD 的（每个补丁带提交说明） |
| `patches/macos/` | 只为 macOS 的改动 |
（另有 `patches/keel/`：我们对 keelOS 生成器输出的改动，`build.sh` 在生成器之后打，`mkpatch.sh keel 名字 文件` 生成。）
| `compat/` | macOS 缺的头文件和函数，让上游文件不改就能编 |
| `hvf/` | 我们自己的、只为 macOS 的文件：`vmmapi_hvf.c`（libvmmapi 实现在 Hypervisor.framework 上）、`net_backend_macos.c`（vmnet 网络后端） |
| `src/` | 我们自己的、和 macOS 无关的文件（可以提上游）：`pflash.c`（存 UEFI 变量的 NOR 闪存）、`acpi_arm64.c` 和 DSDT（arm64 的 ACPI 表） |
| `patches/REVIEW.md` | 提给 FreeBSD 之前的清单：补丁怎么分组、依赖、还缺什么 |
| `firmware/` | UEFI 固件的二进制和构建方法 |
| `test/` | 跑 guest 并按脚本操作串口的工具 |
| `build/` | 构建产物（不进仓库）：`build/src` = `freebsd/` 打完补丁，`build/bhyve` = 成品 |

## 构建

```sh
./build.sh          # -> build/bhyve，ad-hoc 签名，带 hypervisor 和 virtualization 两个 entitlement
```

不带 entitlement 的二进制起不了虚拟机（`vm_openf: Operation not permitted`），所以别直接用 `make` 以外的方式链接后运行。

## 运行

固件是 `firmware/QEMU_EFI.fd`（EDK2 的 UEFI，来历见 `firmware/README.md`）。FreeBSD 包 `u-boot-bhyve-arm64` 里的 `u-boot.bin` 也能用。

```sh
build/bhyve -c 4 -m 4G \
    -o bootrom=firmware/QEMU_EFI.fd -o bootvars=vars.fd \
    -o console=stdio \
    -o rundir=$HOME/Library/Caches/bhyve \
    -s 0,hostbridge \
    -s 1,nvme,disk.img \
    -s 2,virtio-net,nat \
    -s 3,xhci,tablet,kbd -s 5,virtio-rnd \
    -s 7,fbuf,rfb=127.0.0.1:5900 \
    vm0
```

- `-o rundir=`：必须给一个自己能写的目录，默认的 `/var/run/bhyve` 普通用户写不了。
- `-o console=`：`stdio`，或 `tcp=127.0.0.1:PORT`（用 `nc` 连）。
- 退出码和 FreeBSD 上一样：0 = guest 重启（再起一次同样的命令就是重启），1 = 关机，2 = halt，4 = bhyve 出错。
- `-o bootvars=`：UEFI 变量文件，没有就新建。
- 画面：VNC 连 `fbuf` 的端口；键盘鼠标是 `xhci,tablet,kbd`。
- 用 U-Boot 时只从第一块 virtio 盘启动；UEFI 没有这个限制（NVMe、AHCI、virtio 都认）。
- `HVF_WATCH=2`（环境变量）：每 2 秒打印各 vCPU 的位置，固件或系统不出声时用。

### 和 FreeBSD 上的 bhyve 不一样的地方

| | |
|---|---|
| 网络 | `virtio-net,nat`（vmnet 共享模式，macOS 给地址）、`virtio-net,bridged[,if=en0]`（桥接到宿主网卡）、`virtio-net,dgram,fd=N`（父进程传下来的数据报 socket，一帧一个数据报）。没有 tap |
| MSI | 走 GICv2m 帧（设备树里的 `arm,gic-v2m-frame`），不是 FreeBSD 的 MBI |
| 磁盘 | 镜像文件或 `/dev/rdiskN`；`nocache=on` 对应 `F_NOCACHE`；TRIM 在镜像文件上是 `F_PUNCHHOLE` |
| `-p` 绑核、`-S` 锁内存 | macOS 上做不到：`-p` 警告后照常运行 |
| 没有的设备 | virtio-9p、virtio-scsi、virtio-input（依赖 FreeBSD 内核的东西）、直通 |
| virtio | 设备同时有 legacy 和 virtio 1.0 两套接口（`virtio.modern=false` 关掉后者）；ARM64 的 Windows 驱动只能用后者 |
| 屏幕 | `fbuf,w=,h=`（最大 1920x1200）经 fw_cfg 告诉固件，UEFI 和没有显卡驱动的系统就用这个大小 |
| 电源键 | `kill -TERM`（ACPI 的 GED 电源键）；强杀用 `kill -9` |
| PMU | guest 能看到 PMUv3（框架在内核里模拟） |

## 测试

```sh
export UBOOT=$PWD/firmware/QEMU_EFI.fd       # 或 u-boot.bin；run.sh 把它当 bootrom

# 启动一个镜像，按步骤操作串口（console.py 的语法见它的文件头）
test/run.sh disk.img 4 -- '90:login:=>root' '30:# =>uname -a; poweroff' 'sleep:10'

# 额外的设备经 BHYVE_ARGS 给
BHYVE_ARGS="-s 2,nvme,nvme.img -s 4,xhci,tablet -s 7,fbuf,rfb=127.0.0.1:5999" test/run.sh disk.img 4 -- ...

# 不用真网络测网卡：netpeer.py 在 socket 另一头扮演 10.0.0.1
python3 test/netpeer.py sh -c 'BHYVE_ARGS="-s 2,virtio-net,dgram,fd=$NETFD" test/run.sh disk.img 4 -- ...'

# VNC（fbuf）：读像素、发指针事件
python3 test/vnc.py 5999 info 'pixels 0,0,8' move:100

# Debian 从 netinst ISO 无人值守装到硬盘再从硬盘启动（10–20 分钟）
test/debian-install.sh debian-13.x-arm64-netinst.iso target.img

# Windows 11 ARM 从微软的 ISO 无人值守装到桌面（约 10 分钟，账号 win / bhyve1），再带网卡启动
test/win11-install.sh Win11_arm64.iso DIR
FBW=1920 FBH=1080 CD=virtio-win.iso test/win11-run.sh DIR -s 8,virtio-net,nat      # VNC 在 127.0.0.1:5910
```

测试用的镜像不在仓库里：FreeBSD 的 `VM-IMAGES/*/aarch64/Latest/*-arm64-aarch64-ufs.raw.xz`，Debian 的 `cloud/trixie/latest/debian-13-nocloud-arm64.tar.xz`
（首次启动有向导，要在串口上设 root 密码），U-Boot 从 `pkg.freebsd.org/FreeBSD:15:aarch64/latest` 的 `u-boot-bhyve-arm64` 包里解。

## 改上游的文件

不要改 `freebsd/`。流程：

```sh
./build.sh                                   # build/src = freebsd/ + 现有补丁
$EDITOR build/src/usr.sbin/bhyve/xxx.c       # 改
./mkpatch.sh upstream 0006-name usr.sbin/bhyve/xxx.c    # 或 macos；生成 patches/…/0006-name.patch
```

补丁文件开头（第一个 `--- a/` 之前）写提交说明，`mkpatch.sh` 重新生成时会保留。能放进 `compat/` 解决的（缺头文件、缺函数）就不要动上游文件。

升级上游：把新的提交号写进 `UPSTREAM`，在 FreeBSD 的树里检出它，`./import.sh SRCTOP`，然后 `./build.sh` 看哪些补丁打不上。
