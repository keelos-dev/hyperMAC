/*
 * Force-included (-include) into every FreeBSD source file built on macOS: the FreeBSD spellings
 * macOS's headers lack. Nothing here changes what the code does on FreeBSD.
 */
#ifndef _BHYVE_MACOS_H_
#define	_BHYVE_MACOS_H_

#include <sys/cdefs.h>
#include <sys/types.h>
#include <sys/param.h>
#include <sys/time.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <mach/vm_param.h>
#include <sys/mman.h>
#include <sys/endian.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <machine/atomic.h>	/* FreeBSD's <sys/cpuset.h> users get it without asking */
#include <sys/linker_set.h>	/* FreeBSD's <sys/param.h> path brings it in; several files rely on that */

/*
 * The guest's page size, and the stage-2 granule we ask Hypervisor.framework for (hvf/): 4K, as on
 * FreeBSD/arm64. macOS's PAGE_SIZE is the host's (16K) and not a constant expression.
 */
#undef PAGE_SIZE
#undef PAGE_SHIFT
#undef PAGE_MASK
#undef round_page
#undef trunc_page
#define	PAGE_SHIFT	12
#define	PAGE_SIZE	(1 << PAGE_SHIFT)
#define	PAGE_MASK	(PAGE_SIZE - 1)
#define	round_page(x)	(((uintptr_t)(x) + PAGE_MASK) & ~(uintptr_t)PAGE_MASK)
#define	trunc_page(x)	((uintptr_t)(x) & ~(uintptr_t)PAGE_MASK)
#define	PAGE_SIZE_4K	(4 * 1024)
#define	PAGE_SIZE_16K	(16 * 1024)
#define	PAGE_SIZE_64K	(64 * 1024)

typedef uint64_t	vm_paddr_t;
typedef int64_t		vm_ooffset_t;
typedef int64_t		register_t;
typedef unsigned long	cap_ioctl_t;
typedef int		cpusetid_t;
typedef uintptr_t	__uintptr_t;
typedef size_t		__size_t;
typedef intptr_t	__intptr_t;
typedef int		cpuwhich_t;
typedef int		cpulevel_t;

#ifndef __packed
#define	__packed	__attribute__((__packed__))
#endif
#define	SPECNAMELEN	255		/* FreeBSD <sys/param.h>: longest /dev name */
#define	SUNPATHLEN	104		/* FreeBSD <sys/un.h>: sizeof(sun_path) */
#define	ETHER_VLAN_ENCAP_LEN	4	/* FreeBSD <net/ethernet.h> */
#define	ETHER_IS_MULTICAST(addr) (*(addr) & 0x01)	/* FreeBSD <net/ethernet.h> */
#define	_PROT_ALL	(PROT_READ | PROT_WRITE | PROT_EXEC)
#define	EJUSTRETURN	(-2)		/* FreeBSD's kernel-internal errno, used by mevent/ipc handlers */
#ifndef nitems
#define	nitems(x)	(sizeof((x)) / sizeof((x)[0]))
#endif
#ifndef __DECONST
#define	__DECONST(type, var)	((type)(uintptr_t)(const void *)(var))
#endif
#ifndef __containerof
#define	__containerof(x, s, m)	((s *)(void *)((char *)(x) - offsetof(s, m)))
#endif
#ifndef roundup2
#define	roundup2(x, y)	(((x) + ((y) - 1)) & (~((y) - 1)))
#define	rounddown2(x, y) ((x) & (~((y) - 1)))
#endif
#ifndef __predict_true
#define	__predict_true(exp)	__builtin_expect((exp), 1)
#define	__predict_false(exp)	__builtin_expect((exp), 0)
#endif
#define	__assert_unreachable()	abort()		/* FreeBSD <assert.h> */
#ifndef __unreachable
#define	__unreachable()	__builtin_unreachable()
#endif
#ifndef __nonnull
#define	__nonnull(x)	__attribute__((__nonnull__ x))
#endif
#ifndef __result_use_check
#define	__result_use_check __attribute__((__warn_unused_result__))
#endif
#ifndef __aligned
#define	__aligned(x)	__attribute__((__aligned__(x)))
#endif
#ifndef __weak_symbol
#define	__weak_symbol	__attribute__((__weak__))
#endif

/* compat/src/misc.c */
/*
 * macOS has no SOCK_NONBLOCK/SOCK_CLOEXEC, accept4() or pipe2(): the flags get bits socket types do
 * not use, and compat/src/sock.c applies them with fcntl() after the call.
 */
#define	SOCK_CLOEXEC	0x10000000
#define	SOCK_NONBLOCK	0x20000000
__BEGIN_DECLS
int	bhyve_socket(int domain, int type, int protocol);
int	accept4(int s, struct sockaddr *addr, socklen_t *addrlen, int flags);
int	bhyve_pipe2(int fds[2], int flags);
int	bindat(int fd, int s, const struct sockaddr *addr, socklen_t addrlen);
void	*reallocarray(void *ptr, size_t nmemb, size_t size);
void	setproctitle(const char *fmt, ...) __printflike(1, 2);	/* compat/src/misc.c: does nothing */
__END_DECLS
#define	socket(d, t, p)	bhyve_socket((d), (t), (p))
#define	pipe2(f, fl)	bhyve_pipe2((f), (fl))	/* macOS has pipe2() from 27 on; ours runs on 26 too */

/*
 * FreeBSD's default mutex is error-checking: locking one the thread already holds fails with EDEADLK
 * and the thread goes on. bhyve depends on that without knowing (pci_xhci_handle_transfer() ->
 * pci_xhci_try_usb_xfer(), virtio's vi_interrupt() before patches/upstream/0002, ...): with macOS's
 * default mutex those paths deadlock. So a mutex initialized with default attributes is an
 * error-checking one here too (compat/src/misc.c).
 */
#include <pthread.h>
__BEGIN_DECLS
int	bhyve_mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *attr);
__END_DECLS
#define	pthread_mutex_init(m, attr)	bhyve_mutex_init((m), (attr))

/*
 * The audit of that dependence: a lock that fails with EDEADLK (the thread holds it already) or an
 * unlock that fails with EPERM (the thread does not hold it: an inner unlock released it) is
 * reported on stderr, once per place in the source. FreeBSD says nothing in either case. Every
 * line this prints is a locking bug to fix in bhyve (docs/bhyve-plan.md section 5a).
 */
__BEGIN_DECLS
int	bhyve_mutex_lock(pthread_mutex_t *m, const char *file, int line);
int	bhyve_mutex_unlock(pthread_mutex_t *m, const char *file, int line);
__END_DECLS
#define	pthread_mutex_lock(m)	bhyve_mutex_lock((m), __FILE__, __LINE__)
#define	pthread_mutex_unlock(m)	bhyve_mutex_unlock((m), __FILE__, __LINE__)

#endif /* _BHYVE_MACOS_H_ */
