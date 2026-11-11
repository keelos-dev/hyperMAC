/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * Copyright (c) 2026 keelOS
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS'' AND
 * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
 * OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
 * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */

/*
 * keel: remote USB devices (docs/66 §4.3, phase 1).
 *
 *   -s 30,xhci,tablet,remote2,remote3 -o keel.usb_sock=/path/usb.sock
 *
 * "remote2" is an empty USB 2 port of the xHCI, "remote3" an empty USB 3 port ("remote2=2"
 * gives two, patch-xhci.py). A source (keel-rdp, tools/usb/kusb-loop.py) connects to the unix
 * socket (mode 0600, one connection at a time) and speaks kusb (docs/66 §4.2.1): ATTACH puts a
 * device on a free port with its descriptors, the guest sees a connect; the guest's transfers go
 * to the source as CONTROL/BULK/INTR with an id and come back as COMPLETE. Nothing waits on the
 * network in a vCPU thread: ue_request()/ue_data() send the request and return USB_PENDING; the
 * socket thread stores the answer and calls hci_complete(), under whose lock the xHCI calls
 * ue_request()/ue_data() again with the same transfer and gets the result.
 *
 * GET_DESCRIPTOR for the device, configuration, BOS and string descriptors is answered from the
 * copies ATTACH brought (no round trip while the guest enumerates); SET_ADDRESS never reaches a
 * device (the xHCI's Address Device command does the addressing). Everything else is forwarded.
 *
 * Locks: the xHCI calls in with its mutex held and the device takes its own (rs_mtx) after it;
 * the socket thread holds no lock of the device while it calls into the xHCI. The output queue
 * has a lock of its own, taken last. Sends never block: what the socket does not take now is
 * queued and the thread writes it when the socket has room.
 *
 * Checkpoints: a remote device is not in them (ue_snapshot writes an "empty" marker); after a
 * restore the source is gone, the guest sees the port disconnect and the source attaches again
 * (patch-xhci.py, docs/66 §4.3 item 5).
 */

#include <sys/types.h>
#include <sys/endian.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#include <machine/vmm_snapshot.h>

#include <dev/usb/usb.h>
#include <dev/usb/usbdi.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <pthread.h>
#include <pthread_np.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "bhyverun.h"
#include "config.h"
#include "debug.h"
#include "usb_emul.h"
#include "usb_remote.h"

#define	RS_MAXPORTS	16
#define	RS_MAXMSG	(16 * 1024 * 1024)	/* a larger payload ends the connection */
#define	RS_NEPS		32			/* xHCI endpoint index (DCI) 1..31 */
#define	RS_NGONE	1024			/* cancelled requests remembered */
#define	RS_MAXSTASH	(1024 * 1024)		/* IN data kept from them, per endpoint */

/* the request in flight on one endpoint (the xHCI sends one TD per endpoint at a time) */
struct rs_req {
	bool		active;
	bool		done;
	uint32_t	id;
	struct usb_data_xfer *xfer;
	int		status;		/* KUSB_ST_* */
	uint8_t		*data;		/* COMPLETE's payload */
	uint32_t	len;
	uint32_t	outlen;		/* OUT: bytes sent */
	struct timespec	t0;
};

struct rs_str {
	uint16_t	lang;
	uint8_t		index;
	uint8_t		len;
	uint8_t		*d;		/* the whole string descriptor */
};

struct rs_softc {
	struct usb_hci	*hci;
	int		usbver;		/* 2 or 3: the kind of port */
	pthread_mutex_t	mtx;
	bool		attached;
	int		speed;		/* USB_SPEED_* */
	uint8_t		*dev;		/* device descriptor */
	int		dev_len;
	uint8_t		**cfg;		/* configuration descriptors, by index */
	int		*cfg_len;
	int		ncfg;
	uint8_t		*bos;
	int		bos_len;
	struct rs_str	*str;
	int		nstr;
	uint8_t		eptype[32];	/* UE_* by endpoint (address & 0xf) + (IN ? 16 : 0) */
	struct rs_req	rq[RS_NEPS];
	/*
	 * IN data of cancelled requests: the source may already have taken it from the device
	 * (FreeRDP's client does not abort the transfer); the next IN on that endpoint gets it,
	 * as it would from a device that still held it. gone[]: the cancelled requests' ids.
	 */
	struct {
		uint32_t	id;
		int		dci;
	}		gone[RS_NGONE];
	int		ngone;
	uint8_t		*stash[RS_NEPS];
	uint32_t	stash_len[RS_NEPS];
};

/* the socket and its thread: one per bhyve */
static struct {
	char		*path;
	int		lfd;		/* listening */
	int		cfd;		/* the source's connection, -1 when none */
	int		wake[2];	/* a send queued output: poll for POLLOUT too */
	pthread_t	thr;
	bool		started;
	pthread_mutex_t	rmtx;		/* ready */
	pthread_cond_t	rcv;
	bool		ready;
	pthread_mutex_t	omtx;		/* output queue, cfd */
	uint8_t		*obuf;
	size_t		olen, ocap;
	uint32_t	next_id;
	int		debug;
	struct rs_softc	*ports[RS_MAXPORTS];
	int		nports;
} hub = {
	.lfd = -1, .cfd = -1, .wake = { -1, -1 },
	.rmtx = PTHREAD_MUTEX_INITIALIZER, .rcv = PTHREAD_COND_INITIALIZER,
	.omtx = PTHREAD_MUTEX_INITIALIZER,
};

