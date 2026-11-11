/*
 * keel: virtio-gpu (2D) for bhyve, docs/25.
 *
 *	-s N,virtio-gpu[,w=1024,h=768]		a device of its own (docs/25 step 0)
 *	-s N,fbuf,...,virtio=on			keel's virtio-vga: fbuf with this in BARs 2 and 3
 *						(patch-fbuf.py, keel_vtgpu_attach())
 *
 * A virtio 1.0 ("modern") PCI device, 1af4:1050: bhyve's virtio.c only does the legacy
 * transport, and virtio-gpu has no legacy one (Linux's virtio_gpu and Windows' viogpudo both
 * want modern). The transport is here: the four virtio capabilities in one memory BAR (common
 * config, ISR, device config, notify), MSI-X in another; the virtqueues themselves go through
 * virtio.c's vq_getchain()/vq_relchain()/vq_endchains().
 *
 * The picture: the scanout is copied (converted to 32 bpp BGRX) into a POSIX shared memory
 * object /keel-gpu-<vm name> that keel-rdp maps (struct kgpu_shm below): at every
 * RESOURCE_FLUSH of the scanout's resource `seq` goes up. keel-rdp asks for another size by
 * writing req_width/req_height and bumping req_seq; a thread here looks twice a second, makes
 * that the preferred mode (GET_DISPLAY_INFO) and tells the guest (VIRTIO_GPU_EVENT_DISPLAY,
 * config change interrupt). The cursor (cursor queue) goes to the shm too.
 *
 * Snapshots (keel_vtgpu_snapshot): the transport, the queues, the resources with their backing
 * lists (not their pixels: the guest's RAM comes back with the snapshot, so a restore copies
 * every resource from its backing again), the scanout, the cursor.
 */

#include <sys/param.h>
#include <sys/mman.h>
#include <sys/uio.h>

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <pthread_np.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <machine/atomic.h>
#include <machine/vmm.h>
#ifdef BHYVE_SNAPSHOT
#include <machine/vmm_snapshot.h>
#endif
#include <vmmapi.h>

#include "bhyverun.h"
#include "config.h"
#include "debug.h"
#include "pci_emul.h"
#include "pci_virtio_gpu.h"
#include "virtio.h"

#define	VTGPU_QSIZE	256
#define	VTGPU_MAXW	KGPU_MAXW
#define	VTGPU_MAXH	KGPU_MAXH
#define	VTGPU_RES_MAX	(64u << 20)	/* one resource */
#define	VTGPU_RES_TOTAL	(512u << 20)	/* all resources together */
#define	VTGPU_NIOV	64

/* virtio 1.0 PCI capabilities */
#define	VPCI_CAP_COMMON	1
#define	VPCI_CAP_NOTIFY	2
#define	VPCI_CAP_ISR	3
#define	VPCI_CAP_DEVICE	4
#define	BAR_REGS	2	/* standalone: the four regions, 4 KiB each */
#define	BAR_MSIX	1	/* standalone: MSI-X */
#define	OFF_COMMON	0x0000
#define	OFF_ISR		0x1000
#define	OFF_DEVICE	0x2000
#define	OFF_NOTIFY	0x3000
#define	REGS_SIZE	0x4000

#ifndef VIRTIO_F_VERSION_1
#define	VIRTIO_F_VERSION_1	(1ull << 32)
#endif

/* virtio-gpu */
#define	VIRTIO_GPU_EVENT_DISPLAY	1
enum {
	CMD_GET_DISPLAY_INFO = 0x0100,
	CMD_RESOURCE_CREATE_2D,
	CMD_RESOURCE_UNREF,
	CMD_SET_SCANOUT,
	CMD_RESOURCE_FLUSH,
	CMD_TRANSFER_TO_HOST_2D,
	CMD_RESOURCE_ATTACH_BACKING,
	CMD_RESOURCE_DETACH_BACKING,
	CMD_GET_CAPSET_INFO,
	CMD_GET_CAPSET,
	CMD_GET_EDID,
	CMD_UPDATE_CURSOR = 0x0300,
	CMD_MOVE_CURSOR,
	RESP_OK_NODATA = 0x1100,
	RESP_OK_DISPLAY_INFO,
	RESP_OK_CAPSET_INFO,
	RESP_OK_CAPSET,
	RESP_OK_EDID,
	RESP_ERR_UNSPEC = 0x1200,
	RESP_ERR_OUT_OF_MEMORY,
	RESP_ERR_INVALID_SCANOUT_ID,
	RESP_ERR_INVALID_RESOURCE_ID,
	RESP_ERR_INVALID_CONTEXT_ID,
	RESP_ERR_INVALID_PARAMETER,
};
#define	VIRTIO_GPU_FLAG_FENCE	1
#define	VIRTIO_GPU_F_EDID	(1ull << 1)
#define	EDID_RESP_LEN		(8 + 1024)	/* size, padding, edid[1024] */

struct vgpu_hdr {
	uint32_t type, flags;
	uint64_t fence_id;
	uint32_t ctx_id;
	uint8_t ring_idx, pad[3];
};
struct vgpu_rect {
	uint32_t x, y, width, height;
};

struct vtgpu_backing {
	uint64_t gpa;
	uint32_t len;
	uint8_t *hva;
};

struct vtgpu_res {
	struct vtgpu_res *next;
	uint32_t id, format, width, height;
	uint8_t *data;		/* width * height * 4, the resource's own format */
	struct vtgpu_backing *bk;
	uint32_t nbk;
};

struct vtgpu_softc {
	struct virtio_softc vs;
	int bar_regs, bar_msix;
	struct vqueue_info vq[2];
	pthread_mutex_t mtx;
	uint64_t features;		/* offered */
	uint64_t driver_features;
	uint32_t dfsel, gfsel;		/* feature selects */
	uint8_t cfggen;
	struct {
		uint64_t desc, avail, used;
		uint16_t size;
		uint16_t enabled;
	} q[2];
	uint32_t events_read;
	struct vtgpu_res *res;
	size_t res_bytes;
	uint32_t scanout_res;		/* 0: none */
	struct vgpu_rect scanout;
	struct kgpu_shm *shm;
	uint8_t *pix;
	uint32_t pref_w, pref_h;
	uint32_t req_seen;
};

