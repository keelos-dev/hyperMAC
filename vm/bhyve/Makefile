# bhyve for macOS on Hypervisor.framework. Run through build.sh (it makes build/src from freebsd/ + patches/).
# GNU make 3.81 (what macOS ships). macOS 26 at least: the 4K stage-2 granule (hvf/) came with it.

S	:= build/src
B	:= $(S)/usr.sbin/bhyve
O	:= build/obj

# bhyve itself. What is left out, and the step of docs/bhyve-plan.md that brings it in:
#   net_backend_slirp.c     needs libslirp and a helper program; the macOS backends are in hvf/
#   pci_virtio_9p.c         needs lib9p
#   pci_virtio_scsi*.c      needs the kernel's CTL
#   pci_virtio_input.c      passes a host evdev device through: FreeBSD only
#   pci_passthru*.c net_backend_netmap.c net_backend_netgraph.c pctestdev.c snapshot.c   not for macOS/arm64
BHYVE_SRCS := \
	acpi.c acpi_device.c audio.c basl.c bhyvegc.c bhyverun.c block_if.c bootrom.c \
	config.c config_schema.c console.c gdb.c hda_codec.c iov.c ipc.c mem.c mevent.c \
	net_backends.c net_utils.c \
	pci_ahci.c pci_e82545.c pci_emul.c pci_fbuf.c pci_hda.c pci_hostbridge.c pci_nvme.c pci_uart.c \
	pci_virtio_block.c pci_virtio_console.c pci_virtio_net.c pci_virtio_rnd.c pci_xhci.c \
	qemu_fwcfg.c qemu_loader.c rfb.c smbiostbl.c sockstream.c \
	tpm_device.c tpm_emul_passthru.c tpm_emul_swtpm.c tpm_intf_crb.c tpm_ppi_qemu.c \
	rtc_pl031.c uart_backend.c uart_emul.c uart_pl011.c usb_emul.c usb_mouse.c virtio.c vmgenc.c
# keelOS's (build.sh puts them in build/src)
BHYVE_SRCS += usb_kbd.c usb_remote.c pci_virtio_gpu.c
BHYVE_MD_SRCS := bhyverun_machdep.c fdt.c mem_aarch64.c pci_irq.c vmexit.c

SRCS := $(addprefix $(B)/,$(BHYVE_SRCS)) $(addprefix $(B)/aarch64/,$(BHYVE_MD_SRCS)) \
	$(S)/sys/arm64/vmm/vmm_instruction_emul.c \
	$(S)/sys/libkern/crc16.c \
	$(addprefix $(S)/sys/contrib/libfdt/,fdt.c fdt_ro.c fdt_rw.c fdt_sw.c fdt_strerror.c fdt_wip.c fdt_addresses.c) \
	$(addprefix $(S)/sys/contrib/libnv/,nvlist.c bsd_nvpair.c cnvlist.c dnvlist.c) \
	$(S)/lib/libnv/msgio.c \
	$(addprefix $(S)/lib/libutil/,expand_number.c hexdump.c cpuset.c) \
	$(wildcard compat/src/*.c) $(wildcard hvf/*.c) $(filter-out src/tpm_emul_libtpms.c,$(wildcard src/*.c))

# The TPM of a Windows guest inside this process: libtpms (BSD-3-Clause) and the libcrypto it
# needs (OpenSSL, Apache-2.0), both linked statically so that bhyve stays one file with nothing to
# install beside it. Homebrew's by default (brew install libtpms); without them bhyve builds
# without the "libtpms" TPM (tpm.type=swtpm, an swtpm process beside it, still works).
LIBTPMS ?= $(wildcard /opt/homebrew/opt/libtpms)
LIBCRYPTO ?= $(firstword $(wildcard /opt/homebrew/opt/openssl@4/lib/libcrypto.a /opt/homebrew/opt/openssl@3/lib/libcrypto.a))
ifneq ($(and $(wildcard $(LIBTPMS)/lib/libtpms.a),$(LIBCRYPTO)),)
SRCS += src/tpm_emul_libtpms.c
TPM_INC := -I$(LIBTPMS)/include
TPM_LIBS := $(LIBTPMS)/lib/libtpms.a $(LIBCRYPTO)
endif

OBJS := $(patsubst %.c,$(O)/%.o,$(SRCS))

INC := -include compat/include/bhyve_macos.h -Icompat/include -Ibuild/include \
	-I$(S)/sys -I$(S)/sys/contrib/libfdt -I$(S)/sys/contrib/libnv -I$(S)/lib/libnv \
	-I$(S)/sys/dev/e1000 -I$(S)/sys/dev/mii -I$(S)/sys/dev/usb/controller \
	-I$(S)/lib/libvmmapi -I$(B) -I$(B)/aarch64 -Ihvf -Isrc $(TPM_INC)
DEFS := -DBHYVE_GDB -DINET -DINET6 -DNO_OPENSSL
# FreeBSD builds this code with its own warning set; here: errors that matter on, the style ones off.
# -Wno-format: uint64_t is unsigned long on FreeBSD and unsigned long long here (both 64 bits); the
# code prints it with %lx.
WARN := -Wall -Wno-format -Wno-unused-parameter -Wno-unused-function -Wno-unused-variable -Wno-deprecated-declarations \
	-Wno-unused-but-set-variable -Wno-address-of-packed-member -Wno-pointer-sign
CFLAGS := -O2 -g -std=gnu17 -arch arm64 -mmacosx-version-min=26.0 $(WARN) $(DEFS) $(INC)
LDFLAGS := -arch arm64 -framework Hypervisor -framework vmnet -lz -lpthread

all: build/bhyve

# Linked and signed under another name and then renamed: a VM that is running from build/bhyve keeps
# its file (rewriting a signed binary in place kills the processes that run it).
build/bhyve: $(OBJS) hvf/bhyve.entitlements
	$(CC) -o $@.new $(OBJS) $(TPM_LIBS) $(LDFLAGS)
	codesign --force -s - --entitlements hvf/bhyve.entitlements $@.new
	mv -f $@.new $@

$(O)/%.o: %.c
	@mkdir -p $(dir $@)
	$(CC) $(CFLAGS) -MMD -MP -c -o $@ $<

clean:
	rm -rf build/obj build/bhyve

-include $(OBJS:.o=.d)
.PHONY: all clean