static void
rs_log(const char *fmt, ...) __printflike(1, 2);
static void
rs_log(const char *fmt, ...)
{
	struct timespec ts;
	struct tm tm;
	char t[16];
	va_list ap;

	clock_gettime(CLOCK_REALTIME, &ts);
	gmtime_r(&ts.tv_sec, &tm);
	strftime(t, sizeof(t), "%H:%M:%S", &tm);
	fprintf(stderr, "%s.%03ldZ keel-usb: ", t, ts.tv_nsec / 1000000);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
}

static uint32_t
rs_newid(void)
{
	uint32_t id;

	do {
		id = __atomic_add_fetch(&hub.next_id, 1, __ATOMIC_SEQ_CST);
	} while (id == 0);
	return (id);
}

/* write what the socket takes now; omtx held */
static void
rs_flush_locked(void)
{
	ssize_t n;

	while (hub.olen > 0 && hub.cfd >= 0) {
		n = send(hub.cfd, hub.obuf, hub.olen, MSG_DONTWAIT | MSG_NOSIGNAL);
		if (n <= 0)
			break;		/* EAGAIN: the thread waits for room; errors: it sees EOF */
		memmove(hub.obuf, hub.obuf + n, hub.olen - n);
		hub.olen -= n;
	}
}

static int
rs_send(uint8_t type, uint8_t port, uint8_t ep, uint8_t status, uint32_t id,
    const void *p1, size_t l1, const void *p2, size_t l2)
{
	struct kusb_hdr h;
	size_t need;
	uint8_t *nb;
	char c = 1;

	h.type = type;
	h.port = port;
	h.ep = ep;
	h.status = status;
	h.id = htole32(id);
	h.length = htole32((uint32_t)(l1 + l2));
	h.rsvd = 0;

	pthread_mutex_lock(&hub.omtx);
	if (hub.cfd < 0) {
		pthread_mutex_unlock(&hub.omtx);
		return (-1);
	}
	need = hub.olen + sizeof(h) + l1 + l2;
	if (need > hub.ocap) {
		size_t cap = hub.ocap ? hub.ocap : 65536;

		while (cap < need)
			cap *= 2;
		nb = realloc(hub.obuf, cap);
		if (nb == NULL) {
			pthread_mutex_unlock(&hub.omtx);
			return (-1);
		}
		hub.obuf = nb;
		hub.ocap = cap;
	}
	memcpy(hub.obuf + hub.olen, &h, sizeof(h));
	hub.olen += sizeof(h);
	if (l1 > 0) {
		memcpy(hub.obuf + hub.olen, p1, l1);
		hub.olen += l1;
	}
	if (l2 > 0) {
		memcpy(hub.obuf + hub.olen, p2, l2);
		hub.olen += l2;
	}
	rs_flush_locked();
	if (hub.olen > 0)
		(void)write(hub.wake[1], &c, 1);
	pthread_mutex_unlock(&hub.omtx);
	return (0);
}

static int
rs_port(struct rs_softc *sc)
{
	return (sc->hci->hci_port);
}

static int
rs_dci(int epaddr)
{
	if ((epaddr & 0x0f) == 0)
		return (1);
	return ((epaddr & 0x0f) * 2 + ((epaddr & 0x80) ? 1 : 0));
}

static void
rs_req_clear(struct rs_req *r)
{
	free(r->data);
	memset(r, 0, sizeof(*r));
}

/* the transfer's data blocks; IN: copied from p, OUT: counted. Every block is processed. */
static uint32_t
rs_fill(struct usb_data_xfer *xfer, int in, const uint8_t *p, uint32_t left)
{
	struct usb_data_xfer_block *b;
	uint32_t n, total;
	int i, idx;

	total = 0;
	for (i = 0, idx = xfer->head; i < xfer->ndata;
	    i++, idx = (idx + 1) % USB_MAX_XFER_BLOCKS) {
		b = &xfer->data[idx];
		if (b->buf != NULL && b->blen > 0) {
			n = (uint32_t)b->blen < left ? (uint32_t)b->blen : left;
			if (in && n > 0 && p != NULL) {
				memcpy(b->buf, p, n);
				p += n;
			}
			b->bdone = n;
			b->blen -= n;
			left -= n;
			total += n;
		}
		b->processed = 1;
	}
	return (total);
}

/* bytes the data blocks want (IN) or carry (OUT); OUT data gathered into *out */
static uint32_t
rs_gather(struct usb_data_xfer *xfer, uint32_t max, uint8_t **out)
{
	struct usb_data_xfer_block *b;
	uint32_t total, n;
	uint8_t *buf;
	int i, idx;

	total = 0;
	for (i = 0, idx = xfer->head; i < xfer->ndata;
	    i++, idx = (idx + 1) % USB_MAX_XFER_BLOCKS) {
		b = &xfer->data[idx];
		if (b->buf != NULL && b->blen > 0)
			total += b->blen;
	}
	if (total > max)
		total = max;
	if (out == NULL)
		return (total);
	*out = buf = total > 0 ? malloc(total) : NULL;
	if (total > 0 && buf == NULL)
		return (0);
	n = 0;
	for (i = 0, idx = xfer->head; i < xfer->ndata && n < total;
	    i++, idx = (idx + 1) % USB_MAX_XFER_BLOCKS) {
		b = &xfer->data[idx];
		if (b->buf != NULL && b->blen > 0) {
			uint32_t k = (uint32_t)b->blen < total - n ? (uint32_t)b->blen : total - n;

			memcpy(buf + n, b->buf, k);
			n += k;
		}
	}
	return (total);
}

