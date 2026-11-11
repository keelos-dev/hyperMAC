/*
 * Network backends for macOS.
 *
 *   -s N,virtio-net,nat               vmnet.framework's shared mode: the guest is behind the Mac
 *                                     (NAT), macOS's DHCP server gives it an address
 *   -s N,virtio-net,bridged[,if=en0]  vmnet.framework's bridged mode: the guest is on the network
 *                                     of that host interface with an address of its own
 *   -s N,virtio-net,dgram,fd=7        a datagram socket the parent process passed down, one
 *                                     Ethernet frame per datagram (hyperMAC's launcher, tests)
 *
 * vmnet without root needs macOS 26 and the com.apple.security.virtualization entitlement
 * (hvf/bhyve.entitlements). bhyve's own "vmnet" backend is FreeBSD's vmnet(4), a kind of tap.
 */
#include <sys/types.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/un.h>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <dispatch/dispatch.h>
#include <vmnet/vmnet.h>
#include <xpc/xpc.h>

#include "config.h"
#include "debug.h"
#include "iov.h"
#include "mevent.h"
#include "net_backends.h"
#include "net_backends_priv.h"

/* Frames wait in the socket's buffers while the guest is busy: room for a burst, as a NIC's ring. */
#define	DGRAM_SOCKBUF	(4 * 1024 * 1024)

/* The rest of what tap_init() does once it has its descriptor. */
static int
dgram_attach(struct net_backend *be, net_be_rxeof_t cb, void *param)
{
	struct tap_priv *priv = NET_BE_PRIV(be);
	int flags, size = DGRAM_SOCKBUF;

	flags = fcntl(be->fd, F_GETFL);
	if (flags == -1 || fcntl(be->fd, F_SETFL, flags | O_NONBLOCK) == -1) {
		EPRINTLN("network socket: O_NONBLOCK: %s", strerror(errno));
		return (-1);
	}
	/* best effort: the kernel caps these at kern.ipc.maxsockbuf */
	(void)setsockopt(be->fd, SOL_SOCKET, SO_SNDBUF, &size, sizeof(size));
	(void)setsockopt(be->fd, SOL_SOCKET, SO_RCVBUF, &size, sizeof(size));

	memset(priv->bbuf, 0, sizeof(priv->bbuf));
	priv->bbuflen = 0;
	priv->mevp = mevent_add_disabled(be->fd, EVF_READ, cb, param);
	if (priv->mevp == NULL) {
		EPRINTLN("Could not register event");
		return (-1);
	}
	return (0);
}

static int
dgram_init(struct net_backend *be, const char *devname __unused, nvlist_t *nvl, net_be_rxeof_t cb,
    void *param)
{
	const char *value;
	char *end;
	long fd;
	int type;
	socklen_t len = sizeof(type);

	if (cb == NULL) {
		EPRINTLN("dgram backend requires non-NULL callback");
		return (-1);
	}
	value = get_config_value_node(nvl, "fd");
	if (value == NULL) {
		EPRINTLN("dgram backend: fd=N (an inherited datagram socket) must be given");
		return (-1);
	}
	fd = strtol(value, &end, 10);
	if (*value == '\0' || *end != '\0' || fd < 0 ||
	    getsockopt((int)fd, SOL_SOCKET, SO_TYPE, &type, &len) == -1 || type != SOCK_DGRAM) {
		EPRINTLN("dgram backend: fd %s is not a datagram socket", value);
		return (-1);
	}
	be->fd = (int)fd;
	if (dgram_attach(be, cb, param) != 0) {
		tap_cleanup(be);
		return (-1);
	}
	return (0);
}

/*
 * vmnet.framework, in this process. Frames go between the guest's rings and vmnet without a copy
 * in between: vmnet_read() and vmnet_write() take the guest's iovecs.
 *
 * vmnet tells us about received frames on a dispatch queue; bhyve's receive path is driven by a
 * descriptor in its event loop (mevent). A pipe joins the two: the handler puts a byte in, the
 * event loop sees the pipe readable and calls the NIC's receive callback, which pulls frames
 * with vmnet_read() until there are none.
 */