static int
vtgpu_add_cap(struct pci_devinst *pi, u_char *capdata, int caplen)
{
	/* pci_emul.c's pci_emul_add_capability() is static: the same, here */
	int i, capoff, reallen;
	uint16_t sts;

	reallen = roundup2(caplen, 4);
	sts = pci_get_cfgdata16(pi, PCIR_STATUS);
	capoff = (sts & PCIM_STATUS_CAPPRESENT) == 0 ? 0x40 : pi->pi_capend + 1;
	if (capoff + reallen > PCI_REGMAX + 1)
		return (-1);
	if ((sts & PCIM_STATUS_CAPPRESENT) == 0) {
		pci_set_cfgdata8(pi, PCIR_CAP_PTR, capoff);
		pci_set_cfgdata16(pi, PCIR_STATUS, sts | PCIM_STATUS_CAPPRESENT);
	} else
		pci_set_cfgdata8(pi, pi->pi_prevcap + 1, capoff);
	for (i = 0; i < caplen; i++)
		pci_set_cfgdata8(pi, capoff + i, capdata[i]);
	pci_set_cfgdata8(pi, capoff + 1, 0);
	pi->pi_prevcap = capoff;
	pi->pi_capend = capoff + reallen - 1;
	return (0);
}

static void
vtgpu_add_vcap(struct pci_devinst *pi, int bar, uint8_t type, uint32_t off, uint32_t len, int notify)
{
	u_char c[20];

	memset(c, 0, sizeof(c));
	c[0] = PCIY_VENDOR;
	c[2] = notify ? 20 : 16;	/* cap_len */
	c[3] = type;
	c[4] = bar;
	memcpy(&c[8], &off, 4);
	memcpy(&c[12], &len, 4);
	/* notify_off_multiplier 0: every queue notifies at OFF_NOTIFY, writing its index */
	vtgpu_add_cap(pi, c, c[2]);
}

/* resources */

static struct vtgpu_res *
res_find(struct vtgpu_softc *sc, uint32_t id)
{
	struct vtgpu_res *r;

	for (r = sc->res; r != NULL; r = r->next)
		if (r->id == id)
			return (r);
	return (NULL);
}

static void
res_detach(struct vtgpu_res *r)
{
	free(r->bk);
	r->bk = NULL;
	r->nbk = 0;
}

static void
res_free(struct vtgpu_softc *sc, uint32_t id)
{
	struct vtgpu_res **p, *r;

	for (p = &sc->res; (r = *p) != NULL; p = &r->next)
		if (r->id == id) {
			*p = r->next;
			sc->res_bytes -= (size_t)r->width * r->height * 4;
			res_detach(r);
			free(r->data);
			free(r);
			if (sc->scanout_res == id) {
				sc->scanout_res = 0;
				sc->shm->active = 0;
				sc->shm->seq++;
			}
			return;
		}
}

/* copy n bytes at backing offset off to dst; false if the backing is short */
static bool
backing_read(struct vtgpu_res *r, uint64_t off, uint8_t *dst, size_t n)
{
	uint32_t i;

	for (i = 0; i < r->nbk && n > 0; i++) {
		if (off >= r->bk[i].len) {
			off -= r->bk[i].len;
			continue;
		}
		size_t c = MIN(n, r->bk[i].len - off);
		if (r->bk[i].hva == NULL)
			return (false);
		memcpy(dst, r->bk[i].hva + off, c);
		dst += c;
		n -= c;
		off = 0;
	}
	return (n == 0);
}

/* one pixel of the resource's format to BGRX */
static inline void
to_bgrx(uint32_t fmt, const uint8_t *s, uint8_t *d)
{
	switch (fmt) {
	case 3: case 4:		/* A8R8G8B8, X8R8G8B8: bytes A R G B */
		d[0] = s[3]; d[1] = s[2]; d[2] = s[1];
		break;
	case 67: case 134:	/* R8G8B8A8, R8G8B8X8 */
		d[0] = s[2]; d[1] = s[1]; d[2] = s[0];
		break;
	case 68: case 121:	/* X8B8G8R8, A8B8G8R8 */
		d[0] = s[1]; d[1] = s[2]; d[2] = s[3];
		break;
	default:		/* 1, 2: B8G8R8A8, B8G8R8X8 */
		d[0] = s[0]; d[1] = s[1]; d[2] = s[2];
		break;
	}
	d[3] = 0;
}

/* the part rect (resource coordinates) of the scanout's resource to the shm */
static void
scanout_copy(struct vtgpu_softc *sc, struct vtgpu_res *r, struct vgpu_rect rc)
{
	struct vgpu_rect s = sc->scanout;
	uint32_t x0, y0, x1, y1, x, y;

	x0 = MAX(rc.x, s.x);
	y0 = MAX(rc.y, s.y);
	x1 = MIN(rc.x + rc.width, s.x + s.width);
	y1 = MIN(rc.y + rc.height, s.y + s.height);
	x1 = MIN(x1, r->width);
	y1 = MIN(y1, r->height);
	for (y = y0; y < y1; y++) {
		const uint8_t *src = r->data + ((size_t)y * r->width + x0) * 4;
		uint8_t *dst = sc->pix + ((size_t)(y - s.y) * s.width + (x0 - s.x)) * 4;
		if (r->format == 1 || r->format == 2)
			memcpy(dst, src, (size_t)(x1 - x0) * 4);
		else
			for (x = x0; x < x1; x++, src += 4, dst += 4)
				to_bgrx(r->format, src, dst);
	}
}

/*
 * An EDID 1.4 block for GET_EDID (Windows' viogpudo takes its mode list from it: the
 * established timings, the 8 standard timings and an Established Timings III descriptor;
 * Linux takes the detailed timing as the preferred mode): the preferred mode (keel-rdp's
 * request) as the first detailed timing, the usual desktop sizes as standard timings.
 */