/* IN data that came back for a cancelled request: kept for the next IN; rs_mtx held */
static void
rs_stash(struct rs_softc *sc, int dci, const uint8_t *p, uint32_t len)
{
	uint8_t *nb;

	if (dci <= 1 || (dci & 1) == 0 || len == 0 || sc->stash_len[dci] + len > RS_MAXSTASH)
		return;
	nb = realloc(sc->stash[dci], sc->stash_len[dci] + len);
	if (nb == NULL)
		return;
	memcpy(nb + sc->stash_len[dci], p, len);
	sc->stash[dci] = nb;
	sc->stash_len[dci] += len;
	if (hub.debug)
		rs_log("port %d dci %d: %u bytes kept for the next read (%u in all)", rs_port(sc),
		    dci, len, sc->stash_len[dci]);
}

static void
rs_forget(struct rs_softc *sc, uint32_t id, int dci)
{
	sc->gone[sc->ngone % RS_NGONE].id = id;
	sc->gone[sc->ngone % RS_NGONE].dci = dci;
	sc->ngone++;
}

static void
rs_clear_stash(struct rs_softc *sc)
{
	int i;

	for (i = 0; i < RS_NEPS; i++) {
		free(sc->stash[i]);
		sc->stash[i] = NULL;
		sc->stash_len[i] = 0;
	}
	memset(sc->gone, 0, sizeof(sc->gone));
	sc->ngone = 0;
}

static int
rs_status2err(int st)
{
	switch (st) {
	case KUSB_ST_OK:
		return (USB_ERR_NORMAL_COMPLETION);
	case KUSB_ST_STALL:
		return (USB_ERR_STALLED);
	case KUSB_ST_CANCELLED:
		return (USB_ERR_CANCELLED);
	default:
		return (USB_ERR_IOERROR);
	}
}

/* the answer of a finished request into the transfer; rs_mtx held */
static int
rs_deliver(struct rs_softc *sc, int dci, struct rs_req *r, int in)
{
	struct usb_data_xfer *xfer = r->xfer;
	uint32_t got;
	int err;

	if (r->status == KUSB_ST_OK) {
		if (in && sc->stash_len[dci] > 0) {
			/* older data from cancelled reads first: this answer goes behind it */
			rs_stash(sc, dci, r->data, r->len);
			got = rs_fill(xfer, 1, sc->stash[dci], sc->stash_len[dci]);
			memmove(sc->stash[dci], sc->stash[dci] + got, sc->stash_len[dci] - got);
			sc->stash_len[dci] -= got;
		} else if (in)
			got = rs_fill(xfer, 1, r->data, r->len);
		else
			got = rs_fill(xfer, 0, NULL,
			    r->len >= 4 ? le32dec(r->data) : r->outlen);
	} else
		got = rs_fill(xfer, 0, NULL, 0);
	err = rs_status2err(r->status);
	if (hub.debug) {
		struct timespec t1;

		clock_gettime(CLOCK_MONOTONIC, &t1);
		rs_log("port %d id %u dci %d %s %u bytes status %d, %ld us", rs_port(sc), r->id,
		    dci, in ? "in" : "out", got, r->status,
		    (long)((t1.tv_sec - r->t0.tv_sec) * 1000000 +
		    (t1.tv_nsec - r->t0.tv_nsec) / 1000));
	}
	rs_req_clear(r);
	return (err);
}

/*
 * A transfer the xHCI hands in: the answer if it is there, USB_PENDING while it is not, a new
 * request otherwise. rs_mtx held. *newreq is set when the caller has to send one.
 */
static struct rs_req *
rs_lookup(struct rs_softc *sc, int dci, struct usb_data_xfer *xfer, int in, int *ret)
{
	struct rs_req *r = &sc->rq[dci];

	if (r->active && r->xfer == xfer) {
		*ret = r->done ? rs_deliver(sc, dci, r, in) : USB_PENDING;
		return (NULL);
	}
	if (r->active) {	/* a request of a transfer the xHCI dropped without telling */
		if (!r->done)
			rs_send(KUSB_CANCEL, rs_port(sc), 0, 0, r->id, NULL, 0, NULL, 0);
		rs_req_clear(r);
	}
	if (!sc->attached) {
		rs_fill(xfer, 0, NULL, 0);
		*ret = USB_ERR_IOERROR;
		return (NULL);
	}
	r->active = true;
	r->done = false;
	r->id = rs_newid();
	r->xfer = xfer;
	clock_gettime(CLOCK_MONOTONIC, &r->t0);
	return (r);
}

/* a descriptor from ATTACH's copies, or NULL */
static const uint8_t *
rs_desc(struct rs_softc *sc, int type, int index, int lang, int *len)
{
	int i;

	switch (type) {
	case UDESC_DEVICE:
		*len = sc->dev_len;
		return (sc->dev);
	case UDESC_CONFIG:
		if (index < sc->ncfg && sc->cfg[index] != NULL) {
			*len = sc->cfg_len[index];
			return (sc->cfg[index]);
		}
		return (NULL);
	case UDESC_BOS:
		*len = sc->bos_len;
		return (sc->bos_len > 0 ? sc->bos : NULL);
	case UDESC_STRING:
		for (i = 0; i < sc->nstr; i++)
			if (sc->str[i].index == index &&
			    (index == 0 || sc->str[i].lang == lang)) {
				*len = sc->str[i].len;
				return (sc->str[i].d);
			}
		return (NULL);
	}
	return (NULL);
}