struct vmnet_priv {
	interface_ref iface;
	struct mevent *mevp;
	int	wakeup[2];		/* [0] in the event loop, [1] written by vmnet's handler */
	size_t	max_packet;
	/* the frame peek_recvlen() had to read to know its length */
	char	bbuf[1 << 16];
	ssize_t	bbuflen;
};

static void
vmnet_be_cleanup(struct net_backend *be)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);
	dispatch_semaphore_t done;

	if (priv->mevp != NULL)
		mevent_delete(priv->mevp);
	if (priv->iface != NULL) {
		done = dispatch_semaphore_create(0);
		if (vmnet_stop_interface(priv->iface, dispatch_get_global_queue(QOS_CLASS_UTILITY, 0),
		    ^(vmnet_return_t status __unused) { dispatch_semaphore_signal(done); }) == VMNET_SUCCESS)
			dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
		priv->iface = NULL;
	}
	if (priv->wakeup[0] >= 0) {
		close(priv->wakeup[0]);
		close(priv->wakeup[1]);
		priv->wakeup[0] = priv->wakeup[1] = -1;
	}
	be->fd = -1;
}

/* One frame from vmnet into `iov`; 0 if there is none, -1 on error. */
static ssize_t
vmnet_read_one(struct vmnet_priv *priv, const struct iovec *iov, int iovcnt)
{
	struct vmpktdesc pkt;
	int n = 1;

	pkt.vm_pkt_size = count_iov(iov, iovcnt);
	pkt.vm_pkt_iov = __DECONST(struct iovec *, iov);
	pkt.vm_pkt_iovcnt = iovcnt;
	pkt.vm_flags = 0;
	if (vmnet_read(priv->iface, &pkt, &n) != VMNET_SUCCESS) {
		errno = EIO;
		return (-1);
	}
	return (n == 1 ? (ssize_t)pkt.vm_pkt_size : 0);
}

/*
 * As vmnet_read_one(), and when vmnet has nothing: empty the pipe and look once more, so that a
 * frame that arrived in between is not left waiting for the next one's wakeup.
 */
static ssize_t
vmnet_read_drain(struct vmnet_priv *priv, const struct iovec *iov, int iovcnt)
{
	char junk[64];
	ssize_t len;

	len = vmnet_read_one(priv, iov, iovcnt);
	if (len != 0)
		return (len);
	while (read(priv->wakeup[0], junk, sizeof(junk)) > 0)
		;
	return (vmnet_read_one(priv, iov, iovcnt));
}

static ssize_t
vmnet_be_peek_recvlen(struct net_backend *be)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);
	struct iovec iov;
	ssize_t len;

	if (priv->bbuflen > 0)
		return (priv->bbuflen);
	iov.iov_base = priv->bbuf;
	iov.iov_len = sizeof(priv->bbuf);
	len = vmnet_read_drain(priv, &iov, 1);
	if (len > 0)
		priv->bbuflen = len;
	return (len < 0 ? 0 : len);
}

static ssize_t
vmnet_be_recv(struct net_backend *be, const struct iovec *iov, int iovcnt)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);
	ssize_t len;

	if (priv->bbuflen > 0) {
		/* the frame peek_recvlen() read */
		len = buf_to_iov(priv->bbuf, priv->bbuflen, iov, iovcnt);
		priv->bbuflen = 0;
		return (len);
	}
	len = vmnet_read_drain(priv, iov, iovcnt);
	return (len < 0 ? 0 : len);
}

