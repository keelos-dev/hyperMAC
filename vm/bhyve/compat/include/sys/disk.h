/*
 * FreeBSD's disk ioctls (<sys/disk.h>), fspacectl() and O_DIRECT for block_if.c on macOS.
 * The DIOC* requests get numbers of their own and compat/src/disk.c turns each into macOS's DKIOC*
 * request; block_if.c's ioctl() and fpathconf() calls are routed there.
 */
#ifndef _BHYVE_SYS_DISK_H_
#define	_BHYVE_SYS_DISK_H_

#include_next <sys/disk.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <fcntl.h>
#include <unistd.h>

#define	DIOCGSECTORSIZE		_IOR('b', 128, u_int)
#define	DIOCGMEDIASIZE		_IOR('b', 129, off_t)
#define	DIOCGFLUSH		_IO('b', 135)
#define	DIOCGDELETE		_IOW('b', 136, off_t[2])
#define	DIOCGPROVIDERNAME	_IOR('b', 138, char[MAXPATHLEN])
#define	DIOCGSTRIPESIZE		_IOR('b', 139, off_t)
#define	DIOCGSTRIPEOFFSET	_IOR('b', 140, off_t)
#define	DIOCGATTR		_IOWR('b', 142, struct diocgattr_arg)

#define	DISK_IDENT_SIZE		256
struct diocgattr_arg {
	char name[64];
	int len;
	union {
		char str[DISK_IDENT_SIZE];
		off_t off;
		int i;
		uint16_t u16;
	} value;
};

/* fspacectl(2) */
#define	SPACECTL_DEALLOC	1
struct spacectl_range {
	off_t	r_offset;
	off_t	r_len;
};

/* fpathconf(2): "holes can be punched in this file" */
#define	_PC_DEALLOC_PRESENT	0x7fff0001

/*
 * O_DIRECT (bypass the host's cache; block_if's "nocache" option) is F_NOCACHE on macOS, a fcntl()
 * after open(). The flag gets a bit open(2) does not use and block_if.c's open() goes through
 * bhyve_disk_open(), which turns the bit into that fcntl().
 */
#define	O_DIRECT	0x08000000

__BEGIN_DECLS
int	bhyve_disk_open(const char *path, int flags, ...);
int	bhyve_disk_ioctl(int fd, unsigned long req, ...);
long	bhyve_fpathconf(int fd, int name);
int	fspacectl(int fd, int cmd, const struct spacectl_range *rqsr, int flags,
	    struct spacectl_range *rmsr);
__END_DECLS

#define	open(path, ...)	bhyve_disk_open(path, __VA_ARGS__)
#define	ioctl		bhyve_disk_ioctl
#define	fpathconf	bhyve_fpathconf

#endif /* _BHYVE_SYS_DISK_H_ */
