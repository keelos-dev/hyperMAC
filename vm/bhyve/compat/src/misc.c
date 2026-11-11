/* Small FreeBSD calls with no macOS equivalent that bhyve can do without. */
#include <sys/types.h>
#include <sys/cpuset.h>
#include <sys/domainset.h>
#include <sys/sysctl.h>

#include <sys/socket.h>
#include <sys/un.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread_np.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* macOS has no way to change what ps shows for a running process. */
void
setproctitle(const char *fmt __unused, ...)
{
}

int
pthread_setaffinity_np(pthread_t t __unused, size_t cpusetsize __unused, const void *cpuset __unused)
{
	return (ENOTSUP);
}

/* For cpuset_parselist()'s "all": every CPU the host has. */
int
cpuset_getaffinity(cpulevel_t level __unused, cpuwhich_t which __unused, id_t id __unused,
    size_t setsize, cpuset_t *mask)
{
	int i, ncpu;
	size_t len = sizeof(ncpu);

	if (setsize < sizeof(cpuset_t)) {
		errno = ERANGE;
		return (-1);
	}
	if (sysctlbyname("hw.ncpu", &ncpu, &len, NULL, 0) == -1)
		return (-1);
	CPU_ZERO(mask);
	for (i = 0; i < ncpu && i < CPU_SETSIZE; i++)
		CPU_SET(i, mask);
	return (0);
}

/* No NUMA domains on a Mac: domainset_parselist() reports CPUSET_PARSE_GETDOMAIN. */
int
cpuset_getdomain(cpulevel_t level __unused, cpuwhich_t which __unused, id_t id __unused,
    size_t setsize __unused, domainset_t *mask __unused, int *policy __unused)
{
	errno = ENOSYS;
	return (-1);
}

void *
reallocarray(void *ptr, size_t nmemb, size_t size)
{
	size_t total;

	if (__builtin_mul_overflow(nmemb, size, &total)) {
		errno = ENOMEM;
		return (NULL);
	}
	return (realloc(ptr, total));
}

/*
 * bindat(2) for a unix socket whose path is relative to the directory fd: macOS has none, so the
 * directory's path (F_GETPATH) goes in front. The whole path has to fit sun_path.
 */
int
bindat(int fd, int s, const struct sockaddr *addr, socklen_t addrlen)
{
	const struct sockaddr_un *sun = (const struct sockaddr_un *)addr;
	struct sockaddr_un full;
	char dir[MAXPATHLEN];

	if (fd == AT_FDCWD || addr->sa_family != AF_UNIX || sun->sun_path[0] == '/')
		return (bind(s, addr, addrlen));
	if (fcntl(fd, F_GETPATH, dir) == -1)
		return (-1);
	memset(&full, 0, sizeof(full));
	full.sun_family = AF_UNIX;
	if ((size_t)snprintf(full.sun_path, sizeof(full.sun_path), "%s/%s", dir, sun->sun_path) >=
	    sizeof(full.sun_path)) {
		errno = ENAMETOOLONG;
		return (-1);
	}
	full.sun_len = SUN_LEN(&full);
	return (bind(s, (struct sockaddr *)&full, sizeof(full)));
}

#undef pthread_mutex_init

/* See bhyve_macos.h: default attributes mean an error-checking mutex, as on FreeBSD. */
int
bhyve_mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *attr)
{
	pthread_mutexattr_t errorcheck;
	int error;

	/*
	 * BHYVE_NORMAL_MUTEX=1 (a test): macOS's plain mutex, with which a thread that locks
	 * what it holds stops for good. A guest that runs with it has no such path.
	 */
	if (attr != NULL || getenv("BHYVE_NORMAL_MUTEX") != NULL)
		return (pthread_mutex_init(m, attr));
	error = pthread_mutexattr_init(&errorcheck);
	if (error != 0)
		return (error);
	(void)pthread_mutexattr_settype(&errorcheck, PTHREAD_MUTEX_ERRORCHECK);
	error = pthread_mutex_init(m, &errorcheck);
	(void)pthread_mutexattr_destroy(&errorcheck);
	return (error);
}

#undef pthread_mutex_lock
#undef pthread_mutex_unlock

/* One report per place: the places are few, the calls many. */
static void
mutex_report(const char *what, int error, const char *file, int line)
{
	static struct { const char *file; int line; } seen[64];
	static pthread_mutex_t seen_mtx = PTHREAD_MUTEX_INITIALIZER;
	const char *base;
	int i, new;

	new = 0;
	pthread_mutex_lock(&seen_mtx);
	for (i = 0; i < 64 && seen[i].file != NULL; i++)
		if (seen[i].file == file && seen[i].line == line)
			break;
	if (i < 64 && seen[i].file == NULL) {
		seen[i].file = file;
		seen[i].line = line;
		new = 1;
	}
	pthread_mutex_unlock(&seen_mtx);
	if (!new)
		return;
	base = strrchr(file, '/');
	fprintf(stderr, "bhyve: %s:%d: %s: %s\r\n", base != NULL ? base + 1 : file, line, what,
	    error == EDEADLK ? "this thread holds the mutex already" : "this thread does not hold the mutex");
}

/* See bhyve_macos.h. */
int
bhyve_mutex_lock(pthread_mutex_t *m, const char *file, int line)
{
	int error;

	error = pthread_mutex_lock(m);
	if (error == EDEADLK)
		mutex_report("pthread_mutex_lock", error, file, line);
	return (error);
}

int
bhyve_mutex_unlock(pthread_mutex_t *m, const char *file, int line)
{
	int error;

	error = pthread_mutex_unlock(m);
	if (error == EPERM)
		mutex_report("pthread_mutex_unlock", error, file, line);
	return (error);
}