static void
edid_make(uint8_t *e, uint32_t w, uint32_t h)
{
	static const uint16_t std[8][2] = {
		{ 1280, 720 }, { 1280, 800 }, { 1280, 1024 }, { 1440, 900 },
		{ 1600, 900 }, { 1680, 1050 }, { 1920, 1080 }, { 1920, 1200 },
	};
	uint8_t *d;
	uint32_t hb = 160, hfp = 48, hs = 32, vb = MAX(h / 20, 23), vfp = 3, vs = 6;
	uint32_t clk;
	int i, sum;

	memset(e, 0, 128);
	memcpy(e, "\x00\xff\xff\xff\xff\xff\xff\x00", 8);
	e[8] = 0x2c; e[9] = 0xac;	/* manufacturer "KEL" */
	e[10] = 0x01; e[11] = 0x00;	/* product 1 */
	e[16] = 1; e[17] = 2026 - 1990;	/* week, year */
	e[18] = 1; e[19] = 4;		/* EDID 1.4 */
	e[20] = 0xa0;			/* digital, 8 bits per colour */
	e[21] = w * 254 / 960 / 10;	/* size in cm at 96 dpi */
	e[22] = h * 254 / 960 / 10;
	e[23] = 120;			/* gamma 2.2 */
	e[24] = 0x06;			/* sRGB, preferred timing is native */
	/* sRGB chromaticity */
	memcpy(&e[25], "\xee\x91\xa3\x54\x4c\x99\x26\x0f\x50\x54", 10);
	e[35] = 0x21;			/* 640x480@60, 800x600@60 */
	e[36] = 0x08;			/* 1024x768@60 */
	for (i = 0; i < 8; i++) {
		uint16_t x = std[i][0], y = std[i][1];
		e[38 + 2 * i] = x / 8 - 31;
		/* aspect: 00 16:10, 01 4:3, 10 5:4, 11 16:9; 60 Hz */
		e[39 + 2 * i] = (x * 10 == y * 16 ? 0 : x * 3 == y * 4 ? 1 : x * 4 == y * 5 ? 2 : 3) << 6;
	}
	/* 1: the preferred mode, 60 Hz, simple blanking */
	d = &e[54];
	clk = (w + hb) * (h + vb) * 60 / 10000;
	d[0] = clk & 0xff; d[1] = clk >> 8;
	d[2] = w & 0xff; d[3] = hb & 0xff; d[4] = (w >> 8) << 4 | hb >> 8;
	d[5] = h & 0xff; d[6] = vb & 0xff; d[7] = (h >> 8) << 4 | vb >> 8;
	d[8] = hfp; d[9] = hs;
	d[10] = (vfp & 0xf) << 4 | (vs & 0xf);
	d[11] = 0;
	d[12] = e[21] * 10 & 0xff; d[13] = e[22] * 10 & 0xff;
	d[14] = (e[21] * 10 >> 8) << 4 | (e[22] * 10 >> 8);
	d[17] = 0x1e;			/* digital separate sync, +hsync +vsync */
	/*
	 * 2: Established Timings III (VESA E-EDID, bit 7 first): all of them up to 1920x1440
	 * but 1792x1344 and 1856x1392 (byte 10 bits 5-2; the user found them odd in Windows'
	 * list, 2026-09-29)
	 */
	d = &e[72];
	d[3] = 0xf7; d[5] = 0x0a;
	memset(&d[6], 0xff, 5);
	d[10] = 0xff & ~0x3c;
	d[11] = 0xf0;
	/* 3: range limits, 4: name */
	d = &e[90];
	d[3] = 0xfd; d[5] = 50; d[6] = 75; d[7] = 30; d[8] = 160; d[9] = 60; d[10] = 0x01;
	memset(&d[11], 0x20, 7); d[11] = 0x0a;
	d = &e[108];
	d[3] = 0xfc;
	memcpy(&d[5], "keelOS vga\n  ", 13);
	e[126] = 0;
	for (sum = 0, i = 0; i < 127; i++)
		sum += e[i];
	e[127] = (256 - (sum & 0xff)) & 0xff;
}

/* the control queue */

static uint32_t
cmd(struct vtgpu_softc *sc, const uint8_t *req, size_t len, uint8_t *resp, size_t *rlen)
{
	const struct vgpu_hdr *h = (const void *)req;
	struct vtgpu_res *r;
	uint32_t v[8];

	*rlen = sizeof(struct vgpu_hdr);
	if (len < sizeof(*h))
		return (RESP_ERR_UNSPEC);
	req += sizeof(*h);
	len -= sizeof(*h);
	if (sc->shm->debug) {
		uint32_t d[8] = { 0 };
		memcpy(d, req, MIN(len, sizeof(d)));
		EPRINTLN("virtio-gpu: cmd %#x len %zu: %u %u %u %u %u %u %u %u", h->type, len,
		    d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]);
	}
