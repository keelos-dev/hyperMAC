/*
 * FreeBSD's disk ioctls and fspacectl() on macOS (compat/include/sys/disk.h), for block_if.c.
 * A raw disk here is /dev/rdiskN; a disk image is a regular file.
 */
#include <sys/types.h>
#include <sys/disk.h>
#include <sys/stat.h>

#include <errno.h>
#include <stdarg.h>
#include <string.h>
#include <unistd.h>

#undef open
#undef ioctl
#undef fpathconf

int
bhyve_disk_open(const char *path, int flags, ...)
{
	va_list ap;
	int fd, mode, saved;

	va_start(ap, flags);
	mode = (flags & O_CREAT) != 0 ? va_arg(ap, int) : 0;
	va_end(ap);

	fd = open(path, flags & ~O_DIRECT, mode);
	if (fd >= 0 && (flags & O_DIRECT) != 0 && fcntl(fd, F_NOCACHE, 1) == -1) {
		saved = errno;
		(void)close(fd);
		errno = saved;
		return (-1);
	}
	return (fd);
}

int
bhyve_disk_ioctl(int fd, unsigned long req, ...)
{
	va_list ap;
	void *arg;
	uint64_t count;
	uint32_t bsize, features;
	dk_unmap_t unmap;
	dk_extent_t extent;
	struct diocgattr_arg *attr;
	off_t *range;

	va_start(ap, req);
	arg = va_arg(ap, void *);
	va_end(ap);

	switch (req) {
	case DIOCGSECTORSIZE:
		if (ioctl(fd, DKIOCGETBLOCKSIZE, &bsize) == -1)
			return (-1);
		*(u_int *)arg = bsize;
		return (0);
	case DIOCGMEDIASIZE:
		if (ioctl(fd, DKIOCGETBLOCKSIZE, &bsize) == -1 ||
		    ioctl(fd, DKIOCGETBLOCKCOUNT, &count) == -1)
			return (-1);
		*(off_t *)arg = (off_t)count * bsize;
		return (0);
	case DIOCGSTRIPESIZE:
		if (ioctl(fd, DKIOCGETPHYSICALBLOCKSIZE, &bsize) == -1)
			return (-1);
		*(off_t *)arg = bsize;
		return (0);
	case DIOCGSTRIPEOFFSET:
		*(off_t *)arg = 0;
		return (0);
	case DIOCGFLUSH:
		return (ioctl(fd, DKIOCSYNCHRONIZECACHE));
	case DIOCGDELETE:
		range = arg;
		memset(&unmap, 0, sizeof(unmap));
		extent.offset = range[0];
		extent.length = range[1];
		unmap.extents = &extent;
		unmap.extentsCount = 1;
		return (ioctl(fd, DKIOCUNMAP, &unmap));
	case DIOCGATTR:
		attr = arg;
		if (strcmp(attr->name, "GEOM::candelete") != 0) {
			errno = ENOTTY;
			return (-1);
		}
		if (ioctl(fd, DKIOCGETFEATURES, &features) == -1)
			return (-1);
		attr->value.i = (features & DK_FEATURE_UNMAP) != 0;
		return (0);
	case DIOCGPROVIDERNAME:
		/* no GEOM: block_if then treats the disk as a plain character device */
		errno = ENOTTY;
		return (-1);
	default:
		return (ioctl(fd, req, arg));
	}
}

long
bhyve_fpathconf(int fd, int name)
{
	struct stat sb;

	if (name != _PC_DEALLOC_PRESENT)
		return (fpathconf(fd, name));
	/* F_PUNCHHOLE works on regular files of APFS; whether it does on this file shows at the first call */
	if (fstat(fd, &sb) == -1)
		return (-1);
	return (S_ISREG(sb.st_mode) ? 1 : 0);
}

/*
 * Deallocate [r_offset, r_offset + r_len) so that it reads as zeros. F_PUNCHHOLE takes only ranges
 * aligned to the file system's block size; anything else fails with EINVAL and the guest's TRIM gets
 * an error, as on a FreeBSD file system that cannot deallocate.
 */
int
fspacectl(int fd, int cmd, const struct spacectl_range *rqsr, int flags __unused,
    struct spacectl_range *rmsr)
{
	struct fpunchhole hole;

	if (cmd != SPACECTL_DEALLOC) {
		errno = EINVAL;
		return (-1);
	}
	memset(&hole, 0, sizeof(hole));
	hole.fp_offset = rqsr->r_offset;
	hole.fp_length = rqsr->r_len;
	if (fcntl(fd, F_PUNCHHOLE, &hole) == -1)
		return (-1);
	if (rmsr != NULL) {
		rmsr->r_offset = rqsr->r_offset + rqsr->r_len;
		rmsr->r_len = 0;
	}
	return (0);
}
