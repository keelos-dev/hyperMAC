/*
 * ACPI tables for bhyve's arm64 machine (ACPI 6.3, hardware-reduced): what Windows needs to boot
 * at all, and what Linux and FreeBSD use when the firmware gives them ACPI instead of the device
 * tree. They go to the firmware as fw_cfg files with QEMU's table-loader script (basl.c), which
 * EDK2's AcpiPlatformDxe installs.
 *
 *	XSDT	(no RSDT: the guest has no memory below 4 GB, and a table with a 32-bit pointer to
 *		it would have to be allocated there)
 *	FADT	hardware-reduced, PSCI through HVC
 *	MADT	a GICC per CPU, the GICv3 distributor and redistributors, the GICv2m MSI frame
 *	GTDT	the generic timer's interrupts
 *	MCFG	the PCI configuration space
 *	SPCR, DBG2	the PL011 as console and debug port (Windows insists on DBG2)
 *	DSDT	compiled ahead of time from acpi_arm64_dsdt.asl: UART, fw_cfg, power button, PCI
 *	SSDT	the processors, generated here
 *
 * amd64's acpi.c writes ASL and runs iasl on it when bhyve starts. Not here: nothing in the DSDT
 * depends on the configuration, and bhyve should not need a compiler installed to start a VM.
 */
#include <sys/param.h>
#include <sys/endian.h>

#include <assert.h>
#include <err.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

#include <vmmapi.h>

#include "acpi.h"
#include "acpi_arm64.h"
#include "basl.h"
#include "config.h"
#include "debug.h"
#include "mevent.h"

#include "acpi_arm64_dsdt.hex"

/* MADT structures as of ACPI 6.3: ACPICA's may be those of a later revision, with more fields. */
#define	MADT_REVISION		5
#define	MADT_GICC_LENGTH	80

#define	PPI_PMU			23	/* INTIDs of the per-CPU interrupts */
#define	PPI_HYP_TIMER		26
#define	PPI_VIRT_TIMER		27
#define	PPI_SEC_PHYS_TIMER	29
#define	PPI_PHYS_TIMER		30

static int
build_rsdp(struct vmctx *const ctx)
{
	ACPI_TABLE_RSDP rsdp;
	struct basl_table *table;

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_RSDP_NAME, BASL_TABLE_ALIGNMENT));

	memset(&rsdp, 0, sizeof(rsdp));
	memcpy(rsdp.Signature, ACPI_SIG_RSDP, 8);
	memcpy(rsdp.OemId, "BHYVE ", ACPI_OEM_ID_SIZE);
	rsdp.Revision = 2;
	BASL_EXEC(basl_table_append_bytes(table, &rsdp, sizeof(rsdp)));

	BASL_EXEC(basl_table_add_checksum(table, offsetof(ACPI_TABLE_RSDP, Checksum), 0, 20));
	BASL_EXEC(basl_table_add_length(table, offsetof(ACPI_TABLE_RSDP, Length), sizeof(rsdp.Length)));
	BASL_EXEC(basl_table_add_pointer(table, ACPI_SIG_XSDT,
	    offsetof(ACPI_TABLE_RSDP, XsdtPhysicalAddress), sizeof(rsdp.XsdtPhysicalAddress)));
	BASL_EXEC(basl_table_add_checksum(table, offsetof(ACPI_TABLE_RSDP, ExtendedChecksum), 0,
	    BASL_TABLE_CHECKSUM_LEN_FULL_TABLE));
	return (0);
}

static int
build_fadt(struct vmctx *const ctx)
{
	ACPI_TABLE_FADT fadt;
	struct basl_table *table;

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_FADT, BASL_TABLE_ALIGNMENT));

	memset(&fadt, 0, sizeof(fadt));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_FADT, 6, 1));
	fadt.MinorRevision = 3;
	fadt.PreferredProfile = PM_ENTERPRISE_SERVER;
	fadt.Flags = htole32(ACPI_FADT_HW_REDUCED);
	fadt.ArmBootFlags = htole16(ACPI_FADT_PSCI_COMPLIANT | ACPI_FADT_PSCI_USE_HVC);
	memcpy(&fadt.HypervisorId, "BHYVE   ", sizeof(fadt.HypervisorId));
	BASL_EXEC(basl_table_append_content(table, &fadt, sizeof(fadt)));

	BASL_EXEC(basl_table_add_pointer(table, ACPI_SIG_DSDT, offsetof(ACPI_TABLE_FADT, XDsdt),
	    sizeof(fadt.XDsdt)));

	/* the first table of the XSDT, as UEFI wants it */
	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

