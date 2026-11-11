/*
 * A NOR flash chip with the Intel command set (CFI 01), as QEMU's pflash_cfi01: what EDK2's
 * VirtNorFlashDxe keeps the UEFI variables in (and what the stock ArmVirt firmware insists on:
 * its variable driver does not start before that flash is formatted).
 *
 * Every access is emulated, reads of the array too: the firmware reads the variable store with
 * word accesses the CPU decodes (it is built not to use unaligned or paired loads on flash), and
 * the store is small. The commands, as the driver uses them on a 32-bit bank:
 *
 *	0xff			read array
 *	0x70 / 0x50		read / clear status
 *	0x90, 0x98		read ID, CFI query
 *	0x40 or 0x10, DATA	program one word
 *	0xe8, N-1, N x DATA, 0xd0	program N words (buffered)
 *	0x20 or 0x28, 0xd0	erase the block
 *	0x60, 0xd0 or 0x01	unlock / lock the block (accepted, nothing is locked)
 *
 * After a command other than "read array" reads return the status register until the driver
 * goes back to the array.
 */
#include <sys/types.h>
#include <sys/mman.h>
#include <sys/stat.h>

#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <machine/vmm.h>

#include "debug.h"
#include "mem.h"
#include "pflash.h"

#define	STATUS_READY	0x80

struct pflash {
	uint64_t base, size;
	uint8_t	*data;			/* `datasize` bytes; past it the flash reads 0xff */
	size_t	datasize;
	pthread_mutex_t mtx;
	uint8_t	cmd;			/* 0: read array */
	int	wcycle;
	uint8_t	status;
	uint32_t counter;		/* words left of a buffered program */
};

/* The same byte on both chips of the bank, in the low byte of each 16-bit lane. */
static uint64_t
replicate(uint8_t v, int size)
{
	uint64_t r = 0;
	int i;

	for (i = 0; i < size; i += 2)
		r |= (uint64_t)v << (8 * i);
	return (size == 1 ? v : r);
}

static uint64_t
array_read(struct pflash *pf, uint64_t off, int size)
{
	uint64_t v = ~0ul;

	if (off < pf->datasize)
		memcpy(&v, pf->data + off, MIN((size_t)size, pf->datasize - off));
	return (size == 8 ? v : v & ((1ul << (8 * size)) - 1));
}

static void
array_write(struct pflash *pf, uint64_t off, int size, uint64_t v)
{
	if (off < pf->datasize)
		memcpy(pf->data + off, &v, MIN((size_t)size, pf->datasize - off));
}

static void
block_erase(struct pflash *pf, uint64_t off)
{
	off = rounddown2(off, PFLASH_BLOCK_SIZE);
	if (off < pf->datasize)
		memset(pf->data + off, 0xff, MIN((size_t)PFLASH_BLOCK_SIZE, pf->datasize - off));
}