static int
rs_request(void *scarg, struct usb_data_xfer *xfer)
{
	struct rs_softc *sc = scarg;
	struct usb_device_request *req = xfer->ureq;
	struct rs_req *r;
	const uint8_t *d;
	uint8_t *out;
	uint16_t value, index, length;
	uint32_t olen;
	int in, ret, dlen;

	if (req == NULL) {
		rs_fill(xfer, 0, NULL, 0);
		return (USB_ERR_INVAL);
	}
	value = UGETW(req->wValue);
	index = UGETW(req->wIndex);
	length = UGETW(req->wLength);
	in = (req->bmRequestType & UT_READ) != 0;

	pthread_mutex_lock(&sc->mtx);
	r = rs_lookup(sc, 1, xfer, in, &ret);
	if (r == NULL) {
		pthread_mutex_unlock(&sc->mtx);
		return (ret);
	}

	/* answered here: descriptors ATTACH brought, SET_ADDRESS */
	if (req->bmRequestType == UT_READ_DEVICE && req->bRequest == UR_GET_DESCRIPTOR &&
	    (d = rs_desc(sc, value >> 8, value & 0xff, index, &dlen)) != NULL) {
		rs_req_clear(r);
		rs_fill(xfer, 1, d, dlen < length ? (uint32_t)dlen : length);
		pthread_mutex_unlock(&sc->mtx);
		return (USB_ERR_NORMAL_COMPLETION);
	}
	if (req->bmRequestType == UT_WRITE_DEVICE && req->bRequest == UR_SET_ADDRESS) {
		rs_req_clear(r);
		rs_fill(xfer, 0, NULL, 0);
		pthread_mutex_unlock(&sc->mtx);
		return (USB_ERR_NORMAL_COMPLETION);
	}

	out = NULL;
	olen = 0;
	if (!in && length > 0)
		olen = rs_gather(xfer, length, &out);
	r->outlen = olen;
	if (hub.debug)
		rs_log("port %d id %u control %02x %02x %04x %04x %u", rs_port(sc), r->id,
		    req->bmRequestType, req->bRequest, value, index, length);
	if (rs_send(KUSB_CONTROL, rs_port(sc), 0, 0, r->id, req, 8, out, olen) < 0) {
		rs_req_clear(r);
		rs_fill(xfer, 0, NULL, 0);
		free(out);
		pthread_mutex_unlock(&sc->mtx);
		return (USB_ERR_IOERROR);
	}
	free(out);
	pthread_mutex_unlock(&sc->mtx);
	return (USB_PENDING);
}

static int
rs_data(void *scarg, struct usb_data_xfer *xfer, int dir, int epctx)
{
	struct rs_softc *sc = scarg;
	struct rs_req *r;
	uint8_t *out, type;
	uint32_t want, le;
	int in, dci, ret, epaddr;

	in = dir == USB_XFER_IN;
	epaddr = (epctx & 0x0f) | (in ? 0x80 : 0);
	dci = rs_dci(epaddr);

	pthread_mutex_lock(&sc->mtx);
	r = rs_lookup(sc, dci, xfer, in, &ret);
	if (r == NULL) {
		pthread_mutex_unlock(&sc->mtx);
		return (ret);
	}
	type = sc->eptype[(epaddr & 0x0f) + (in ? 16 : 0)];
	if (type == UE_ISOCHRONOUS) {	/* not in phase 1 (docs/66 §4.5) */
		rs_req_clear(r);
		rs_fill(xfer, 0, NULL, 0);
		pthread_mutex_unlock(&sc->mtx);
		return (USB_ERR_IOERROR);
	}
	out = NULL;
	if (in && sc->stash_len[dci] > 0) {	/* what a cancelled read brought */
		want = rs_gather(xfer, RS_MAXMSG, NULL);
		le = rs_fill(xfer, 1, sc->stash[dci], sc->stash_len[dci]);
		memmove(sc->stash[dci], sc->stash[dci] + le, sc->stash_len[dci] - le);
		sc->stash_len[dci] -= le;
		if (hub.debug)
			rs_log("port %d dci %d: %u of %u bytes from kept data", rs_port(sc), dci, le,
			    want);
		rs_req_clear(r);
		pthread_mutex_unlock(&sc->mtx);
		return (USB_ERR_NORMAL_COMPLETION);
	}
	if (in) {
		want = rs_gather(xfer, RS_MAXMSG, NULL);
		le = htole32(want);
		ret = rs_send(type == UE_INTERRUPT ? KUSB_INTR : KUSB_BULK, rs_port(sc),
		    epaddr, 0, r->id, &le, 4, NULL, 0);
	} else {
		want = rs_gather(xfer, RS_MAXMSG, &out);
		r->outlen = want;
		ret = rs_send(type == UE_INTERRUPT ? KUSB_INTR : KUSB_BULK, rs_port(sc),
		    epaddr, 0, r->id, out, want, NULL, 0);
		free(out);
	}
	if (hub.debug)
		rs_log("port %d id %u %s ep %02x %u", rs_port(sc), r->id,
		    type == UE_INTERRUPT ? "intr" : "bulk", epaddr, want);
	if (ret < 0) {
		rs_req_clear(r);
		rs_fill(xfer, 0, NULL, 0);
		pthread_mutex_unlock(&sc->mtx);
		return (USB_ERR_IOERROR);
	}
	pthread_mutex_unlock(&sc->mtx);
	return (USB_PENDING);
}

static void
rs_cancel(void *scarg, struct usb_data_xfer *xfer)
{
	struct rs_softc *sc = scarg;
	struct rs_req *r;
	int i;

	pthread_mutex_lock(&sc->mtx);
	for (i = 1; i < RS_NEPS; i++) {
		r = &sc->rq[i];
		if (!r->active || r->xfer != xfer)
			continue;
		if (!r->done) {
			rs_send(KUSB_CANCEL, rs_port(sc), 0, 0, r->id, NULL, 0, NULL, 0);
			rs_forget(sc, r->id, i);
		} else if (r->status == KUSB_ST_OK)
			rs_stash(sc, i, r->data, r->len);	/* answered, not delivered */
		if (hub.debug)
			rs_log("port %d id %u cancelled", rs_port(sc), r->id);
		rs_req_clear(r);
	}
	pthread_mutex_unlock(&sc->mtx);
}