static int
build_madt(struct vmctx *const ctx, const struct acpi_arm64_info *info)
{
	ACPI_TABLE_MADT madt;
	ACPI_MADT_GENERIC_INTERRUPT gicc;
	ACPI_MADT_GENERIC_DISTRIBUTOR gicd;
	ACPI_MADT_GENERIC_REDISTRIBUTOR gicr;
	ACPI_MADT_GENERIC_MSI_FRAME msi;
	struct basl_table *table;
	int i;

	_Static_assert(sizeof(gicc) >= MADT_GICC_LENGTH, "ACPICA's GICC is older than ACPI 6.3's");

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_MADT, BASL_TABLE_ALIGNMENT));

	memset(&madt, 0, sizeof(madt));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_MADT, MADT_REVISION, 1));
	BASL_EXEC(basl_table_append_content(table, &madt, sizeof(madt)));

	for (i = 0; i < info->ncpu; i++) {
		memset(&gicc, 0, sizeof(gicc));
		gicc.Header.Type = ACPI_MADT_TYPE_GENERIC_INTERRUPT;
		gicc.Header.Length = MADT_GICC_LENGTH;
		gicc.CpuInterfaceNumber = htole32(i);
		gicc.Uid = htole32(i);		/* the _UID of the processor's device in the SSDT */
		gicc.Flags = htole32(ACPI_MADT_ENABLED);
		gicc.PerformanceInterrupt = htole32(PPI_PMU);
		gicc.ArmMpidr = htole64(info->mpidr[i]);
		BASL_EXEC(basl_table_append_bytes(table, &gicc, MADT_GICC_LENGTH));
	}

	memset(&gicd, 0, sizeof(gicd));
	gicd.Header.Type = ACPI_MADT_TYPE_GENERIC_DISTRIBUTOR;
	gicd.Header.Length = sizeof(gicd);
	gicd.BaseAddress = htole64(info->gicd_base);
	gicd.Version = 3;
	BASL_EXEC(basl_table_append_bytes(table, &gicd, sizeof(gicd)));

	memset(&gicr, 0, sizeof(gicr));
	gicr.Header.Type = ACPI_MADT_TYPE_GENERIC_REDISTRIBUTOR;
	gicr.Header.Length = sizeof(gicr);
	gicr.BaseAddress = htole64(info->gicr_base);
	gicr.Length = htole32(info->gicr_size);
	BASL_EXEC(basl_table_append_bytes(table, &gicr, sizeof(gicr)));

	if (info->gic_msi_base != 0) {
		memset(&msi, 0, sizeof(msi));
		msi.Header.Type = ACPI_MADT_TYPE_GENERIC_MSI_FRAME;
		msi.Header.Length = sizeof(msi);
		msi.BaseAddress = htole64(info->gic_msi_base);
		/* the SPIs are the ones given here, not what the frame's MSI_TYPER says */
		msi.Flags = htole32(ACPI_MADT_OVERRIDE_SPI_VALUES);
		msi.SpiCount = htole16(info->gic_msi_spi_count);
		msi.SpiBase = htole16(info->gic_msi_spi_base);
		BASL_EXEC(basl_table_append_bytes(table, &msi, sizeof(msi)));
	}

	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

