/*
 * macOS has no tap(4). bhyve's tap backend still builds (the macOS backends in hvf/ reuse its
 * send and receive routines for their datagram sockets); opening /dev/tapN simply fails.
 */
#ifndef _NET_IF_TAP_H_
#define	_NET_IF_TAP_H_

#include <sys/ioctl.h>

#define	VMIO_SIOCSIFFLAGS	_IOW('V', 0, int)

#endif /* _NET_IF_TAP_H_ */