static void
rs_event(void *scarg, int ev, int arg)
{
	struct rs_softc *sc = scarg;

	pthread_mutex_lock(&sc->mtx);
	if (sc->attached) {
		if (ev == KEEL_UE_PORT_RESET)
			rs_send(KUSB_RESET, rs_port(sc), 0, 0, rs_newid(), NULL, 0, NULL, 0);
		else if (ev == KEEL_UE_EP_RESET)
			rs_send(KUSB_EP_HALT_CLEAR, rs_port(sc), (uint8_t)arg, 0, rs_newid(),
			    NULL, 0, NULL, 0);
	}
	pthread_mutex_unlock(&sc->mtx);
}

/* Address Device: there must be a device on the port */
static int
rs_reset(void *scarg)
{
	struct rs_softc *sc = scarg;
	int ret;

	pthread_mutex_lock(&sc->mtx);
	ret = sc->attached ? 0 : -1;
	pthread_mutex_unlock(&sc->mtx);
	return (ret);
}

static int
rs_noop(void *scarg __unused)
{
	return (0);
}

#ifdef BHYVE_SNAPSHOT
/* a remote device is never in a checkpoint: an "empty" marker only */
static int
rs_snapshot(void *scarg __unused, struct vm_snapshot_meta *meta)
{
	uint32_t empty = 0;
	int ret;

	SNAPSHOT_VAR_OR_LEAVE(empty, meta, ret, done);
done:
	return (ret);
}
#endif

/* ---- the socket thread ---- */

static void
rs_free_descs(struct rs_softc *sc)
{
	int i;

	free(sc->dev);
	for (i = 0; i < sc->ncfg; i++)
		free(sc->cfg[i]);
	free(sc->cfg);
	free(sc->cfg_len);
	free(sc->bos);
	for (i = 0; i < sc->nstr; i++)
		free(sc->str[i].d);
	free(sc->str);
	sc->dev = sc->bos = NULL;
	sc->cfg = NULL;
	sc->cfg_len = NULL;
	sc->str = NULL;
	sc->dev_len = sc->bos_len = sc->ncfg = sc->nstr = 0;
	memset(sc->eptype, 0, sizeof(sc->eptype));
}

static void
rs_detach(struct rs_softc *sc, const char *why)
{
	struct usb_data_xfer *xf[RS_NEPS];
	int i;

	pthread_mutex_lock(&sc->mtx);
	if (!sc->attached) {
		pthread_mutex_unlock(&sc->mtx);
		return;
	}
	sc->attached = false;
	for (i = 1; i < RS_NEPS; i++) {
		xf[i] = NULL;
		if (sc->rq[i].active && !sc->rq[i].done) {
			sc->rq[i].done = true;
			sc->rq[i].status = KUSB_ST_NODEV;
			xf[i] = sc->rq[i].xfer;
		}
	}
	rs_free_descs(sc);
	rs_clear_stash(sc);
	pthread_mutex_unlock(&sc->mtx);
	rs_log("port %d: detached (%s)", rs_port(sc), why);
	for (i = 1; i < RS_NEPS; i++)
		if (xf[i] != NULL)
			sc->hci->hci_complete(sc->hci, i, xf[i]);
	sc->hci->hci_event(sc->hci, USBDEV_REMOVE, NULL);
}

static void
rs_free_counts(int *f2, int *f3)
{
	int i;

	*f2 = *f3 = 0;
	for (i = 0; i < hub.nports; i++) {
		pthread_mutex_lock(&hub.ports[i]->mtx);
		if (!hub.ports[i]->attached) {
			if (hub.ports[i]->usbver == 3)
				(*f3)++;
			else
				(*f2)++;
		}
		pthread_mutex_unlock(&hub.ports[i]->mtx);
	}
}

static void
rs_eptypes(struct rs_softc *sc, const uint8_t *c, int len)
{
	int off, bl;

	for (off = 0; off + 2 <= len; off += bl) {
		bl = c[off];
		if (bl < 2)
			break;
		if (c[off + 1] == UDESC_ENDPOINT && bl >= 7 && off + 7 <= len)
			sc->eptype[(c[off + 2] & 0x0f) + ((c[off + 2] & 0x80) ? 16 : 0)] =
			    c[off + 3] & UE_XFERTYPE;
	}
}

/*
 * UAS (docs/66 §4.9): a USB 3 disk offers USB Attached SCSI (08/06/62) as an alternate setting
 * next to bulk-only (08/06/50). UAS needs streams, which remote endpoints do not have: the
 * guest would pick UAS and hang. The UAS alternate settings (not alternate setting 0) and
 * everything up to the next interface leave the configuration the guest is shown; it uses
 * bulk-only. Returns the new length (wTotalLength is set to it).
 */
static int
rs_strip_uas(uint8_t *c, int len)
{
	int off, bl, out, skip, n;

	out = 0;
	skip = 0;
	n = 0;
	for (off = 0; off + 2 <= len; off += bl) {
		bl = c[off];
		if (bl < 2 || off + bl > len)
			break;
		if (c[off + 1] == UDESC_INTERFACE && bl >= 9)
			skip = c[off + 3] != 0 && c[off + 5] == UICLASS_MASS &&
			    c[off + 6] == 0x06 && c[off + 7] == 0x62;
		else if (c[off + 1] == UDESC_IFACE_ASSOC)
			skip = 0;
		if (skip) {
			n++;
			continue;
		}
		memmove(c + out, c + off, bl);
		out += bl;
	}
	if (n == 0)
		return (len);
	if (out >= 4) {
		c[2] = out & 0xff;
		c[3] = out >> 8;
	}
	rs_log("attach: UAS alternate setting left out (%d descriptors): the guest uses bulk-only",
	    n);
	return (out);
}