static ssize_t
vmnet_be_send(struct net_backend *be, const struct iovec *iov, int iovcnt)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);
	struct vmpktdesc pkt;
	int n = 1;

	pkt.vm_pkt_size = count_iov(iov, iovcnt);
	if (pkt.vm_pkt_size > priv->max_packet) {
		/* dropped, as a NIC drops an oversized frame */
		return (pkt.vm_pkt_size);
	}
	pkt.vm_pkt_iov = __DECONST(struct iovec *, iov);
	pkt.vm_pkt_iovcnt = iovcnt;
	pkt.vm_flags = 0;
	if (vmnet_write(priv->iface, &pkt, &n) != VMNET_SUCCESS || n != 1) {
		errno = ENOBUFS;
		return (-1);
	}
	return (pkt.vm_pkt_size);
}

static void
vmnet_be_recv_enable(struct net_backend *be)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);

	mevent_enable(priv->mevp);
}

static void
vmnet_be_recv_disable(struct net_backend *be)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);

	mevent_disable(priv->mevp);
}

static uint64_t
vmnet_be_get_cap(struct net_backend *be __unused)
{
	return (0);	/* no offloads: the guest computes checksums and sends frames of at most the MTU */
}

static int
vmnet_be_set_cap(struct net_backend *be __unused, uint64_t features, unsigned vnet_hdr_len)
{
	return ((features || vnet_hdr_len) ? -1 : 0);
}

static const char *
vmnet_strerror(vmnet_return_t status)
{
	switch (status) {
	case VMNET_FAILURE:
		return ("general failure (macOS 26 or later and the com.apple.security.virtualization "
		    "entitlement are needed to use vmnet without root)");
	case VMNET_MEM_FAILURE:
		return ("out of memory");
	case VMNET_INVALID_ARGUMENT:
		return ("invalid argument (is the host interface's name right?)");
	case VMNET_SETUP_INCOMPLETE:
		return ("interface setup is not complete");
	case VMNET_INVALID_ACCESS:
		return ("permission denied");
	case VMNET_PACKET_TOO_BIG:
		return ("packet too big");
	case VMNET_BUFFER_EXHAUSTED:
		return ("buffers exhausted");
	case VMNET_TOO_MANY_PACKETS:
		return ("too many packets");
	case VMNET_SHARING_SERVICE_BUSY:
		return ("Internet Sharing is in use");
	default:
		return ("unknown error");
	}
}

/* `host_if` NULL: vmnet's shared mode (NAT, macOS hands out addresses); else bridged to it. */
static int
vmnet_be_start(struct net_backend *be, const char *host_if, net_be_rxeof_t cb, void *param)
{
	struct vmnet_priv *priv = NET_BE_PRIV(be);
	xpc_object_t desc;
	dispatch_queue_t queue;
	dispatch_semaphore_t started;
	__block vmnet_return_t status = VMNET_FAILURE;
	__block uint64_t max_packet = 0;
	int wfd, flags, i;

	if (cb == NULL) {
		EPRINTLN("vmnet backend requires non-NULL callback");
		return (-1);
	}
	priv->wakeup[0] = priv->wakeup[1] = -1;
	priv->bbuflen = 0;

	desc = xpc_dictionary_create(NULL, NULL, 0);
	xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key,
	    host_if != NULL ? VMNET_BRIDGED_MODE : VMNET_SHARED_MODE);
	if (host_if != NULL)
		xpc_dictionary_set_string(desc, vmnet_shared_interface_name_key, host_if);
	queue = dispatch_queue_create("bhyve.vmnet", DISPATCH_QUEUE_SERIAL);
	started = dispatch_semaphore_create(0);
	priv->iface = vmnet_start_interface(desc, queue, ^(vmnet_return_t st, xpc_object_t params) {
		status = st;
		if (st == VMNET_SUCCESS && params != NULL)
			max_packet = xpc_dictionary_get_uint64(params, vmnet_max_packet_size_key);
		dispatch_semaphore_signal(started);
	});
	xpc_release(desc);
	if (priv->iface == NULL ||
	    dispatch_semaphore_wait(started, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0 ||
	    status != VMNET_SUCCESS) {
		EPRINTLN("vmnet (%s%s): %s", host_if != NULL ? "bridged to " : "shared",
		    host_if != NULL ? host_if : "", vmnet_strerror(status));
		priv->iface = NULL;
		return (-1);
	}
	priv->max_packet = max_packet != 0 ? max_packet : 1514;

	if (pipe(priv->wakeup) == -1) {
		EPRINTLN("vmnet: pipe: %s", strerror(errno));
		goto error;
	}
	for (i = 0; i < 2; i++) {
		flags = fcntl(priv->wakeup[i], F_GETFL);
		(void)fcntl(priv->wakeup[i], F_SETFL, flags | O_NONBLOCK);
		(void)fcntl(priv->wakeup[i], F_SETFD, FD_CLOEXEC);
	}
	be->fd = priv->wakeup[0];
	priv->mevp = mevent_add_disabled(priv->wakeup[0], EVF_READ, cb, param);
	if (priv->mevp == NULL) {
		EPRINTLN("Could not register event");
		goto error;
	}
	wfd = priv->wakeup[1];
	if (vmnet_interface_set_event_callback(priv->iface, VMNET_INTERFACE_PACKETS_AVAILABLE, queue,
	    ^(interface_event_t event __unused, xpc_object_t ev __unused) {
		/* a full pipe already means "look": the write may fail */
		(void)write(wfd, "", 1);
	    }) != VMNET_SUCCESS) {
		EPRINTLN("vmnet: could not set the receive handler");
		goto error;
	}
	return (0);

error:
	vmnet_be_cleanup(be);
	return (-1);
}

