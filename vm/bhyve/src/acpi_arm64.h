/*
 * ACPI tables for bhyve's arm64 machine, handed to the firmware through fw_cfg.
 */
#ifndef _ACPI_ARM64_H_
#define	_ACPI_ARM64_H_

#include <stdint.h>

struct vmctx;

/* What the tables say about the machine; the DSDT (acpi_arm64_dsdt.asl) has its share built in. */
struct acpi_arm64_info {
	int		ncpu;
	const uint64_t	*mpidr;			/* of each CPU */
	uint64_t	uart_base;
	uint32_t	uart_irq;		/* INTIDs */
	uint64_t	gicd_base;
	uint64_t	gicr_base;
	uint64_t	gicr_size;
	uint64_t	gic_msi_base;		/* the GICv2m frame; 0: none */
	uint32_t	gic_msi_spi_base;
	uint32_t	gic_msi_spi_count;
	uint64_t	pci_ecam_base;
	uint32_t	power_button_irq;	/* the GED's interrupt in the DSDT */
};

int	acpi_arm64_build(struct vmctx *ctx, const struct acpi_arm64_info *info);

#endif /* _ACPI_ARM64_H_ */