static void
rs_attach(const struct kusb_hdr *h, const uint8_t *p, uint32_t len)
{
	struct rs_softc *sc;
	uint32_t off;
	uint16_t dev_len, cfg_count, bos_len, str_count, l, vid, pid;
	uint8_t speed, want;
	int i, kind, usbspeed;

	if (len < 16)
		goto bad;
	speed = p[0];
	want = p[1];
	dev_len = le16dec(p + 4);
	cfg_count = le16dec(p + 6);
	bos_len = le16dec(p + 8);
	str_count = le16dec(p + 10);
	if (speed < KUSB_SPEED_LOW || speed > KUSB_SPEED_SUPER || dev_len < 8 ||
	    cfg_count > 32 || str_count > 255)
		goto bad;
	/* a SuperSpeed device needs a USB 3 port, the others a USB 2 port (docs/66 §4.4) */
	kind = speed == KUSB_SPEED_SUPER ? 3 : 2;
	if (want != 0 && want != kind)
		rs_log("attach: wants a USB %d port, speed %d needs USB %d: USB %d", want, speed,
		    kind, kind);
	sc = NULL;
	for (i = 0; i < hub.nports; i++) {
		pthread_mutex_lock(&hub.ports[i]->mtx);
		if (!hub.ports[i]->attached && hub.ports[i]->usbver == kind)
			sc = hub.ports[i];
		pthread_mutex_unlock(&hub.ports[i]->mtx);
		if (sc != NULL)
			break;
	}
	if (sc == NULL) {
		rs_log("attach: no free USB %d port", kind);
		rs_send(KUSB_COMPLETE, 0, 0, KUSB_ST_ERROR, le32toh(h->id), NULL, 0, NULL, 0);
		return;
	}

	pthread_mutex_lock(&sc->mtx);
	rs_free_descs(sc);
	rs_clear_stash(sc);
	off = 16;
	if (off + dev_len > len)
		goto badl;
	sc->dev = malloc(dev_len);
	memcpy(sc->dev, p + off, dev_len);
	sc->dev_len = dev_len;
	off += dev_len;
	sc->cfg = calloc(cfg_count ? cfg_count : 1, sizeof(*sc->cfg));
	sc->cfg_len = calloc(cfg_count ? cfg_count : 1, sizeof(*sc->cfg_len));
	for (i = 0; i < cfg_count; i++) {
		if (off + 2 > len)
			goto badl;
		l = le16dec(p + off);
		off += 2;
		if (off + l > len)
			goto badl;
		sc->cfg[i] = malloc(l ? l : 1);
		memcpy(sc->cfg[i], p + off, l);
		sc->cfg_len[i] = rs_strip_uas(sc->cfg[i], l);
		sc->ncfg = i + 1;
		rs_eptypes(sc, sc->cfg[i], sc->cfg_len[i]);
		off += l;
	}
	if (bos_len > 0) {
		if (off + bos_len > len)
			goto badl;
		sc->bos = malloc(bos_len);
		memcpy(sc->bos, p + off, bos_len);
		sc->bos_len = bos_len;
		off += bos_len;
	}
	sc->str = calloc(str_count ? str_count : 1, sizeof(*sc->str));
	for (i = 0; i < str_count; i++) {
		if (off + 4 > len)
			goto badl;
		sc->str[i].lang = le16dec(p + off);
		sc->str[i].index = p[off + 2];
		sc->str[i].len = p[off + 3];
		off += 4;
		if (off + sc->str[i].len > len)
			goto badl;
		sc->str[i].d = malloc(sc->str[i].len ? sc->str[i].len : 1);
		memcpy(sc->str[i].d, p + off, sc->str[i].len);
		sc->nstr = i + 1;
		off += sc->str[i].len;
	}
	switch (speed) {
	case KUSB_SPEED_LOW:
		usbspeed = USB_SPEED_LOW;
		break;
	case KUSB_SPEED_FULL:
		usbspeed = USB_SPEED_FULL;
		break;
	case KUSB_SPEED_HIGH:
		usbspeed = USB_SPEED_HIGH;
		break;
	default:
		usbspeed = USB_SPEED_SUPER;
		break;
	}
	sc->speed = usbspeed;
	sc->attached = true;
	vid = sc->dev_len >= 12 ? le16dec(sc->dev + 8) : 0;
	pid = sc->dev_len >= 12 ? le16dec(sc->dev + 10) : 0;
	pthread_mutex_unlock(&sc->mtx);

	rs_log("port %d: attached %04x:%04x, speed %d, %d configuration(s), %d string(s)%s",
	    rs_port(sc), vid, pid, speed, cfg_count, str_count, bos_len ? ", BOS" : "");
	rs_send(KUSB_PORT, (uint8_t)rs_port(sc), 0, 0, le32toh(h->id), NULL, 0, NULL, 0);
	sc->hci->hci_event(sc->hci, USBDEV_ATTACH, &usbspeed);
	return;

badl:
	rs_free_descs(sc);
	pthread_mutex_unlock(&sc->mtx);
bad:
	rs_log("attach: malformed (%u bytes)", len);
	rs_send(KUSB_COMPLETE, 0, 0, KUSB_ST_ERROR, le32toh(h->id), NULL, 0, NULL, 0);
}

