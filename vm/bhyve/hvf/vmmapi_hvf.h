/*
 * What bhyve on Hypervisor.framework has beyond FreeBSD's libvmmapi (vmmapi.h): used from the
 * macOS patches of bhyve's sources (patches/macos).
 */
#ifndef _VMMAPI_HVF_H_
#define	_VMMAPI_HVF_H_

#include <sys/types.h>
#include <stdint.h>

struct vmctx;

__BEGIN_DECLS
/*
 * Hypervisor.framework's GIC takes MSIs through a GICv2m frame: where it is in the guest and which
 * SPIs it hands out. To be called before vm_attach_vgic(); vm_raise_msi() then accepts the frame's
 * MSI_SETSPI_NS register (base + 0x40) as the address.
 */
int	vm_hvf_set_msi_frame(struct vmctx *ctx, uint64_t base, size_t size, uint32_t spi_base,
	    uint32_t spi_count);
__END_DECLS

#endif /* _VMMAPI_HVF_H_ */