static int
build_gtdt(struct vmctx *const ctx)
{
	ACPI_TABLE_GTDT gtdt;
	struct basl_table *table;

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_GTDT, BASL_TABLE_ALIGNMENT));

	memset(&gtdt, 0, sizeof(gtdt));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_GTDT, 2, 1));
	/* the timers are the CPU's own (system registers): no memory-mapped counter block */
	gtdt.CounterBlockAddresss = htole64(0xffffffffffffffffUL);
	gtdt.CounterReadBlockAddress = htole64(0xffffffffffffffffUL);
	/* level-triggered, active high, and they keep running while the CPU idles */
	gtdt.SecureEl1Interrupt = htole32(PPI_SEC_PHYS_TIMER);
	gtdt.SecureEl1Flags = htole32(ACPI_GTDT_ALWAYS_ON);
	gtdt.NonSecureEl1Interrupt = htole32(PPI_PHYS_TIMER);
	gtdt.NonSecureEl1Flags = htole32(ACPI_GTDT_ALWAYS_ON);
	gtdt.VirtualTimerInterrupt = htole32(PPI_VIRT_TIMER);
	gtdt.VirtualTimerFlags = htole32(ACPI_GTDT_ALWAYS_ON);
	gtdt.NonSecureEl2Interrupt = htole32(PPI_HYP_TIMER);
	gtdt.NonSecureEl2Flags = htole32(ACPI_GTDT_ALWAYS_ON);
	BASL_EXEC(basl_table_append_content(table, &gtdt, sizeof(gtdt)));

	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

static int
build_mcfg(struct vmctx *const ctx, const struct acpi_arm64_info *info)
{
	ACPI_TABLE_MCFG mcfg;
	ACPI_MCFG_ALLOCATION alloc;
	struct basl_table *table;

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_MCFG, BASL_TABLE_ALIGNMENT));

	memset(&mcfg, 0, sizeof(mcfg));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_MCFG, 1, 1));
	BASL_EXEC(basl_table_append_content(table, &mcfg, sizeof(mcfg)));

	memset(&alloc, 0, sizeof(alloc));
	alloc.Address = htole64(info->pci_ecam_base);
	alloc.EndBusNumber = 0xff;
	BASL_EXEC(basl_table_append_bytes(table, &alloc, sizeof(alloc)));

	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

static int
build_spcr(struct vmctx *const ctx, const struct acpi_arm64_info *info)
{
	ACPI_TABLE_SPCR spcr;
	struct basl_table *table;

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_SPCR, BASL_TABLE_ALIGNMENT));

	memset(&spcr, 0, sizeof(spcr));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_SPCR, 2, 1));
	spcr.InterfaceType = ACPI_DBG2_ARM_PL011;
	basl_fill_gas(&spcr.SerialPort, ACPI_ADR_SPACE_SYSTEM_MEMORY, 32, 0, ACPI_GAS_ACCESS_WIDTH_DWORD,
	    info->uart_base);
	spcr.InterruptType = 0x08;	/* ARM GIC */
	spcr.Interrupt = htole32(info->uart_irq);
	spcr.BaudRate = ACPI_SPCR_BAUD_RATE_115200;
	spcr.Parity = ACPI_SPCR_PARITY_NO_PARITY;
	spcr.StopBits = ACPI_SPCR_STOP_BITS_1;
	spcr.TerminalType = ACPI_SPCR_TERMINAL_TYPE_VT_UTF8;
	spcr.PciDeviceId = htole16(0xffff);
	spcr.PciVendorId = htole16(0xffff);
	BASL_EXEC(basl_table_append_content(table, &spcr, sizeof(spcr)));

	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

/* The debug port: the same UART. Windows does not boot on arm64 without this table. */
static int
build_dbg2(struct vmctx *const ctx, const struct acpi_arm64_info *info)
{
	static const char namepath[] = "\\_SB.COM0";
	ACPI_TABLE_DBG2 dbg2;
	ACPI_DBG2_DEVICE dev;
	ACPI_GENERIC_ADDRESS gas;
	struct basl_table *table;
	uint32_t addrsize;

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_DBG2, BASL_TABLE_ALIGNMENT));

	memset(&dbg2, 0, sizeof(dbg2));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_DBG2, 0, 1));
	dbg2.InfoOffset = htole32(sizeof(dbg2));
	dbg2.InfoCount = htole32(1);
	BASL_EXEC(basl_table_append_content(table, &dbg2, sizeof(dbg2)));

	/* one device: this structure, its register (a GAS), the register's size, its name */
	memset(&dev, 0, sizeof(dev));
	dev.Revision = 0;
	dev.Length = htole16(sizeof(dev) + sizeof(gas) + sizeof(addrsize) + sizeof(namepath));
	dev.RegisterCount = 1;
	dev.NamepathLength = htole16(sizeof(namepath));
	dev.NamepathOffset = htole16(sizeof(dev) + sizeof(gas) + sizeof(addrsize));
	dev.PortType = htole16(ACPI_DBG2_SERIAL_PORT);
	dev.PortSubtype = htole16(ACPI_DBG2_ARM_PL011);
	dev.BaseAddressOffset = htole16(sizeof(dev));
	dev.AddressSizeOffset = htole16(sizeof(dev) + sizeof(gas));
	BASL_EXEC(basl_table_append_bytes(table, &dev, sizeof(dev)));

	basl_fill_gas(&gas, ACPI_ADR_SPACE_SYSTEM_MEMORY, 32, 0, ACPI_GAS_ACCESS_WIDTH_DWORD,
	    info->uart_base);
	BASL_EXEC(basl_table_append_bytes(table, &gas, sizeof(gas)));
	addrsize = htole32(0x1000);
	BASL_EXEC(basl_table_append_bytes(table, &addrsize, sizeof(addrsize)));
	BASL_EXEC(basl_table_append_bytes(table, namepath, sizeof(namepath)));

	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