static void
rs_complete(const struct kusb_hdr *h, const uint8_t *p, uint32_t len)
{
	struct rs_softc *sc;
	struct rs_req *r;
	struct usb_data_xfer *xfer;
	uint32_t id;
	int i, j;

	id = le32toh(h->id);
	for (i = 0; i < hub.nports; i++) {
		sc = hub.ports[i];
		pthread_mutex_lock(&sc->mtx);
		for (j = 1; j < RS_NEPS; j++) {
			r = &sc->rq[j];
			if (!r->active || r->done || r->id != id)
				continue;
			r->status = h->status;
			r->len = len;
			r->data = len > 0 ? malloc(len) : NULL;
			if (len > 0 && r->data != NULL)
				memcpy(r->data, p, len);
			else
				r->len = 0;
			r->done = true;
			xfer = r->xfer;
			pthread_mutex_unlock(&sc->mtx);
			sc->hci->hci_complete(sc->hci, j, xfer);
			return;
		}
		for (j = 0; j < RS_NGONE && j < sc->ngone; j++)
			if (sc->gone[j].id == id) {
				if (h->status == KUSB_ST_OK)
					rs_stash(sc, sc->gone[j].dci, p, len);
				sc->gone[j].id = 0;
				pthread_mutex_unlock(&sc->mtx);
				return;
			}
		pthread_mutex_unlock(&sc->mtx);
	}
	/* RESET, EP_HALT_CLEAR: nothing waits for it */
}

static void
rs_message(const struct kusb_hdr *h, const uint8_t *p, uint32_t len)
{
	uint32_t v;
	int i, f2, f3;
	uint8_t hello[8];

	switch (h->type) {
	case KUSB_HELLO:
		v = len >= 4 ? le32dec(p) : 0;
		rs_free_counts(&f2, &f3);
		rs_log("source says hello, version %u; free ports: %d USB 2, %d USB 3", v, f2,
		    f3);
		le32enc(hello, KUSB_VERSION);
		hello[4] = (uint8_t)f2;
		hello[5] = (uint8_t)f3;
		hello[6] = hello[7] = 0;
		rs_send(KUSB_HELLO, 0, 0, 0, 0, hello, sizeof(hello), NULL, 0);
		break;
	case KUSB_ATTACH:
		rs_attach(h, p, len);
		break;
	case KUSB_DETACH:
		for (i = 0; i < hub.nports; i++)
			if (rs_port(hub.ports[i]) == h->port)
				rs_detach(hub.ports[i], "DETACH");
		break;
	case KUSB_COMPLETE:
		rs_complete(h, p, len);
		break;
	default:
		break;		/* unknown types are skipped */
	}
}

static void
rs_disconnect(const char *why)
{
	int i;

	pthread_mutex_lock(&hub.omtx);
	if (hub.cfd >= 0)
		close(hub.cfd);
	hub.cfd = -1;
	hub.olen = 0;
	pthread_mutex_unlock(&hub.omtx);
	rs_log("source gone (%s)", why);
	for (i = 0; i < hub.nports; i++)
		rs_detach(hub.ports[i], "source gone");
}

static void *
rs_thread(void *arg __unused)
{
	struct pollfd pfd[3];
	struct kusb_hdr h;
	uint8_t *in, *nb;
	size_t inlen, incap, off;
	ssize_t n;
	uint32_t ml;
	int fd, np, drop;
	char junk[64];

	pthread_set_name_np(pthread_self(), "kusb");
	pthread_mutex_lock(&hub.rmtx);
	while (!hub.ready)
		pthread_cond_wait(&hub.rcv, &hub.rmtx);
	pthread_mutex_unlock(&hub.rmtx);

	incap = 65536;
	in = malloc(incap);
	inlen = 0;
	for (;;) {
		pfd[0].fd = hub.lfd;
		pfd[0].events = POLLIN;
		pfd[1].fd = hub.wake[0];
		pfd[1].events = POLLIN;
		np = 2;
		pthread_mutex_lock(&hub.omtx);
		if (hub.cfd >= 0) {
			pfd[2].fd = hub.cfd;
			pfd[2].events = POLLIN | (hub.olen > 0 ? POLLOUT : 0);
			np = 3;
		}
		pthread_mutex_unlock(&hub.omtx);
		pfd[0].revents = pfd[1].revents = pfd[2].revents = 0;
		if (poll(pfd, np, -1) < 0) {
			if (errno == EINTR)
				continue;
			rs_log("poll: %s", strerror(errno));
			sleep(1);
			continue;
		}
		if (pfd[1].revents & POLLIN)
			while (read(hub.wake[0], junk, sizeof(junk)) > 0)
				;
		if (pfd[0].revents & POLLIN) {
			fd = accept(hub.lfd, NULL, NULL);
			if (fd >= 0) {
				pthread_mutex_lock(&hub.omtx);
				if (hub.cfd >= 0) {
					pthread_mutex_unlock(&hub.omtx);
					rs_log("a second source refused (one connection at a time)");
					close(fd);
				} else {
					fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
					hub.cfd = fd;
					hub.olen = 0;
					pthread_mutex_unlock(&hub.omtx);
					inlen = 0;
					rs_log("source connected");
				}
			}
		}
		if (np < 3)
			continue;
		if (pfd[2].revents & POLLOUT) {
			pthread_mutex_lock(&hub.omtx);
			rs_flush_locked();
			pthread_mutex_unlock(&hub.omtx);
		}
		if ((pfd[2].revents & (POLLIN | POLLHUP | POLLERR)) == 0)
			continue;
		drop = 0;
		for (;;) {
			if (inlen == incap) {
				nb = realloc(in, incap * 2);
				if (nb == NULL)
					break;
				in = nb;
				incap *= 2;
			}
			n = read(pfd[2].fd, in + inlen, incap - inlen);
			if (n > 0) {
				inlen += n;
				continue;
			}
			if (n < 0 && (errno == EAGAIN || errno == EINTR))
				break;
			/* EOF or an error: what is complete first, then drop the source */
			drop = 1;
			break;
		}
		off = 0;
		while (inlen - off >= sizeof(h)) {
			memcpy(&h, in + off, sizeof(h));
			ml = le32toh(h.length);
			if (ml > RS_MAXMSG) {
				rs_disconnect("message too large");
				inlen = off = 0;
				break;
			}
			if (inlen - off < sizeof(h) + ml) {
				if (sizeof(h) + ml > incap) {
					nb = realloc(in, sizeof(h) + ml);
					if (nb != NULL) {
						in = nb;
						incap = sizeof(h) + ml;
					}
				}
				break;
			}
			rs_message(&h, in + off + sizeof(h), ml);
			off += sizeof(h) + ml;
		}
		if (off > 0) {
			memmove(in, in + off, inlen - off);
			inlen -= off;
		}
		if (drop && hub.cfd >= 0) {
			rs_disconnect("EOF");
			inlen = 0;
		}
	}
	return (NULL);
}

