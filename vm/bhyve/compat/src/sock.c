/* socket() with SOCK_NONBLOCK/SOCK_CLOEXEC, accept4() and pipe2() for macOS (bhyve_macos.h). */
#include <sys/types.h>
#include <sys/socket.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>

#undef socket
#undef pipe2

static int
apply(int fd, int nonblock, int cloexec)
{
	int fl, saved;

	if (fd < 0)
		return (fd);
	if (cloexec && fcntl(fd, F_SETFD, FD_CLOEXEC) == -1)
		goto fail;
	if (nonblock) {
		fl = fcntl(fd, F_GETFL);
		if (fl == -1 || fcntl(fd, F_SETFL, fl | O_NONBLOCK) == -1)
			goto fail;
	}
	return (fd);
fail:
	saved = errno;
	(void)close(fd);
	errno = saved;
	return (-1);
}

int
bhyve_socket(int domain, int type, int protocol)
{
	int fd;

	fd = socket(domain, type & ~(SOCK_NONBLOCK | SOCK_CLOEXEC), protocol);
	return (apply(fd, type & SOCK_NONBLOCK, type & SOCK_CLOEXEC));
}

int
accept4(int s, struct sockaddr *addr, socklen_t *addrlen, int flags)
{
	return (apply(accept(s, addr, addrlen), flags & SOCK_NONBLOCK, flags & SOCK_CLOEXEC));
}

int
bhyve_pipe2(int fds[2], int flags)
{
	int saved;

	if (pipe(fds) == -1)
		return (-1);
	if (apply(fds[0], flags & O_NONBLOCK, flags & O_CLOEXEC) == -1) {
		saved = errno;
		(void)close(fds[1]);
		errno = saved;
		return (-1);
	}
	if (apply(fds[1], flags & O_NONBLOCK, flags & O_CLOEXEC) == -1) {
		saved = errno;
		(void)close(fds[0]);
		errno = saved;
		return (-1);
	}
	return (0);
}