static int
build_dsdt(struct vmctx *const ctx)
{
	struct basl_table *table;

	/* complete as iasl made it, header and checksum included; the FADT points to it */
	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_DSDT, BASL_TABLE_ALIGNMENT));
	BASL_EXEC(basl_table_append_bytes(table, acpi_arm64_dsdt_aml_code,
	    sizeof(acpi_arm64_dsdt_aml_code)));
	return (0);
}

/* AML's PkgLength: the length of a package, the length field itself included. */
static size_t
aml_pkglength(uint8_t *p, size_t content)
{
	size_t len;

	if (content + 1 <= 0x3f) {
		p[0] = content + 1;
		return (1);
	}
	len = content + 2;
	assert(len <= 0xfff);
	p[0] = 0x40 | (len & 0xf);
	p[1] = len >> 4;
	return (2);
}

/*
 * A TPM (the "tpm" configuration node; tpm_device.c registered it as an ACPI device, and its
 * TPM2 table comes from acpi_tables_build_devices()), in \_SB beside the processors:
 *	Device (TPM0) {
 *		Name (_HID, "MSFT0101")
 *		Name (_STA, 0x0F)
 *		Name (_CRS, ResourceTemplate () { Memory32Fixed (ReadWrite, 0xFED40000, 0x5000) })
 *	}
 * amd64 writes this (and the physical presence interface's _DSM) as ASL at run time; here it
 * is AML by hand, without the _DSM. It shares the processors' SSDT: the firmware's table loader
 * takes one fw_cfg file per table signature ("etc/acpi/SSDT"), a second SSDT is refused and
 * with it every table.
 */
#define	TPM_MMIO_BASE	0xFED40000u	/* tpm_intf_crb.c: TPM_CRB_ADDRESS, five localities */
#define	TPM_MMIO_SIZE	0x5000u

static const uint8_t tpm_device_aml[] = {
	0x5b, 0x82, 0x32, 'T', 'P', 'M', '0',		/* Device (TPM0), 0x32 bytes */
	0x08, '_', 'H', 'I', 'D', 0x0d, 'M', 'S', 'F', 'T', '0', '1', '0', '1', 0,
	0x08, '_', 'S', 'T', 'A', 0x0a, 0x0f,
	0x08, '_', 'C', 'R', 'S', 0x11, 0x11, 0x0a, 0x0e,	/* Buffer (14) */
	0x86, 0x09, 0x00, 0x01,				/* Memory32Fixed, read/write */
	TPM_MMIO_BASE & 0xff, (TPM_MMIO_BASE >> 8) & 0xff, (TPM_MMIO_BASE >> 16) & 0xff, TPM_MMIO_BASE >> 24,
	TPM_MMIO_SIZE & 0xff, (TPM_MMIO_SIZE >> 8) & 0xff, (TPM_MMIO_SIZE >> 16) & 0xff, TPM_MMIO_SIZE >> 24,
	0x79, 0x00,					/* EndTag */
};
_Static_assert(sizeof(tpm_device_aml) == 2 + 0x32, "Device's PkgLength");

/*
 * The processors:
 *	Scope (\_SB) { Device (Cnnn) { Name (_HID, "ACPI0007") Name (_UID, n) } ... }
 * in AML by hand; _UID is the GICC's ACPI processor UID in the MADT.
 */
