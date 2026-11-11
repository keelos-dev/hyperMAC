/*
 * A NOR flash chip with the Intel command set (CFI 01), as QEMU's pflash_cfi01: what EDK2's
 * VirtNorFlashDxe keeps the UEFI variables in.
 */
#ifndef _PFLASH_H_
#define	_PFLASH_H_

#include <stdint.h>

/* As on QEMU's "virt" machine: 32-bit bank of two 16-bit chips, 256K blocks. */
#define	PFLASH_BLOCK_SIZE	(256 * 1024)

/*
 * A flash of `size` bytes at `base`, kept in the file `path` (made if it is not there; NULL: only
 * in memory, lost when bhyve ends). An erased flash reads as 0xff. Returns 0 or an errno.
 */
int	pflash_init(uint64_t base, uint64_t size, const char *path);

#endif /* _PFLASH_H_ */