static int
nat_init(struct net_backend *be, const char *devname __unused, nvlist_t *nvl __unused,
    net_be_rxeof_t cb, void *param)
{
	return (vmnet_be_start(be, NULL, cb, param));
}

static int
bridged_init(struct net_backend *be, const char *devname __unused, nvlist_t *nvl, net_be_rxeof_t cb,
    void *param)
{
	const char *host_if;

	host_if = get_config_value_node(nvl, "if");
	if (host_if == NULL)
		host_if = "en0";
	return (vmnet_be_start(be, host_if, cb, param));
}

static struct net_backend dgram_backend = {
	.prefix = "dgram",
	.priv_size = sizeof(struct tap_priv),
	.init = dgram_init,
	.cleanup = tap_cleanup,
	.send = tap_send,
	.peek_recvlen = tap_peek_recvlen,
	.recv = tap_recv,
	.recv_enable = tap_recv_enable,
	.recv_disable = tap_recv_disable,
	.get_cap = tap_get_cap,
	.set_cap = tap_set_cap,
};

static struct net_backend nat_backend = {
	.prefix = "nat",
	.priv_size = sizeof(struct vmnet_priv),
	.init = nat_init,
	.cleanup = vmnet_be_cleanup,
	.send = vmnet_be_send,
	.peek_recvlen = vmnet_be_peek_recvlen,
	.recv = vmnet_be_recv,
	.recv_enable = vmnet_be_recv_enable,
	.recv_disable = vmnet_be_recv_disable,
	.get_cap = vmnet_be_get_cap,
	.set_cap = vmnet_be_set_cap,
};

static struct net_backend bridged_backend = {
	.prefix = "bridged",
	.priv_size = sizeof(struct vmnet_priv),
	.init = bridged_init,
	.cleanup = vmnet_be_cleanup,
	.send = vmnet_be_send,
	.peek_recvlen = vmnet_be_peek_recvlen,
	.recv = vmnet_be_recv,
	.recv_enable = vmnet_be_recv_enable,
	.recv_disable = vmnet_be_recv_disable,
	.get_cap = vmnet_be_get_cap,
	.set_cap = vmnet_be_set_cap,
};

DATA_SET(net_backend_set, dgram_backend);
DATA_SET(net_backend_set, nat_backend);
DATA_SET(net_backend_set, bridged_backend);