static int
build_ssdt_cpus(struct vmctx *const ctx, const struct acpi_arm64_info *info)
{
	static const uint8_t hid[] = { 0x08, '_', 'H', 'I', 'D', 0x0d, 'A', 'C', 'P', 'I', '0', '0', '0', '7', 0 };
	struct basl_table *table;
	uint8_t devs[64 * 32 + sizeof(tpm_device_aml)], dev[32], pkg[2];
	uint8_t scope_name[] = { 0x5c, '_', 'S', 'B', '_' };
	char name[5];
	size_t ndevs = 0, n, pl;
	int i;

	assert(info->ncpu <= 64);
	for (i = 0; i < info->ncpu; i++) {
		/* the device's contents: its name and the two objects */
		n = 0;
		snprintf(name, sizeof(name), "C%03X", i);
		memcpy(dev + n, name, 4);
		n += 4;
		memcpy(dev + n, hid, sizeof(hid));
		n += sizeof(hid);
		dev[n++] = 0x08;
		memcpy(dev + n, "_UID", 4);
		n += 4;
		if (i <= 1)
			dev[n++] = i;		/* ZeroOp, OneOp */
		else {
			dev[n++] = 0x0a;	/* BytePrefix */
			dev[n++] = i;
		}
		devs[ndevs++] = 0x5b;		/* DeviceOp */
		devs[ndevs++] = 0x82;
		ndevs += aml_pkglength(devs + ndevs, n);
		memcpy(devs + ndevs, dev, n);
		ndevs += n;
	}

	if (find_config_node("tpm") != NULL) {
		memcpy(devs + ndevs, tpm_device_aml, sizeof(tpm_device_aml));
		ndevs += sizeof(tpm_device_aml);
	}

	BASL_EXEC(basl_table_create(&table, ctx, ACPI_SIG_SSDT, BASL_TABLE_ALIGNMENT));
	BASL_EXEC(basl_table_append_header(table, ACPI_SIG_SSDT, 2, 1));
	pkg[0] = 0x10;				/* ScopeOp */
	BASL_EXEC(basl_table_append_bytes(table, pkg, 1));
	pl = aml_pkglength(pkg, sizeof(scope_name) + ndevs);
	BASL_EXEC(basl_table_append_bytes(table, pkg, pl));
	BASL_EXEC(basl_table_append_bytes(table, scope_name, sizeof(scope_name)));
	BASL_EXEC(basl_table_append_bytes(table, devs, ndevs));

	BASL_EXEC(basl_table_register_to_rsdt(table));
	return (0);
}

/*
 * SIGTERM is the power button, as on amd64: an edge on the generic event device's interrupt,
 * whose _EVT method notifies the button. A guest without ACPI support does not notice.
 */
struct power_button {
	struct vmctx *ctx;
	uint32_t irq;
};

static void
power_button_handler(int signal __unused, enum ev_type type __unused, void *arg)
{
	struct power_button *pb = arg;

	(void)vm_assert_irq(pb->ctx, pb->irq);
	(void)vm_deassert_irq(pb->ctx, pb->irq);
}

int
acpi_arm64_build(struct vmctx *ctx, const struct acpi_arm64_info *info)
{
	static struct power_button pb;

	BASL_EXEC(basl_init(ctx));

	BASL_EXEC(build_rsdp(ctx));
	BASL_EXEC(build_fadt(ctx));
	BASL_EXEC(build_madt(ctx, info));
	BASL_EXEC(build_gtdt(ctx));
	BASL_EXEC(build_mcfg(ctx, info));
	BASL_EXEC(build_spcr(ctx, info));
	BASL_EXEC(build_dbg2(ctx, info));
	BASL_EXEC(build_ssdt_cpus(ctx, info));
	BASL_EXEC(acpi_tables_build_devices());
	BASL_EXEC(build_dsdt(ctx));

	BASL_EXEC(basl_finish());

	pb.ctx = ctx;
	pb.irq = info->power_button_irq;
	signal(SIGTERM, SIG_IGN);
	if (mevent_add(SIGTERM, EVF_SIGNAL, power_button_handler, &pb) == NULL)
		return (errno);
	return (0);
}
