/* FreeBSD's non-portable pthread calls bhyve uses, on macOS. */
#ifndef _PTHREAD_NP_H_
#define	_PTHREAD_NP_H_

#include <pthread.h>

/* macOS can only name the calling thread: naming another one is skipped (it is cosmetic). */
static inline void
pthread_set_name_np(pthread_t t, const char *name)
{
	if (pthread_equal(t, pthread_self()))
		(void)pthread_setname_np(name);
}

/*
 * Only used in assertions ("the caller holds the lock"). macOS cannot tell who owns a mutex; that it
 * is locked at all is the nearest check: trylock fails with EBUSY.
 */
static inline int
pthread_mutex_isowned_np(pthread_mutex_t *m)
{
	if (pthread_mutex_trylock(m) == 0) {
		(void)pthread_mutex_unlock(m);
		return (0);
	}
	return (1);
}

/* macOS cannot pin a thread to CPUs: fails with ENOTSUP (bhyve -p then warns and runs unpinned). */
int	pthread_setaffinity_np(pthread_t t, size_t cpusetsize, const void *cpuset);

#endif /* _PTHREAD_NP_H_ */