#define	NEED(n)	do { if (len < (n)) return (RESP_ERR_INVALID_PARAMETER); memcpy(v, req, (n)); } while (0)
	switch (h->type) {
	case CMD_GET_DISPLAY_INFO: {
		/* 16 pmodes of {rect, enabled, flags}; one display */
		uint32_t *pm = (uint32_t *)(resp + sizeof(struct vgpu_hdr));
		memset(pm, 0, 16 * 24);
		pm[2] = sc->pref_w;
		pm[3] = sc->pref_h;
		pm[4] = 1;
		*rlen += 16 * 24;
		return (RESP_OK_DISPLAY_INFO);
	}
	case CMD_GET_EDID: {
		/* {scanout, padding} -> {size, padding, edid[1024]} */
		uint32_t *er = (uint32_t *)(resp + sizeof(struct vgpu_hdr));
		NEED(4);
		if (v[0] != 0)
			return (RESP_ERR_INVALID_SCANOUT_ID);
		memset(er, 0, EDID_RESP_LEN);
		er[0] = 128;
		edid_make((uint8_t *)&er[2], sc->pref_w, sc->pref_h);
		*rlen += EDID_RESP_LEN;
		return (RESP_OK_EDID);
	}
	case CMD_RESOURCE_CREATE_2D: {
		NEED(16);	/* resource_id, format, width, height */
		size_t sz = (size_t)v[2] * v[3] * 4;
		if (v[0] == 0 || res_find(sc, v[0]) != NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		if (sz == 0 || sz > VTGPU_RES_MAX || sc->res_bytes + sz > VTGPU_RES_TOTAL)
			return (RESP_ERR_OUT_OF_MEMORY);
		r = calloc(1, sizeof(*r));
		if (r == NULL || (r->data = calloc(1, sz)) == NULL) {
			free(r);
			return (RESP_ERR_OUT_OF_MEMORY);
		}
		r->id = v[0];
		r->format = v[1];
		r->width = v[2];
		r->height = v[3];
		r->next = sc->res;
		sc->res = r;
		sc->res_bytes += sz;
		return (RESP_OK_NODATA);
	}
	case CMD_RESOURCE_UNREF:
		NEED(4);
		if (res_find(sc, v[0]) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		res_free(sc, v[0]);
		return (RESP_OK_NODATA);
	case CMD_RESOURCE_ATTACH_BACKING: {
		NEED(8);	/* resource_id, nr_entries; then {addr, length, padding} each */
		uint32_t i, n = v[1];
		if ((r = res_find(sc, v[0])) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		if (n == 0 || n > 1u << 20 || len < 8 + (size_t)n * 16)
			return (RESP_ERR_INVALID_PARAMETER);
		res_detach(r);
		r->bk = calloc(n, sizeof(*r->bk));
		if (r->bk == NULL)
			return (RESP_ERR_OUT_OF_MEMORY);
		for (i = 0; i < n; i++) {
			memcpy(&r->bk[i].gpa, req + 8 + i * 16, 8);
			memcpy(&r->bk[i].len, req + 8 + i * 16 + 8, 4);
			r->bk[i].hva = paddr_guest2host(sc->vs.vs_pi->pi_vmctx, r->bk[i].gpa, r->bk[i].len);
		}
		r->nbk = n;
		return (RESP_OK_NODATA);
	}
	case CMD_RESOURCE_DETACH_BACKING:
		NEED(4);
		if ((r = res_find(sc, v[0])) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		res_detach(r);
		return (RESP_OK_NODATA);
	case CMD_TRANSFER_TO_HOST_2D: {
		NEED(32);	/* rect, offset (u64), resource_id, padding */
		struct vgpu_rect rc = { v[0], v[1], v[2], v[3] };
		uint64_t off;
		uint32_t y;
		memcpy(&off, &v[4], 8);
		if ((r = res_find(sc, v[6])) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		if (rc.x + rc.width > r->width || rc.y + rc.height > r->height ||
		    rc.x + rc.width < rc.x || rc.y + rc.height < rc.y)
			return (RESP_ERR_INVALID_PARAMETER);
		for (y = 0; y < rc.height; y++) {
			size_t o = ((size_t)(rc.y + y) * r->width + rc.x) * 4;
			if (!backing_read(r, off + (uint64_t)y * r->width * 4, r->data + o,
			    (size_t)rc.width * 4))
				return (RESP_ERR_INVALID_PARAMETER);
		}
		return (RESP_OK_NODATA);
	}
	case CMD_SET_SCANOUT: {
		NEED(24);	/* rect, scanout_id, resource_id */
		struct vgpu_rect rc = { v[0], v[1], v[2], v[3] };
		if (v[4] != 0)
			return (RESP_ERR_INVALID_SCANOUT_ID);
		if (v[5] == 0 || rc.width == 0 || rc.height == 0) {
			sc->scanout_res = 0;
			sc->shm->active = 0;
			sc->shm->seq++;
			return (RESP_OK_NODATA);
		}
		if ((r = res_find(sc, v[5])) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		if (rc.width > VTGPU_MAXW || rc.height > VTGPU_MAXH ||
		    rc.x + rc.width > r->width || rc.y + rc.height > r->height)
			return (RESP_ERR_INVALID_PARAMETER);
		if (sc->scanout_res == 0 || rc.width != sc->scanout.width || rc.height != sc->scanout.height)
			EPRINTLN("virtio-gpu: scanout %ux%u", rc.width, rc.height);
		sc->scanout_res = v[5];
		sc->scanout = rc;
		sc->shm->width = rc.width;
		sc->shm->height = rc.height;
		scanout_copy(sc, r, rc);
		sc->shm->active = 1;
		atomic_thread_fence_rel();
		sc->shm->seq++;
		return (RESP_OK_NODATA);
	}
	case CMD_RESOURCE_FLUSH: {
		NEED(20);	/* rect, resource_id */
		struct vgpu_rect rc = { v[0], v[1], v[2], v[3] };
		if ((r = res_find(sc, v[4])) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		if (v[4] == sc->scanout_res) {
			scanout_copy(sc, r, rc);
			atomic_thread_fence_rel();
			sc->shm->seq++;
		}
		return (RESP_OK_NODATA);
	}
	default:
		EPRINTLN("virtio-gpu: command %#x not done", h->type);
		return (RESP_ERR_UNSPEC);
	}
#undef NEED
}

/* the cursor queue: {hdr, pos {scanout_id, x, y, padding}, resource_id, hot_x, hot_y, padding} */
static void
cursor_cmd(struct vtgpu_softc *sc, const uint8_t *req, size_t len)
{
	const struct vgpu_hdr *h = (const void *)req;
	uint32_t v[8];
	struct vtgpu_res *r;
	uint32_t y, x;

	if (len < sizeof(*h) + 32)
		return;
	memcpy(v, req + sizeof(*h), 32);
	sc->shm->cur_x = (int32_t)v[1];
	sc->shm->cur_y = (int32_t)v[2];
	if (h->type == CMD_UPDATE_CURSOR) {
		r = res_find(sc, v[4]);
		sc->shm->cur_visible = r != NULL;
		sc->shm->cur_hot_x = v[5];
		sc->shm->cur_hot_y = v[6];
		if (r != NULL) {
			uint8_t *d = (uint8_t *)sc->shm + KGPU_CURSOR;
			memset(d, 0, 64 * 64 * 4);
			for (y = 0; y < MIN(r->height, 64u); y++)
				for (x = 0; x < MIN(r->width, 64u); x++) {
					const uint8_t *s = r->data + ((size_t)y * r->width + x) * 4;
					uint8_t *o = d + (y * 64 + x) * 4;
					to_bgrx(r->format, s, o);
					/* keep the alpha: first byte for A8R8G8B8/A8B8G8R8, else last */
					o[3] = (r->format == 3 || r->format == 121) ? s[0] : s[3];
				}
		}
	}
	atomic_thread_fence_rel();
	sc->shm->cur_seq++;
}

static void
vtgpu_queue(struct vtgpu_softc *sc, int qn)
{
	struct vqueue_info *vq = &sc->vq[qn];
	struct iovec iov[VTGPU_NIOV];
	struct vi_req req;
	uint8_t *buf = NULL, resp[sizeof(struct vgpu_hdr) + EDID_RESP_LEN];
	size_t cap = 0;
	int n, i;

	while (vq_has_descs(vq)) {
		n = vq_getchain(vq, iov, VTGPU_NIOV, &req);
		if (n <= 0 || n > VTGPU_NIOV) {
			EPRINTLN("virtio-gpu: bad descriptor chain (%d)", n);
			if (n > 0)
				vq_relchain(vq, req.idx, 0);
			break;
		}
		/* the readable descriptors come first: gather them */
		size_t len = 0;
		for (i = 0; i < req.readable; i++)
			len += iov[i].iov_len;
		if (len > cap) {
			free(buf);
			cap = len;
			buf = malloc(cap);
			if (buf == NULL) {
				cap = 0;
				vq_relchain(vq, req.idx, 0);
				continue;
			}
		}
		size_t o = 0;
		for (i = 0; i < req.readable; i++) {
			memcpy(buf + o, iov[i].iov_base, iov[i].iov_len);
			o += iov[i].iov_len;
		}
		uint32_t used = 0;
		if (qn == 1) {
			cursor_cmd(sc, buf, len);
		} else {
			size_t rlen;
			struct vgpu_hdr *rh = (struct vgpu_hdr *)resp;
			const struct vgpu_hdr *qh = (const void *)buf;
			memset(resp, 0, sizeof(struct vgpu_hdr));
			rh->type = cmd(sc, buf, len, resp, &rlen);
			if (len >= sizeof(*qh) && (qh->flags & VIRTIO_GPU_FLAG_FENCE)) {
				rh->flags = VIRTIO_GPU_FLAG_FENCE;
				rh->fence_id = qh->fence_id;
				rh->ctx_id = qh->ctx_id;
				rh->ring_idx = qh->ring_idx;
			}
			/* the response into the writable descriptors */
			size_t done = 0;
			for (i = req.readable; i < n && done < rlen; i++) {
				size_t c = MIN(iov[i].iov_len, rlen - done);
				memcpy(iov[i].iov_base, resp + done, c);
				done += c;
			}
			used = done;
		}
		vq_relchain(vq, req.idx, used);
	}
	vq_endchains(vq, 1);
	free(buf);
}

/* keel-rdp's size wish: the thread */

static void *
vtgpu_watch(void *arg)
{
	struct vtgpu_softc *sc = arg;

	for (;;) {
		usleep(500000);
		if (sc->shm->debug >= 2) {
			/* the queues' state, to tell a missed notify from a lost interrupt */
			pthread_mutex_lock(&sc->mtx);
			for (int qn = 0; qn < 2; qn++) {
				struct vqueue_info *vq = &sc->vq[qn];
				if (!sc->q[qn].enabled)
					continue;
				EPRINTLN("virtio-gpu: q%d avail %u (flags %#x) done %u used %u (flags %#x) "
				    "msix %u enabled %d isr %#x", qn, vq->vq_avail->idx, vq->vq_avail->flags,
				    vq->vq_last_avail, vq->vq_used->idx, vq->vq_used->flags, vq->vq_msix_idx,
				    pci_msix_enabled(sc->vs.vs_pi), sc->vs.vs_isr);
			}
			pthread_mutex_unlock(&sc->mtx);
		}
		uint32_t s = sc->shm->req_seq;
		if (s == sc->req_seen)
			continue;
		pthread_mutex_lock(&sc->mtx);
		sc->req_seen = s;
		uint32_t w = sc->shm->req_width, h = sc->shm->req_height;
		if (w >= 320 && h >= 200 && w <= VTGPU_MAXW && h <= VTGPU_MAXH &&
		    (w != sc->pref_w || h != sc->pref_h)) {
			sc->pref_w = w & ~7u;	/* 8-pixel columns: text consoles */
			sc->pref_h = h;
			sc->shm->pref_width = sc->pref_w;
			sc->shm->pref_height = sc->pref_h;
			sc->events_read |= VIRTIO_GPU_EVENT_DISPLAY;
			sc->cfggen++;
			EPRINTLN("virtio-gpu: preferred mode %ux%u (keel-rdp)", sc->pref_w, sc->pref_h);
			if (sc->vs.vs_status & 4)	/* DRIVER_OK */
				vi_interrupt(&sc->vs, VIRTIO_PCI_ISR_CONFIG, sc->vs.vs_msix_cfg_idx);
		}
		pthread_mutex_unlock(&sc->mtx);
	}
	return (NULL);
}

/* the transport */

static void
vtgpu_reset(struct vtgpu_softc *sc)
{
	while (sc->res != NULL)
		res_free(sc, sc->res->id);
	sc->scanout_res = 0;
	sc->shm->active = 0;
	sc->shm->cur_visible = 0;
	sc->shm->seq++;
	sc->shm->cur_seq++;
	memset(sc->q, 0, sizeof(sc->q));
	sc->q[0].size = sc->q[1].size = VTGPU_QSIZE;
	sc->driver_features = 0;
	sc->events_read = 0;
	vi_reset_dev(&sc->vs);
	/*
	 * vi_reset_dev() leaves vs_status alone (legacy virtio stores the written 0 itself).
	 * A modern driver polls device_status until it reads 0 after the reset: viogpudo does,
	 * forever, on D3 (hibernate, restart) and in its bugcheck display callback (docs/13).
	 */
	sc->vs.vs_status = 0;
}

static void
vtgpu_enable_queue(struct vtgpu_softc *sc, int qn)
{
	struct vqueue_info *vq = &sc->vq[qn];
	struct vmctx *ctx = sc->vs.vs_pi->pi_vmctx;
	uint16_t qs = sc->q[qn].size;

	vq->vq_qsize = qs;
	vq->vq_desc = paddr_guest2host(ctx, sc->q[qn].desc, 16 * qs);
	vq->vq_avail = paddr_guest2host(ctx, sc->q[qn].avail, 6 + 2 * qs);
	vq->vq_used = paddr_guest2host(ctx, sc->q[qn].used, 6 + 8 * qs);
	if (vq->vq_desc == NULL || vq->vq_avail == NULL || vq->vq_used == NULL) {
		EPRINTLN("virtio-gpu: queue %d outside guest memory", qn);
		return;
	}
	vq->vq_flags = VQ_ALLOC;
	vq->vq_last_avail = vq->vq_next_used = vq->vq_save_used = 0;
	sc->q[qn].enabled = 1;
}

static uint64_t
common_read(struct vtgpu_softc *sc, uint64_t off, int size)
{
	int qs = sc->vs.vs_curq;
	uint64_t v;

	switch (off & ~3ul) {
	case 0x00: v = sc->dfsel; break;
	case 0x04: v = sc->dfsel == 0 ? (uint32_t)sc->features :
	    sc->dfsel == 1 ? (uint32_t)(sc->features >> 32) : 0; break;
	case 0x08: v = sc->gfsel; break;
	case 0x0c: v = sc->gfsel == 0 ? (uint32_t)sc->driver_features :
	    sc->gfsel == 1 ? (uint32_t)(sc->driver_features >> 32) : 0; break;
	case 0x10: v = sc->vs.vs_msix_cfg_idx | 2u << 16; break;	/* msix_config, num_queues */
	case 0x14: v = sc->vs.vs_status | (uint32_t)sc->cfggen << 8 | (uint32_t)qs << 16; break;
	case 0x18: v = (qs < 2 ? sc->q[qs].size : 0) |
	    (uint32_t)(qs < 2 ? sc->vq[qs].vq_msix_idx : VIRTIO_MSI_NO_VECTOR) << 16; break;
	case 0x1c: v = qs < 2 ? sc->q[qs].enabled : 0; break;	/* queue_notify_off 0 */
	case 0x20: case 0x24: v = qs < 2 ? sc->q[qs].desc >> (off & 4 ? 32 : 0) : 0; break;
	case 0x28: case 0x2c: v = qs < 2 ? sc->q[qs].avail >> (off & 4 ? 32 : 0) : 0; break;
	case 0x30: case 0x34: v = qs < 2 ? sc->q[qs].used >> (off & 4 ? 32 : 0) : 0; break;
	default: return (0);
	}
	v >>= (off & 3) * 8;
	return (size == 4 ? (uint32_t)v : size == 2 ? (uint16_t)v : (uint8_t)v);
}

static void
set_part(uint64_t *field, uint64_t off, int size, uint64_t value)
{
	int sh = (off & 7) * 8;
	uint64_t mask = (size == 8 ? ~0ull : ((1ull << (size * 8)) - 1)) << sh;

	*field = (*field & ~mask) | ((value << sh) & mask);
}

static void
common_write(struct vtgpu_softc *sc, uint64_t off, int size, uint64_t value)
{
	int qs = sc->vs.vs_curq;

	switch (off) {
	case 0x00: sc->dfsel = value; break;
	case 0x08: sc->gfsel = value; break;
	case 0x0c:
		if (sc->gfsel < 2) {
			uint64_t m = 0xffffffffull << (32 * sc->gfsel);
			sc->driver_features = (sc->driver_features & ~m) |
			    (((uint64_t)(uint32_t)value << (32 * sc->gfsel)) & sc->features);
			sc->vs.vs_negotiated_caps = (uint32_t)sc->driver_features;
		}
		break;
	case 0x10: sc->vs.vs_msix_cfg_idx = value; break;
	case 0x14:
		if ((value & 0xff) == 0)
			vtgpu_reset(sc);
		else
			sc->vs.vs_status = value;
		break;
	case 0x16: sc->vs.vs_curq = value; break;
	case 0x18:
		if (qs < 2 && value > 0 && value <= VTGPU_QSIZE && powerof2(value))
			sc->q[qs].size = value;
		break;
	case 0x1a:
		if (qs < 2)
			sc->vq[qs].vq_msix_idx = value;
		break;
	case 0x1c:
		if (qs < 2 && value == 1)
			vtgpu_enable_queue(sc, qs);
		break;
	default:
		if (qs < 2 && off >= 0x20 && off < 0x38) {
			uint64_t *f = off < 0x28 ? &sc->q[qs].desc :
			    off < 0x30 ? &sc->q[qs].avail : &sc->q[qs].used;
			set_part(f, off, size, value);
		}
		break;
	}
}

void
keel_vtgpu_barwrite(void *arg, struct pci_devinst *pi, int baridx, uint64_t off, int size,
    uint64_t value)
{
	struct vtgpu_softc *sc = arg;

	if (baridx == pci_msix_table_bar(pi) || baridx == pci_msix_pba_bar(pi)) {
		pci_emul_msix_twrite(pi, off, size, value);
		return;
	}
	if (baridx != sc->bar_regs)
		return;
	pthread_mutex_lock(&sc->mtx);
	if (off < OFF_ISR) {
		common_write(sc, off, size, value);
	} else if (off >= OFF_DEVICE && off < OFF_DEVICE + 16) {
		if (off == OFF_DEVICE + 4)	/* events_clear */
			sc->events_read &= ~(uint32_t)value;
	} else if (off >= OFF_NOTIFY && off < OFF_NOTIFY + 4) {
		int qn = value & 0xffff;
		if (qn < 2 && sc->q[qn].enabled)
			vtgpu_queue(sc, qn);
	}
	pthread_mutex_unlock(&sc->mtx);
}

uint64_t
keel_vtgpu_barread(void *arg, struct pci_devinst *pi, int baridx, uint64_t off, int size)
{
	struct vtgpu_softc *sc = arg;
	uint64_t v = 0;

	if (baridx == pci_msix_table_bar(pi) || baridx == pci_msix_pba_bar(pi))
		return (pci_emul_msix_tread(pi, off, size));
	if (baridx != sc->bar_regs)
		return (0);
	pthread_mutex_lock(&sc->mtx);
	if (off < OFF_ISR) {
		v = common_read(sc, off, size);
	} else if (off == OFF_ISR) {
		v = sc->vs.vs_isr;	/* read clears */
		sc->vs.vs_isr = 0;
		if (v)
			pci_lintr_deassert(pi);
	} else if (off >= OFF_DEVICE && off < OFF_DEVICE + 16) {
		/* events_read, events_clear, num_scanouts, num_capsets */
		uint32_t cfg[4] = { sc->events_read, 0, 1, 0 };
		uint64_t o = off - OFF_DEVICE;
		memcpy(&v, (uint8_t *)cfg + o, MIN((uint64_t)size, 16 - o));
	}
	pthread_mutex_unlock(&sc->mtx);
	return (v);
}

/* only the name and the queue count are used: the transport is ours */
static struct virtio_consts vtgpu_vi_consts = {
	.vc_name = "vtgpu",
	.vc_nvq = 2,
};

/*
 * The virtio-gpu part on pi: MSI-X in BAR bar_msix, the virtio regions in BAR bar_regs, the
 * shm, the thread. The caller sets the PCI IDs and keeps pi->pi_arg; NULL on failure.
 */
void *
keel_vtgpu_attach(struct pci_devinst *pi, nvlist_t *nvl, int bar_regs, int bar_msix)
{
	struct vtgpu_softc *sc;
	const char *name, *v;
	char shmname[64];
	void *arg = pi->pi_arg;
	pthread_t tid;
	int fd;

	sc = calloc(1, sizeof(*sc));
	pthread_mutex_init(&sc->mtx, NULL);
	sc->bar_regs = bar_regs;
	sc->bar_msix = bar_msix;
	sc->pref_w = 1024;
	sc->pref_h = 768;
	if ((v = get_config_value_node(nvl, "w")) != NULL)
		sc->pref_w = MIN(MAX(atoi(v), 320), VTGPU_MAXW);
	if ((v = get_config_value_node(nvl, "h")) != NULL)
		sc->pref_h = MIN(MAX(atoi(v), 200), VTGPU_MAXH);

	/* the shm for keel-rdp, made before bhyve enters capability mode */
	name = get_config_value("name");
	snprintf(shmname, sizeof(shmname), "/keel-gpu-%s", name != NULL ? name : "vm");
	shm_unlink(shmname);
	fd = shm_open(shmname, O_RDWR | O_CREAT | O_EXCL, 0600);
	if (fd < 0 || ftruncate(fd, KGPU_SIZE) != 0) {
		EPRINTLN("virtio-gpu: %s: %s", shmname, strerror(errno));
		return (NULL);
	}
	sc->shm = mmap(NULL, KGPU_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	close(fd);
	if (sc->shm == MAP_FAILED) {
		EPRINTLN("virtio-gpu: mmap: %s", strerror(errno));
		return (NULL);
	}
	sc->pix = (uint8_t *)sc->shm + KGPU_PIXELS;
	sc->shm->magic = KGPU_MAGIC;
	sc->shm->pref_width = sc->pref_w;
	sc->shm->pref_height = sc->pref_h;

	sc->features = VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_EDID;
	vi_softc_linkup(&sc->vs, &vtgpu_vi_consts, sc, pi, sc->vq);
	pi->pi_arg = arg;	/* vi_softc_linkup() takes it: it stays the caller's */
	sc->vs.vs_mtx = &sc->mtx;
	sc->q[0].size = sc->q[1].size = VTGPU_QSIZE;

	/*
	 * MSI-X: config + 2 queues; and INTx like virtio.c. The MSI capability is the caller's
	 * (fbuf has one already).
	 */
	sc->vs.vs_flags |= VIRTIO_USE_MSIX;
	pthread_mutex_lock(&sc->mtx);
	vtgpu_reset(sc);
	pthread_mutex_unlock(&sc->mtx);
	if (pci_emul_add_msixcap(pi, 3, bar_msix))
		return (NULL);
	pci_lintr_request(pi);

	pci_emul_alloc_bar(pi, bar_regs, PCIBAR_MEM32, REGS_SIZE);
	vtgpu_add_vcap(pi, bar_regs, VPCI_CAP_COMMON, OFF_COMMON, 0x38, 0);
	vtgpu_add_vcap(pi, bar_regs, VPCI_CAP_ISR, OFF_ISR, 1, 0);
	vtgpu_add_vcap(pi, bar_regs, VPCI_CAP_DEVICE, OFF_DEVICE, 16, 0);
	vtgpu_add_vcap(pi, bar_regs, VPCI_CAP_NOTIFY, OFF_NOTIFY, 4, 1);

	pthread_create(&tid, NULL, vtgpu_watch, sc);
	pthread_set_name_np(tid, "virtio-gpu watch");
	return (sc);
}

int
keel_vtgpu_owns(void *arg, int baridx)
{
	struct vtgpu_softc *sc = arg;

	return (baridx == sc->bar_regs || baridx == sc->bar_msix);
}

/*
 * The scanout while the guest's driver shows one (BGRX, stride = w * 4): for fbuf's
 * console image (VNC, keel-rdp's QUERY). 0 without one.
 */
int
keel_vtgpu_scanout(void *arg, void **pix, int *w, int *h)
{
	struct vtgpu_softc *sc = arg;

	if (!sc->shm->active)
		return (0);
	*pix = sc->pix;
	*w = sc->shm->width;
	*h = sc->shm->height;
	return (1);
}

/* standalone: -s N,virtio-gpu */

static void
vtgpu_write(struct pci_devinst *pi, int baridx, uint64_t off, int size, uint64_t value)
{
	keel_vtgpu_barwrite(pi->pi_arg, pi, baridx, off, size, value);
}

static uint64_t
vtgpu_read(struct pci_devinst *pi, int baridx, uint64_t off, int size)
{
	return (keel_vtgpu_barread(pi->pi_arg, pi, baridx, off, size));
}

static int
vtgpu_init(struct pci_devinst *pi, nvlist_t *nvl)
{
	void *sc;

	pci_set_cfgdata16(pi, PCIR_VENDOR, 0x1af4);
	pci_set_cfgdata16(pi, PCIR_DEVICE, 0x1050);
	pci_set_cfgdata8(pi, PCIR_REVID, 1);
	pci_set_cfgdata8(pi, PCIR_CLASS, PCIC_DISPLAY);
	pci_set_cfgdata8(pi, PCIR_SUBCLASS, PCIS_DISPLAY_OTHER);
	pci_set_cfgdata16(pi, PCIR_SUBVEND_0, 0x1af4);
	pci_set_cfgdata16(pi, PCIR_SUBDEV_0, 0x1100);
	pci_emul_add_msicap(pi, 1);
	sc = keel_vtgpu_attach(pi, nvl, BAR_REGS, BAR_MSIX);
	if (sc == NULL)
		return (1);
	pi->pi_arg = sc;
	return (0);
}

#ifdef BHYVE_SNAPSHOT
/*
 * Save or restore the virtio-gpu part. The PCI config space, BARs and MSI-X table are
 * pci_emul's. See the top of the file for what is kept.
 */
int
keel_vtgpu_snapshot(void *arg, struct vm_snapshot_meta *meta)
{
	struct vtgpu_softc *sc = arg;
	struct vmctx *ctx = sc->vs.vs_pi->pi_vmctx;
	struct vtgpu_res *r, **tail;
	uint32_t ver = 1, nres = 0, i, y;
	int ret, qn;

	pthread_mutex_lock(&sc->mtx);
	SNAPSHOT_VAR_CMP_OR_LEAVE(ver, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->vs.vs_negotiated_caps, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->vs.vs_curq, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->vs.vs_status, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->vs.vs_isr, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->vs.vs_msix_cfg_idx, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->driver_features, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->dfsel, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->gfsel, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->cfggen, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->events_read, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->pref_w, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->pref_h, meta, ret, done);
	for (qn = 0; qn < 2; qn++) {
		struct vqueue_info *vq = &sc->vq[qn];

		SNAPSHOT_VAR_OR_LEAVE(sc->q[qn].desc, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(sc->q[qn].avail, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(sc->q[qn].used, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(sc->q[qn].size, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(sc->q[qn].enabled, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(vq->vq_last_avail, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(vq->vq_next_used, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(vq->vq_save_used, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(vq->vq_msix_idx, meta, ret, done);
	}
	SNAPSHOT_VAR_OR_LEAVE(sc->scanout_res, meta, ret, done);
	SNAPSHOT_BUF_OR_LEAVE(&sc->scanout, sizeof(sc->scanout), meta, ret, done);
	/* cur_visible, cur_x, cur_y, cur_hot_x, cur_hot_y: one after the other in the shm */
	SNAPSHOT_BUF_OR_LEAVE((void *)(uintptr_t)&sc->shm->cur_visible, 5 * sizeof(uint32_t), meta,
	    ret, done);
	SNAPSHOT_BUF_OR_LEAVE((uint8_t *)sc->shm + KGPU_CURSOR, 64 * 64 * 4, meta, ret, done);

	if (meta->op == VM_SNAPSHOT_SAVE) {
		for (r = sc->res; r != NULL; r = r->next)
			nres++;
		SNAPSHOT_VAR_OR_LEAVE(nres, meta, ret, done);
		for (r = sc->res; r != NULL; r = r->next) {
			SNAPSHOT_VAR_OR_LEAVE(r->id, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(r->format, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(r->width, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(r->height, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(r->nbk, meta, ret, done);
			for (i = 0; i < r->nbk; i++) {
				SNAPSHOT_VAR_OR_LEAVE(r->bk[i].gpa, meta, ret, done);
				SNAPSHOT_VAR_OR_LEAVE(r->bk[i].len, meta, ret, done);
			}
		}
		goto done;
	}

	/* restore: the queues' rings, the resources, the scanout */
	while (sc->res != NULL)
		res_free(sc, sc->res->id);
	for (qn = 0; qn < 2; qn++) {
		struct vqueue_info *vq = &sc->vq[qn];
		uint16_t qs = sc->q[qn].size;

		vq->vq_qsize = qs;
		vq->vq_flags = 0;
		if (!sc->q[qn].enabled)
			continue;
		vq->vq_desc = paddr_guest2host(ctx, sc->q[qn].desc, 16 * qs);
		vq->vq_avail = paddr_guest2host(ctx, sc->q[qn].avail, 6 + 2 * qs);
		vq->vq_used = paddr_guest2host(ctx, sc->q[qn].used, 6 + 8 * qs);
		if (vq->vq_desc == NULL || vq->vq_avail == NULL || vq->vq_used == NULL) {
			EPRINTLN("virtio-gpu: restore: queue %d outside guest memory", qn);
			ret = EINVAL;
			goto done;
		}
		vq->vq_flags = VQ_ALLOC;
	}
	SNAPSHOT_VAR_OR_LEAVE(nres, meta, ret, done);
	tail = &sc->res;
	for (; nres > 0; nres--) {
		size_t sz;

		r = calloc(1, sizeof(*r));
		if (r == NULL) {
			ret = ENOMEM;
			goto done;
		}
		SNAPSHOT_VAR_OR_LEAVE(r->id, meta, ret, fail);
		SNAPSHOT_VAR_OR_LEAVE(r->format, meta, ret, fail);
		SNAPSHOT_VAR_OR_LEAVE(r->width, meta, ret, fail);
		SNAPSHOT_VAR_OR_LEAVE(r->height, meta, ret, fail);
		SNAPSHOT_VAR_OR_LEAVE(r->nbk, meta, ret, fail);
		sz = (size_t)r->width * r->height * 4;
		if (sz == 0 || sz > VTGPU_RES_MAX || r->nbk > 1u << 20 ||
		    (r->data = calloc(1, sz)) == NULL ||
		    (r->nbk > 0 && (r->bk = calloc(r->nbk, sizeof(*r->bk))) == NULL)) {
			ret = ENOMEM;
			goto fail;
		}
		for (i = 0; i < r->nbk; i++) {
			SNAPSHOT_VAR_OR_LEAVE(r->bk[i].gpa, meta, ret, fail);
			SNAPSHOT_VAR_OR_LEAVE(r->bk[i].len, meta, ret, fail);
			r->bk[i].hva = paddr_guest2host(ctx, r->bk[i].gpa, r->bk[i].len);
		}
		/* the pixels from the backing (stride = width * 4, as the guest transfers them) */
		for (y = 0; y < r->height && r->nbk > 0; y++)
			if (!backing_read(r, (uint64_t)y * r->width * 4,
			    r->data + (size_t)y * r->width * 4, (size_t)r->width * 4))
				break;
		sc->res_bytes += sz;
		*tail = r;
		tail = &r->next;
		continue;
fail:
		free(r->bk);
		free(r->data);
		free(r);
		goto done;
	}
	if (sc->scanout_res != 0 && (r = res_find(sc, sc->scanout_res)) != NULL) {
		sc->shm->width = sc->scanout.width;
		sc->shm->height = sc->scanout.height;
		scanout_copy(sc, r, sc->scanout);
		sc->shm->active = 1;
	} else {
		sc->scanout_res = 0;
		sc->shm->active = 0;
	}
	sc->shm->pref_width = sc->pref_w;
	sc->shm->pref_height = sc->pref_h;
	sc->shm->seq++;
	sc->shm->cur_seq++;
	if (sc->vs.vs_isr != 0 && !pci_msix_enabled(sc->vs.vs_pi))
		pci_lintr_assert(sc->vs.vs_pi);
	EPRINTLN("virtio-gpu: restored (scanout %ux%u)", sc->scanout.width, sc->scanout.height);
done:
	pthread_mutex_unlock(&sc->mtx);
	return (ret);
}

static int
vtgpu_pci_snapshot(struct vm_snapshot_meta *meta)
{
	struct pci_devinst *pi = meta->dev_data;

	return (keel_vtgpu_snapshot(pi->pi_arg, meta));
}
#endif

static const struct pci_devemu pci_de_vtgpu = {
	.pe_emu =	"virtio-gpu",
	.pe_init =	vtgpu_init,
	.pe_barwrite =	vtgpu_write,
	.pe_barread =	vtgpu_read,
#ifdef BHYVE_SNAPSHOT
	.pe_snapshot =	vtgpu_pci_snapshot,
#endif
};
PCI_EMUL_SET(pci_de_vtgpu);