void
usb_remote_ready(void)
{
	pthread_mutex_lock(&hub.rmtx);
	hub.ready = true;
	pthread_cond_broadcast(&hub.rcv);
	pthread_mutex_unlock(&hub.rmtx);
}

/* the socket: opened here, before bhyve enters capability mode */
static int
rs_open(void)
{
	struct sockaddr_un sun;
	const char *path;

	path = get_config_value("keel.usb_sock");
	if (path == NULL) {
		EPRINTLN("keel-usb: remote USB ports without -o keel.usb_sock=PATH stay empty");
		return (0);
	}
	if (strlen(path) >= sizeof(sun.sun_path)) {
		EPRINTLN("keel-usb: %s: path too long", path);
		return (-1);
	}
	hub.debug = get_config_bool_default("keel.usb_debug", false) ||
	    access("/var/run/keel-usb-debug", F_OK) == 0;
	hub.lfd = socket(PF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (hub.lfd < 0) {
		EPRINTLN("keel-usb: socket: %s", strerror(errno));
		return (-1);
	}
	memset(&sun, 0, sizeof(sun));
	sun.sun_family = AF_UNIX;
	strlcpy(sun.sun_path, path, sizeof(sun.sun_path));
	sun.sun_len = SUN_LEN(&sun);
	(void)unlink(path);
	if (bind(hub.lfd, (struct sockaddr *)&sun, sizeof(sun)) < 0 ||
	    chmod(path, 0600) < 0 || listen(hub.lfd, 1) < 0) {
		EPRINTLN("keel-usb: %s: %s", path, strerror(errno));
		close(hub.lfd);
		hub.lfd = -1;
		return (-1);
	}
	if (pipe2(hub.wake, O_NONBLOCK | O_CLOEXEC) < 0) {
		EPRINTLN("keel-usb: pipe: %s", strerror(errno));
		return (-1);
	}
	hub.path = strdup(path);
	if (pthread_create(&hub.thr, NULL, rs_thread, NULL) != 0) {
		EPRINTLN("keel-usb: no thread");
		return (-1);
	}
	hub.started = true;
	return (0);
}

static void *
rs_probe(struct usb_hci *hci, nvlist_t *nvl __unused)
{
	struct rs_softc *sc;

	sc = calloc(1, sizeof(*sc));
	if (sc == NULL)
		return (NULL);
	sc->hci = hci;
	pthread_mutex_init(&sc->mtx, NULL);
	return (sc);
}

static int
rs_init(void *scarg)
{
	struct rs_softc *sc = scarg;
	static bool tried;

	sc->usbver = sc->hci->hci_usbver == 3 ? 3 : 2;
	if (hub.nports >= RS_MAXPORTS)
		return (-1);
	hub.ports[hub.nports++] = sc;
	if (!tried) {
		tried = true;
		if (rs_open() != 0)
			return (-1);
	}
	return (0);
}

static struct usb_devemu ue_remote2 = {
	.ue_emu =	"remote2",
	.ue_usbver =	2,
	.ue_usbspeed =	USB_SPEED_HIGH,
	.ue_probe =	rs_probe,
	.ue_init =	rs_init,
	.ue_request =	rs_request,
	.ue_data =	rs_data,
	.ue_reset =	rs_reset,
	.ue_remove =	rs_noop,
	.ue_stop =	rs_noop,
#ifdef BHYVE_SNAPSHOT
	.ue_snapshot =	rs_snapshot,
#endif
	.ue_cancel =	rs_cancel,
	.ue_event =	rs_event,
};
USB_EMUL_SET(ue_remote2);

static struct usb_devemu ue_remote3 = {
	.ue_emu =	"remote3",
	.ue_usbver =	3,
	.ue_usbspeed =	USB_SPEED_SUPER,
	.ue_probe =	rs_probe,
	.ue_init =	rs_init,
	.ue_request =	rs_request,
	.ue_data =	rs_data,
	.ue_reset =	rs_reset,
	.ue_remove =	rs_noop,
	.ue_stop =	rs_noop,
#ifdef BHYVE_SNAPSHOT
	.ue_snapshot =	rs_snapshot,
#endif
	.ue_cancel =	rs_cancel,
	.ue_event =	rs_event,
};
USB_EMUL_SET(ue_remote3);