static int
pflash_handler(struct vcpu *vcpu __unused, int dir, uint64_t addr, int size, uint64_t *val,
    void *arg1, long arg2 __unused)
{
	struct pflash *pf = arg1;
	uint64_t off = addr - pf->base;
	uint8_t cmd;

	pthread_mutex_lock(&pf->mtx);
	if (dir == MEM_F_READ) {
		switch (pf->cmd) {
		case 0x00:
			*val = array_read(pf, off, size);
			break;
		case 0x90:	/* manufacturer and device ID: Intel, a generic device */
			*val = replicate((off & 0xf) == 0 ? 0x89 : (off & 0xf) == 4 ? 0x18 : 0, size);
			break;
		case 0x98:	/* CFI query: only the "QRY" signature; the driver has the geometry from the device tree */
			switch (off / 4) {
			case 0x10: *val = replicate('Q', size); break;
			case 0x11: *val = replicate('R', size); break;
			case 0x12: *val = replicate('Y', size); break;
			default: *val = 0; break;
			}
			break;
		default:
			*val = replicate(pf->status, size);
			break;
		}
		pthread_mutex_unlock(&pf->mtx);
		return (0);
	}

	cmd = *val & 0xff;
	switch (pf->wcycle) {
	case 0:
		switch (cmd) {
		case 0x00:
		case 0xf0:
		case 0xff:
			pf->cmd = 0;
			break;
		case 0x10:
		case 0x40:	/* program a word: the data comes next */
		case 0x20:
		case 0x28:	/* erase: the confirmation comes next */
		case 0x60:	/* lock or unlock: which comes next */
			pf->cmd = cmd;
			pf->wcycle = 1;
			break;
		case 0xe8:	/* buffered program: the count comes next */
			pf->cmd = cmd;
			pf->wcycle = 1;
			pf->status |= STATUS_READY;
			break;
		case 0x50:
			pf->status = STATUS_READY;
			pf->cmd = 0;
			break;
		case 0x70:
		case 0x90:
		case 0x98:
			pf->cmd = cmd;
			break;
		default:
			pf->cmd = 0;
			break;
		}
		break;
	case 1:
		switch (pf->cmd) {
		case 0x10:
		case 0x40:
			array_write(pf, off, size, *val);
			pf->status |= STATUS_READY;
			pf->wcycle = 0;
			break;
		case 0x20:
		case 0x28:
			if (cmd == 0xd0) {
				block_erase(pf, off);
				pf->status |= STATUS_READY;
			} else
				pf->cmd = 0;
			pf->wcycle = 0;
			break;
		case 0x60:
			if (cmd == 0xd0 || cmd == 0x01)
				pf->status |= STATUS_READY;
			else
				pf->cmd = 0;
			pf->wcycle = 0;
			break;
		case 0xe8:
			/* the number of words, minus one, on each chip's lane */
			pf->counter = *val & 0xffff;
			pf->wcycle = 2;
			break;
		default:
			pf->cmd = 0;
			pf->wcycle = 0;
			break;
		}
		break;
	case 2:
		array_write(pf, off, size, *val);
		if (pf->counter == 0)
			pf->wcycle = 3;
		else
			pf->counter--;
		break;
	case 3:
		/* 0xd0 confirms; anything else abandons the command */
		pf->status |= STATUS_READY;
		if (cmd != 0xd0)
			pf->cmd = 0;
		pf->wcycle = 0;
		break;
	}
	pthread_mutex_unlock(&pf->mtx);
	return (0);
}

int
pflash_init(uint64_t base, uint64_t size, const char *path)
{
	struct pflash *pf;
	struct mem_range mr;
	struct stat sb;
	int fd, error;

	pf = calloc(1, sizeof(*pf));
	if (pf == NULL)
		return (ENOMEM);
	pf->base = base;
	pf->size = size;
	pf->status = STATUS_READY;
	pthread_mutex_init(&pf->mtx, NULL);

	/* three blocks: the variable store, the fault-tolerant write area and its spare */
	pf->datasize = 3 * PFLASH_BLOCK_SIZE;
	if (path != NULL) {
		fd = open(path, O_RDWR | O_CREAT, 0600);
		if (fd < 0 || fstat(fd, &sb) != 0) {
			error = errno;
			EPRINTLN("pflash: %s: %s", path, strerror(error));
			return (error);
		}
		if ((uint64_t)sb.st_size > size) {
			EPRINTLN("pflash: %s is larger than the flash (%llu bytes)", path,
			    (unsigned long long)size);
			return (EFBIG);
		}
		if ((size_t)sb.st_size > pf->datasize)
			pf->datasize = roundup2((size_t)sb.st_size, PFLASH_BLOCK_SIZE);
		if ((size_t)sb.st_size < pf->datasize) {
			/* a new or short file: the rest is erased flash */
			char *ff = malloc(pf->datasize - sb.st_size);

			memset(ff, 0xff, pf->datasize - sb.st_size);
			if (pwrite(fd, ff, pf->datasize - sb.st_size, sb.st_size) !=
			    (ssize_t)(pf->datasize - sb.st_size)) {
				error = errno;
				EPRINTLN("pflash: %s: %s", path, strerror(error));
				return (error);
			}
			free(ff);
		}
		pf->data = mmap(NULL, pf->datasize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
		close(fd);
	} else {
		pf->data = mmap(NULL, pf->datasize, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
		if (pf->data != MAP_FAILED)
			memset(pf->data, 0xff, pf->datasize);
	}
	if (pf->data == MAP_FAILED)
		return (errno);

	bzero(&mr, sizeof(mr));
	mr.name = "pflash";
	mr.base = base;
	mr.size = size;
	mr.flags = MEM_F_RW;
	mr.handler = pflash_handler;
	mr.arg1 = pf;
	return (register_mem(&mr));
}
