/*
 * virtio-gpu 3D on virglrenderer (see pci_virtio_gpu_3d.h). macOS only: blobs are put in the
 * guest's address space with Hypervisor.framework directly.
 *
 * What the guest's driver (Triton's viogpu3d) uses and where it goes:
 *
 *	GET_CAPSET_INFO, GET_CAPSET		virgl_renderer_get_cap_set / fill_caps
 *	CTX_CREATE/DESTROY, ATTACH/DETACH	a renderer context per guest process; the context
 *						lives in a render server process of its own
 *	SUBMIT_3D				virgl_renderer_submit_cmd (for Neptune: little,
 *						the calls go through a ring in a blob)
 *	RESOURCE_CREATE_BLOB, MAP/UNMAP_BLOB	host memory in the window (hv_vm_map)
 *	a fence with a ring index		answered when the renderer retires it
 *	SET_SCANOUT_BLOB, RESOURCE_FLUSH	the blob is a shared memory object with linear
 *						pixels: copied to the 2D device's picture
 *
 * No vrend (OpenGL): VIRGL_RENDERER_NO_VIRGL, so RESOURCE_CREATE_3D is an error. The guest's
 * driver wants a virgl context for one thing all the same: a present that is a copy (a
 * Direct3D program in a window that does not flip: the picture goes from its back buffer to
 * the surface the desktop's compositor made for the window). Without one it gives up, and
 * such windows are black. So there is a VIRGL capset, and a context made with it is ours (the
 * "blit context" below): the few commands the driver sends it are done here, on the pixels,
 * which both surfaces have in shared memory.
 *
 * Threads: commands come on a vCPU thread with the device's lock held, which also keeps the
 * calls into virglrenderer one at a time. The renderer retires fences on a thread of its own
 * (VIRGL_RENDERER_ASYNC_FENCE_CB): that only queues them, and fence_thread() completes the
 * commands with the device's lock, so the renderer is never called back into from under it.
 *
 * Not done: snapshots (a 3D guest cannot be saved), more than one scanout, the cursor of a
 * blob resource.
 */

#include <sys/param.h>
#include <sys/mman.h>
#include <sys/queue.h>
#include <sys/stat.h>
#include <sys/uio.h>

#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <Hypervisor/Hypervisor.h>
#include <virglrenderer.h>

#include "bhyverun.h"
#include "debug.h"
#include "pci_emul.h"
#include "pci_virtio_gpu_3d.h"

/* virtio-gpu (virtio 1.3, 5.7) */
enum {
	CMD_RESOURCE_UNREF = 0x0102,
	CMD_SET_SCANOUT = 0x0103,
	CMD_RESOURCE_FLUSH = 0x0104,
	CMD_RESOURCE_ATTACH_BACKING = 0x0106,
	CMD_RESOURCE_DETACH_BACKING = 0x0107,
	CMD_GET_CAPSET_INFO = 0x0108,
	CMD_GET_CAPSET = 0x0109,
	CMD_RESOURCE_ASSIGN_UUID = 0x010b,
	CMD_RESOURCE_CREATE_BLOB = 0x010c,
	CMD_SET_SCANOUT_BLOB = 0x010d,
	CMD_CTX_CREATE = 0x0200,
	CMD_CTX_DESTROY,
	CMD_CTX_ATTACH_RESOURCE,
	CMD_CTX_DETACH_RESOURCE,
	CMD_RESOURCE_CREATE_3D,
	CMD_TRANSFER_TO_HOST_3D,
	CMD_TRANSFER_FROM_HOST_3D,
	CMD_SUBMIT_3D,
	CMD_RESOURCE_MAP_BLOB,
	CMD_RESOURCE_UNMAP_BLOB,
	RESP_OK_NODATA = 0x1100,
	RESP_OK_CAPSET_INFO = 0x1102,
	RESP_OK_CAPSET = 0x1103,
	RESP_OK_MAP_INFO = 0x1106,
	RESP_ERR_UNSPEC = 0x1200,
	RESP_ERR_OUT_OF_MEMORY,
	RESP_ERR_INVALID_SCANOUT_ID,
	RESP_ERR_INVALID_RESOURCE_ID,
	RESP_ERR_INVALID_CONTEXT_ID,
	RESP_ERR_INVALID_PARAMETER,
};
#define	VIRTIO_GPU_FLAG_FENCE		(1u << 0)
#define	VIRTIO_GPU_FLAG_INFO_RING_IDX	(1u << 1)
#define	VIRTIO_GPU_F_VIRGL		(1ull << 0)
#define	VIRTIO_GPU_F_RESOURCE_BLOB	(1ull << 3)
#define	VIRTIO_GPU_F_CONTEXT_INIT	(1ull << 4)
#define	VIRTIO_GPU_BLOB_MEM_HOST3D	2
#define	VIRTIO_GPU_SHM_ID_HOST_VISIBLE	1
#define	VIRTIO_PCI_CAP_SHARED_MEMORY_CFG 8
#define	CAPSET_VIRGL	1
#define	CAPSET_VIRGL2	2
#define	CAPSET_VENUS	4
#define	CAPSET_NEPTUNE	7
/* sizeof(struct virgl_caps_v1) (virgl_hw.h): what the VIRGL capset is said to be long */
#define	VIRGL_CAPS_V1_SIZE	308
/* virgl's commands that the driver's copy present sends (virgl_protocol.h) */
#define	VIRGL_CCMD_RESOURCE_COPY_REGION		17
#define	VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE	49
#define	MAX_BLT_CTX	32
#define	MAX_CAPSETS	4
#define	GUEST_PAGE	4096u		/* the stage-2 granule (vmmapi_hvf.c asks for 4 KiB) */

struct vgpu_hdr {
	uint32_t type, flags;
	uint64_t fence_id;
	uint32_t ctx_id;
	uint8_t ring_idx, pad[3];
};
#define	HDR	sizeof(struct vgpu_hdr)

struct res3d {
	LIST_ENTRY(res3d) link;
	uint32_t id;
	uint32_t ctx;			/* the context that made it (a blob) */
	struct iovec *iov;		/* the guest's pages of a blob with guest memory */
	uint32_t niov;
	void *map;			/* in the window: host address, offset, length */
	uint64_t map_off, map_len;
	uint8_t *pix;			/* a scanout blob's shared memory (linear pixels), mapped */
	size_t pix_len;
	/* the blit context's: the pixels to write as well, and what the driver said they are */
	uint8_t *rw;
	size_t rw_len;
	uint32_t t_format, t_width, t_height, t_stride, t_offset;
	/*
	 * Not a blob: a texture of the blit context's own (RESOURCE_CREATE_3D), its pixels in
	 * memory of this process (rw), the guest's copy in pages attached later (iov). The
	 * driver makes these for surfaces Windows reads on the CPU: the redirection surface
	 * of a window that is presented by copies, the primary before the compositor runs.
	 */
	int classic;
};

struct fence {
	STAILQ_ENTRY(fence) link;
	uint32_t ctx_id, ring_idx;
	uint64_t fence_id;
	uint64_t token;			/* pending only */
};
STAILQ_HEAD(fenceq, fence);

struct vtgpu3d {
	struct pci_devinst *pi;
	struct vtgpu3d_host host;
	int bar;
	uint64_t hostmem;		/* the window's size */
	uint64_t base;			/* its guest address; 0: not decoded */
	uint32_t capsets[MAX_CAPSETS];
	uint32_t ncapsets;
	LIST_HEAD(, res3d) res;
	struct fenceq pending;		/* deferred commands; the device's lock */
	pthread_mutex_t fmtx;		/* retired, and fcond */
	pthread_cond_t fcond;
	struct fenceq retired;
	/* the scanout: a blob with its shared memory mapped (res3d.pix) */
	uint32_t so_res;
	uint8_t *so_map;
	uint32_t so_format, so_width, so_height, so_stride, so_offset;
	/*
	 * The scanout is shown when its picture is whole: so_late while the context that drew
	 * it still has fences under way that were there when the guest flipped to it (up to
	 * so_fence), since so_late_ms (scanout_show).
	 */
	int so_late, no_late;		/* no_late: VTGPU3D_LATE=0, every picture at once as before */
	uint64_t so_fence, so_late_ms;
	int debug;
	/* the blit context(s): contexts the guest made with the VIRGL capset */
	int blt;
	uint32_t blt_ctx[MAX_BLT_CTX];
	uint32_t nblt;
	uint64_t blt_copies, blt_said;
};

static struct res3d *
res_find(struct vtgpu3d *g, uint32_t id)
{
	struct res3d *r;

	LIST_FOREACH(r, &g->res, link)
		if (r->id == id)
			return (r);
	return (NULL);
}

/* the window */

static void
blob_unmap_guest(struct vtgpu3d *g, struct res3d *r)
{
	if (g->base != 0 && hv_vm_unmap(g->base + r->map_off, r->map_len) != HV_SUCCESS)
		EPRINTLN("virtio-gpu 3d: hv_vm_unmap(%#llx, %#llx) failed",
		    (unsigned long long)(g->base + r->map_off), (unsigned long long)r->map_len);
}

static int
blob_map_guest(struct vtgpu3d *g, struct res3d *r)
{
	hv_return_t hr;

	if (g->base == 0)
		return (0);	/* mapped when the BAR is decoded (vtgpu3d_baraddr) */
	hr = hv_vm_map(r->map, g->base + r->map_off, r->map_len,
	    HV_MEMORY_READ | HV_MEMORY_WRITE);
	if (hr != HV_SUCCESS) {
		EPRINTLN("virtio-gpu 3d: hv_vm_map(%p, %#llx, %#llx): %#x", r->map,
		    (unsigned long long)(g->base + r->map_off), (unsigned long long)r->map_len,
		    (unsigned)hr);
		return (-1);
	}
	return (0);
}

static void
res_unmap(struct vtgpu3d *g, struct res3d *r)
{
	if (r->map == NULL)
		return;
	blob_unmap_guest(g, r);
	virgl_renderer_resource_unmap(r->id);
	r->map = NULL;
}

void
vtgpu3d_baraddr(struct vtgpu3d *g, int enabled, uint64_t addr)
{
	struct res3d *r;
	uint64_t base = enabled ? addr : 0;

	g->host.lock(g->host.arg);
	if (base != g->base) {
		LIST_FOREACH(r, &g->res, link)
			if (r->map != NULL)
				blob_unmap_guest(g, r);
		g->base = base;
		LIST_FOREACH(r, &g->res, link)
			if (r->map != NULL)
				(void)blob_map_guest(g, r);
	}
	g->host.unlock(g->host.arg);
}

/* the scanout */

static void
scanout_off(struct vtgpu3d *g)
{
	if (g->so_res == 0)
		return;
	g->host.present(g->host.arg, NULL, 0, 0, 0, 0, 0, 0);
	g->so_map = NULL;
	g->so_res = 0;
	g->so_late = 0;
}

static void
scanout_present(struct vtgpu3d *g, uint32_t y, uint32_t rows)
{
	if (y >= g->so_height)
		return;
	rows = MIN(rows, g->so_height - y);
	g->host.present(g->host.arg, g->so_map + g->so_offset + (size_t)y * g->so_stride,
	    g->so_format, g->so_width, g->so_height, g->so_stride, y, rows);
}

static uint64_t
now_ms(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ((uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000);
}

/* does the scanout's context still have a fence under way from before the flip? */
static int
scanout_busy(struct vtgpu3d *g, uint32_t ctx, uint64_t upto)
{
	struct fence *p;

	STAILQ_FOREACH(p, &g->pending, link)
		if (p->ctx_id == ctx && p->fence_id <= upto)
			return (1);
	return (0);
}

/*
 * The guest shows a picture (a flip to another blob, a flush): show it here. The guest says
 * so when it has sent the drawing, not when the drawing is done: the compositor's commands
 * are still under way in the renderer, and what is copied at once can be a frame without
 * what is drawn last (seen 2026-11-12: with a program redrawing under another window, the
 * upper window's title bar was missing in about one copy in thirty). The context's fences
 * that are pending now are that work: the picture is copied when they have retired
 * (scanout_retired, from the fence thread), or after a tenth of a second whatever happens.
 */
static void
scanout_show(struct vtgpu3d *g, uint32_t y, uint32_t rows)
{
	struct res3d *r = res_find(g, g->so_res);
	struct fence *p;
	uint64_t last = 0;
	int any = 0;

	if (r != NULL && !r->classic && r->ctx != 0)
		STAILQ_FOREACH(p, &g->pending, link)
			if (p->ctx_id == r->ctx) {
				any = 1;
				if (p->fence_id > last)
					last = p->fence_id;
			}
	if (!any || g->no_late || (g->so_late && now_ms() - g->so_late_ms > 100)) {
		g->so_late = 0;
		scanout_present(g, y, rows);
		return;
	}
	if (!g->so_late)
		g->so_late_ms = now_ms();
	g->so_late = 1;
	g->so_fence = last;
}

/* fences have retired (the device's lock is held): is the picture that waited whole now? */
static void
scanout_retired(struct vtgpu3d *g)
{
	struct res3d *r;

	if (!g->so_late)
		return;
	r = res_find(g, g->so_res);
	if (r != NULL && scanout_busy(g, r->ctx, g->so_fence) && now_ms() - g->so_late_ms <= 100)
		return;
	g->so_late = 0;
	if (g->so_res != 0)
		scanout_present(g, 0, g->so_height);
}

static void
res_free(struct vtgpu3d *g, struct res3d *r)
{
	struct iovec *iov = NULL;
	int n = 0;

	if (g->so_res == r->id)
		scanout_off(g);
	if (r->classic) {
		free(r->rw);
		free(r->iov);
		LIST_REMOVE(r, link);
		free(r);
		return;
	}
	if (r->pix != NULL)
		munmap(r->pix, r->pix_len);
	if (r->rw != NULL)
		munmap(r->rw, r->rw_len);
	res_unmap(g, r);
	virgl_renderer_resource_detach_iov(r->id, &iov, &n);
	virgl_renderer_resource_unref(r->id);
	free(r->iov);
	LIST_REMOVE(r, link);
	free(r);
}

/* fences */

static void
fence_retired(struct vtgpu3d *g, uint32_t ctx_id, uint32_t ring_idx, uint64_t fence_id)
{
	struct fence *f = malloc(sizeof(*f));

	if (f == NULL)
		return;
	f->ctx_id = ctx_id;
	f->ring_idx = ring_idx;
	f->fence_id = fence_id;
	pthread_mutex_lock(&g->fmtx);
	STAILQ_INSERT_TAIL(&g->retired, f, link);
	pthread_cond_signal(&g->fcond);
	pthread_mutex_unlock(&g->fmtx);
}

static void
cb_write_fence(void *cookie __unused, uint32_t fence __unused)
{
	/* fences without a ring index are answered at once (fence_or_done) */
}

static void
cb_write_context_fence(void *cookie, uint32_t ctx_id, uint32_t ring_idx, uint64_t fence_id)
{
	fence_retired(cookie, ctx_id, ring_idx, fence_id);
}

static void *
fence_thread(void *arg)
{
	struct vtgpu3d *g = arg;
	struct fenceq q;
	struct fence *f, *p, *tmp;

	for (;;) {
		pthread_mutex_lock(&g->fmtx);
		while (STAILQ_EMPTY(&g->retired))
			pthread_cond_wait(&g->fcond, &g->fmtx);
		STAILQ_INIT(&q);
		STAILQ_CONCAT(&q, &g->retired);
		pthread_mutex_unlock(&g->fmtx);

		g->host.lock(g->host.arg);
		while ((f = STAILQ_FIRST(&q)) != NULL) {
			STAILQ_REMOVE_HEAD(&q, link);
			/* a retired fence retires the earlier ones of its ring too (mergeable) */
			STAILQ_FOREACH_SAFE(p, &g->pending, link, tmp) {
				if (p->ctx_id != f->ctx_id || p->ring_idx != f->ring_idx ||
				    p->fence_id > f->fence_id)
					continue;
				STAILQ_REMOVE(&g->pending, p, fence, link);
				if (g->debug)
					EPRINTLN("virtio-gpu 3d: fence %llu ctx %u ring %u retired",
					    (unsigned long long)p->fence_id, p->ctx_id, p->ring_idx);
				g->host.complete(g->host.arg, p->token);
				free(p);
			}
			free(f);
		}
		scanout_retired(g);
		g->host.unlock(g->host.arg);
	}
	return (NULL);
}

/*
 * A command that went well: the answer now, or when its fence retires. Only a fence with a ring
 * index waits: the others are for commands that are finished when they return.
 */
static uint32_t
fence_or_done(struct vtgpu3d *g, const struct vgpu_hdr *h, uint64_t token)
{
	struct fence *p;

	if ((h->flags & (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX)) !=
	    (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX))
		return (RESP_OK_NODATA);
	if ((p = malloc(sizeof(*p))) == NULL)
		return (RESP_OK_NODATA);
	p->ctx_id = h->ctx_id;
	p->ring_idx = h->ring_idx;
	p->fence_id = h->fence_id;
	p->token = token;
	/* queued first: the renderer may retire it before create_fence returns */
	STAILQ_INSERT_TAIL(&g->pending, p, link);
	if (virgl_renderer_context_create_fence(h->ctx_id, VIRGL_RENDERER_FENCE_FLAG_MERGEABLE,
	    h->ring_idx, h->fence_id) != 0) {
		/*
		 * The context is gone (its render server process died): the fence would never
		 * retire and Windows would stop the adapter (VIDEO_TDR_FAILURE). Answer now.
		 */
		STAILQ_REMOVE(&g->pending, p, fence, link);
		free(p);
		return (RESP_OK_NODATA);
	}
	return (VTGPU3D_DEFERRED);
}

/*
 * The blit context: what viogpu3d sends a virgl context for a present that is a copy
 * (viogpu_device.cpp GenerateBltPresent): PIPE_RESOURCE_SET_TYPE for each surface once (its
 * format and layout), RESOURCE_COPY_REGION per rectangle, and transfers for a surface the
 * guest also keeps in its own memory. A surface's pixels are linear, in shared memory that the
 * renderer's texture is made on (DXMT's shared textures; the scanout is read the same way), so
 * the copy is done here on the CPU and the compositor's texture has it at once.
 */

static int blt_red_first(uint32_t virgl_format);

static int
blt_is(struct vtgpu3d *g, uint32_t ctx_id)
{
	uint32_t i;

	for (i = 0; i < g->nblt; i++)
		if (g->blt_ctx[i] == ctx_id)
			return (1);
	return (0);
}

/* a surface's pixels, for writing too: its shared memory, mapped the first time */
static uint8_t *
blt_pixels(struct vtgpu3d *g, struct res3d *r, size_t *len)
{
	struct stat st;
	uint32_t fd_type = 0;
	int fd = -1;

	if (r->rw == NULL && !r->classic) {
		if (virgl_renderer_resource_export_blob(r->id, &fd_type, &fd) != 0 ||
		    fd_type != VIRGL_RENDERER_BLOB_FD_TYPE_SHM || fstat(fd, &st) != 0 ||
		    st.st_size <= 0) {
			if (g->blt_said++ < 40)
				EPRINTLN("virtio-gpu 3d: blit: resource %u is not shared memory (type %u)",
				    r->id, fd_type);
			if (fd >= 0)
				close(fd);
			return (NULL);
		}
		r->rw = mmap(NULL, (size_t)st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
		close(fd);
		if (r->rw == MAP_FAILED) {
			r->rw = NULL;
			return (NULL);
		}
		r->rw_len = (size_t)st.st_size;
	}
	*len = r->rw_len;
	return (r->rw);
}

/* a texture of the blit context's own: RESOURCE_CREATE_3D */
static uint32_t
blt_create(struct vtgpu3d *g, const uint8_t *b, size_t len)
{
	/* resource_id, target, format, bind, width, height, depth, array_size, last_level, nr_samples, flags */
	uint32_t c[11];
	struct res3d *r;

	if (len < sizeof(c))
		return (RESP_ERR_INVALID_PARAMETER);
	memcpy(c, b, sizeof(c));
	if (c[0] == 0 || res_find(g, c[0]) != NULL)
		return (RESP_ERR_INVALID_RESOURCE_ID);
	/* a flat picture of 32-bit pixels: what a surface is */
	if (c[1] != 2 /* PIPE_TEXTURE_2D */ || blt_red_first(c[2]) < 0 || c[4] == 0 || c[5] == 0 ||
	    c[4] > 16384 || c[5] > 16384 || c[6] > 1 || c[8] != 0) {
		if (g->blt_said++ < 40)
			EPRINTLN("virtio-gpu 3d: blit: a texture of target %u, format %u, %ux%ux%u, %u levels is not made",
			    c[1], c[2], c[4], c[5], c[6], c[8] + 1);
		return (RESP_ERR_UNSPEC);
	}
	if ((r = calloc(1, sizeof(*r))) == NULL)
		return (RESP_ERR_OUT_OF_MEMORY);
	r->rw_len = (size_t)c[4] * c[5] * 4;
	if ((r->rw = calloc(1, r->rw_len)) == NULL) {
		free(r);
		return (RESP_ERR_OUT_OF_MEMORY);
	}
	r->id = c[0];
	r->classic = 1;
	r->t_format = c[2];
	r->t_width = c[4];
	r->t_height = c[5];
	r->t_stride = c[4] * 4;
	LIST_INSERT_HEAD(&g->res, r, link);
	return (RESP_OK_NODATA);
}

/* the guest's pages for one of those: RESOURCE_ATTACH_BACKING */
static uint32_t
blt_backing(struct vtgpu3d *g, struct res3d *r, const uint8_t *b, size_t len)
{
	/* resource_id, nr_entries; then {addr, length, padding} */
	uint32_t n, i;

	if (len < 8)
		return (RESP_ERR_INVALID_PARAMETER);
	memcpy(&n, b + 4, 4);
	if (n > 1u << 20 || len < 8 + (size_t)n * 16)
		return (RESP_ERR_INVALID_PARAMETER);
	free(r->iov);
	r->niov = 0;
	if ((r->iov = calloc(n ? n : 1, sizeof(*r->iov))) == NULL)
		return (RESP_ERR_OUT_OF_MEMORY);
	for (i = 0; i < n; i++) {
		uint64_t gpa;
		uint32_t l;

		memcpy(&gpa, b + 8 + (size_t)i * 16, 8);
		memcpy(&l, b + 8 + (size_t)i * 16 + 8, 4);
		r->iov[i].iov_base = paddr_guest2host(g->pi->pi_vmctx, gpa, l);
		r->iov[i].iov_len = l;
		if (r->iov[i].iov_base == NULL) {
			free(r->iov);
			r->iov = NULL;
			return (RESP_ERR_INVALID_PARAMETER);
		}
	}
	r->niov = n;
	return (RESP_OK_NODATA);
}

/* bytes between a texture's pixels here and the guest's pages, from `off` in the pages */
static void
blt_pages(struct res3d *r, uint64_t off, uint8_t *p, size_t n, int to_guest)
{
	uint32_t i;

	for (i = 0; i < r->niov && n != 0; i++) {
		size_t l = r->iov[i].iov_len, m;

		if (off >= l) {
			off -= l;
			continue;
		}
		m = MIN(n, l - (size_t)off);
		if (to_guest)
			memcpy((uint8_t *)r->iov[i].iov_base + off, p, m);
		else
			memcpy(p, (uint8_t *)r->iov[i].iov_base + off, m);
		p += m;
		n -= m;
		off = 0;
	}
}

/* TRANSFER_TO_HOST_3D (the guest's pages -> the texture) and TRANSFER_FROM_HOST_3D */
static uint32_t
blt_transfer(struct vtgpu3d *g, const uint8_t *b, size_t len, int to_guest)
{
	/* box x, y, z, w, h, d; offset (64 bits); resource_id, level, stride, layer_stride */
	uint32_t c[12], x, y, w, h, stride, row;
	uint64_t off;
	struct res3d *r;

	if (len < sizeof(c))
		return (RESP_ERR_INVALID_PARAMETER);
	memcpy(c, b, sizeof(c));
	memcpy(&off, &c[6], 8);
	if ((r = res_find(g, c[8])) == NULL || !r->classic) {
		if (g->blt_said++ < 40)
			EPRINTLN("virtio-gpu 3d: blit: transfer %s for resource %u: not a texture of ours",
			    to_guest ? "from the host" : "to the host", c[8]);
		return (RESP_OK_NODATA);
	}
	x = c[0], y = c[1], w = c[3], h = c[4];
	if (x >= r->t_width || y >= r->t_height || r->iov == NULL)
		return (RESP_OK_NODATA);
	w = MIN(w, r->t_width - x);
	h = MIN(h, r->t_height - y);
	/* (no stride given: the rows of the box follow each other) */
	stride = c[10] != 0 ? c[10] : w * 4;
	for (row = 0; row < h; row++)
		blt_pages(r, off + (uint64_t)row * stride,
		    r->rw + (size_t)(y + row) * r->t_stride + (size_t)x * 4, (size_t)w * 4, to_guest);
	/* the guest drew into what is on the screen */
	if (!to_guest && r->id == g->so_res)
		scanout_present(g, y, h);
	return (RESP_OK_NODATA);
}

/* red first in memory (R8G8B8A8, R8G8B8X8, their sRGB), or blue first; -1: not a 32-bit one we know */
static int
blt_red_first(uint32_t virgl_format)
{
	switch (virgl_format) {
	case 67: case 134: case 104:
		return (1);
	case 1: case 2: case 100: case 101:
		return (0);
	default:
		return (-1);
	}
}

/* dst <- src, a rectangle; the numbers are RESOURCE_COPY_REGION's */
static void
blt_copy(struct vtgpu3d *g, const uint32_t *c)
{
	struct res3d *d = res_find(g, c[0]), *s = res_find(g, c[5]);
	uint32_t dx = c[2], dy = c[3], sx = c[7], sy = c[8], w = c[10], h = c[11], y, x;
	uint8_t *dp, *sp;
	size_t dlen, slen;
	int dr, sr;

	if (d == NULL || s == NULL || d->t_stride == 0 || s->t_stride == 0) {
		if (g->blt_said++ < 40)
			EPRINTLN("virtio-gpu 3d: blit: copy %u <- %u: %s", c[0], c[5],
			    d == NULL || s == NULL ? "no such resource" : "a surface without its type");
		return;
	}
	dr = blt_red_first(d->t_format);
	sr = blt_red_first(s->t_format);
	if (dr < 0 || sr < 0) {
		if (g->blt_said++ < 40)
			EPRINTLN("virtio-gpu 3d: blit: copy %u (format %u) <- %u (format %u): not done",
			    c[0], d->t_format, c[5], s->t_format);
		return;
	}
	/* what is outside either surface is not copied */
	if (sx >= s->t_width || sy >= s->t_height || dx >= d->t_width || dy >= d->t_height)
		return;
	w = MIN(w, MIN(s->t_width - sx, d->t_width - dx));
	h = MIN(h, MIN(s->t_height - sy, d->t_height - dy));
	if ((dp = blt_pixels(g, d, &dlen)) == NULL || (sp = blt_pixels(g, s, &slen)) == NULL)
		return;
	if ((uint64_t)d->t_offset + (uint64_t)(dy + h - 1) * d->t_stride + (uint64_t)(dx + w) * 4 > dlen ||
	    (uint64_t)s->t_offset + (uint64_t)(sy + h - 1) * s->t_stride + (uint64_t)(sx + w) * 4 > slen)
		return;
	for (y = 0; y < h; y++) {
		uint8_t *o = dp + d->t_offset + (size_t)(dy + y) * d->t_stride + (size_t)dx * 4;
		const uint8_t *i = sp + s->t_offset + (size_t)(sy + y) * s->t_stride + (size_t)sx * 4;

		if (dr == sr) {
			memcpy(o, i, (size_t)w * 4);
			continue;
		}
		for (x = 0; x < w; x++, o += 4, i += 4) {
			o[0] = i[2];
			o[1] = i[1];
			o[2] = i[0];
			o[3] = i[3];
		}
	}
	if (g->blt_copies++ == 0 || g->debug)
		EPRINTLN("virtio-gpu 3d: blit: %u (format %u, %ux%u) <- %u (format %u, %ux%u): %ux%u at %u,%u from %u,%u",
		    c[0], d->t_format, d->t_width, d->t_height, c[5], s->t_format, s->t_width,
		    s->t_height, w, h, dx, dy, sx, sy);
	/* the copy went into what is on the screen: show it */
	if (d->id == g->so_res)
		scanout_present(g, dy, h);
}

/* the commands of one SUBMIT_3D to the blit context */
static void
blt_submit(struct vtgpu3d *g, const uint8_t *b, uint32_t words)
{
	uint32_t w[16], hd, n, at = 0;
	struct res3d *r;

	while (at < words) {
		memcpy(&hd, b + (size_t)at * 4, 4);
		n = hd >> 16;
		if (n > words - at - 1)
			return;
		memset(w, 0, sizeof(w));
		memcpy(w, b + (size_t)(at + 1) * 4, MIN((size_t)n, nitems(w)) * 4);
		switch (hd & 0xff) {
		case VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE:
			/* res, format, bind, width, height, usage, modifier lo, hi, {stride, offset} */
			if (n >= 10 && (r = res_find(g, w[0])) != NULL) {
				r->t_format = w[1];
				r->t_width = w[3];
				r->t_height = w[4];
				r->t_stride = w[8];
				r->t_offset = w[9];
				if (g->debug)
					EPRINTLN("virtio-gpu 3d: blit: resource %u is format %u, %ux%u, stride %u, offset %u",
					    w[0], w[1], w[3], w[4], w[8], w[9]);
			}
			break;
		case VIRGL_CCMD_RESOURCE_COPY_REGION:
			/* dst res, level, x, y, z; src res, level, x, y, z, width, height, depth */
			if (n >= 13)
				blt_copy(g, w);
			break;
		default:
			if (g->blt_said++ < 40)
				EPRINTLN("virtio-gpu 3d: blit: virgl command %u (%u words) is not done",
				    hd & 0xff, n);
			break;
		}
		at += 1 + n;
	}
}

/* commands */

static uint32_t
cmd_create_blob(struct vtgpu3d *g, const struct vgpu_hdr *h, const uint8_t *b, size_t len)
{
	/* resource_id, blob_mem, blob_flags, nr_entries, blob_id, size; then {addr, length, pad} */
	struct virgl_renderer_resource_create_blob_args a;
	struct { uint32_t id, mem, flags, n; uint64_t blob_id, size; } c;
	struct res3d *r;
	uint32_t i;

	if (len < sizeof(c))
		return (RESP_ERR_INVALID_PARAMETER);
	memcpy(&c, b, sizeof(c));
	if (c.id == 0 || res_find(g, c.id) != NULL)
		return (RESP_ERR_INVALID_RESOURCE_ID);
	if (c.n > 1u << 20 || len < sizeof(c) + (size_t)c.n * 16)
		return (RESP_ERR_INVALID_PARAMETER);
	if ((r = calloc(1, sizeof(*r))) == NULL)
		return (RESP_ERR_OUT_OF_MEMORY);
	r->id = c.id;
	r->ctx = h->ctx_id;
	if (c.mem != VIRTIO_GPU_BLOB_MEM_HOST3D && c.n != 0) {
		if ((r->iov = calloc(c.n, sizeof(*r->iov))) == NULL) {
			free(r);
			return (RESP_ERR_OUT_OF_MEMORY);
		}
		for (i = 0; i < c.n; i++) {
			uint64_t gpa;
			uint32_t l;
			memcpy(&gpa, b + sizeof(c) + (size_t)i * 16, 8);
			memcpy(&l, b + sizeof(c) + (size_t)i * 16 + 8, 4);
			r->iov[i].iov_base = paddr_guest2host(g->pi->pi_vmctx, gpa, l);
			r->iov[i].iov_len = l;
			if (r->iov[i].iov_base == NULL) {
				free(r->iov);
				free(r);
				return (RESP_ERR_INVALID_PARAMETER);
			}
		}
		r->niov = c.n;
	}
	memset(&a, 0, sizeof(a));
	a.res_handle = c.id;
	a.ctx_id = h->ctx_id;
	a.blob_mem = c.mem;
	a.blob_flags = c.flags;
	a.blob_id = c.blob_id;
	a.size = c.size;
	a.iovecs = r->iov;
	a.num_iovs = r->niov;
	int rc = virgl_renderer_resource_create_blob(&a);
	if (rc != 0) {
		/* (the renderer gives -errno; "Too many open files" is this process's limit) */
		EPRINTLN("virtio-gpu 3d: blob %u (ctx %u, mem %u, id %llu, %llu bytes) not created: %d (%s)",
		    c.id, h->ctx_id, c.mem, (unsigned long long)c.blob_id,
		    (unsigned long long)c.size, rc, strerror(rc < 0 ? -rc : rc));
		free(r->iov);
		free(r);
		return (RESP_ERR_UNSPEC);
	}
	LIST_INSERT_HEAD(&g->res, r, link);
	return (RESP_OK_NODATA);
}

static uint32_t
cmd_map_blob(struct vtgpu3d *g, const uint8_t *b, size_t len, uint8_t *resp, size_t cap,
    size_t *rlen)
{
	struct { uint32_t id, pad; uint64_t offset; } m;
	struct res3d *r;
	uint64_t size;
	uint32_t info = 0;
	void *p;

	if (len < sizeof(m) || cap < HDR + 8)
		return (RESP_ERR_INVALID_PARAMETER);
	memcpy(&m, b, sizeof(m));
	if ((r = res_find(g, m.id)) == NULL)
		return (RESP_ERR_INVALID_RESOURCE_ID);
	if (r->map != NULL || (m.offset & (GUEST_PAGE - 1)) != 0)
		return (RESP_ERR_INVALID_PARAMETER);
	if (virgl_renderer_resource_map(m.id, &p, &size) != 0)
		return (RESP_ERR_UNSPEC);
	size = roundup2(size, (uint64_t)GUEST_PAGE);
	if (m.offset > g->hostmem || size > g->hostmem - m.offset) {
		virgl_renderer_resource_unmap(m.id);
		return (RESP_ERR_INVALID_PARAMETER);
	}
	r->map = p;
	r->map_off = m.offset;
	r->map_len = size;
	if (blob_map_guest(g, r) != 0) {
		r->map = NULL;
		virgl_renderer_resource_unmap(m.id);
		return (RESP_ERR_UNSPEC);
	}
	virgl_renderer_resource_get_map_info(m.id, &info);
	memset(resp + HDR, 0, 8);
	memcpy(resp + HDR, &info, 4);
	*rlen = HDR + 8;
	return (RESP_OK_MAP_INFO);
}

static uint32_t
cmd_set_scanout_blob(struct vtgpu3d *g, const uint8_t *b, size_t len)
{
	/* rect, scanout_id, resource_id, width, height, format, padding, strides[4], offsets[4] */
	uint32_t v[18], fd_type = 0;
	struct res3d *r;
	int fd = -1;

	if (len < sizeof(v))
		return (RESP_ERR_INVALID_PARAMETER);
	memcpy(v, b, sizeof(v));
	if (v[4] != 0)
		return (RESP_ERR_INVALID_SCANOUT_ID);
	if (v[5] == 0) {
		scanout_off(g);
		return (RESP_OK_NODATA);
	}
	if ((r = res_find(g, v[5])) == NULL)
		return (RESP_ERR_INVALID_RESOURCE_ID);
	/* the rect is the whole image for the drivers there are: only that is done */
	if (v[6] < 16 || v[7] < 16 || v[0] != 0 || v[1] != 0 || v[2] != v[6] || v[3] != v[7] ||
	    v[10] < (uint64_t)v[6] * 4)
		return (RESP_ERR_INVALID_PARAMETER);
	if (r->pix == NULL) {
		struct stat st;

		if (virgl_renderer_resource_export_blob(v[5], &fd_type, &fd) != 0 ||
		    fd_type != VIRGL_RENDERER_BLOB_FD_TYPE_SHM) {
			EPRINTLN("virtio-gpu 3d: scanout blob %u is not shared memory (type %u)",
			    v[5], fd_type);
			if (fd >= 0)
				close(fd);
			return (RESP_ERR_UNSPEC);
		}
		if (fstat(fd, &st) != 0 || st.st_size <= 0) {
			EPRINTLN("virtio-gpu 3d: scanout blob %u: fstat: %s", v[5], strerror(errno));
			close(fd);
			return (RESP_ERR_UNSPEC);
		}
		r->pix = mmap(NULL, (size_t)st.st_size, PROT_READ, MAP_SHARED, fd, 0);
		close(fd);
		if (r->pix == MAP_FAILED) {
			r->pix = NULL;
			return (RESP_ERR_UNSPEC);
		}
		r->pix_len = (size_t)st.st_size;
	}
	if ((uint64_t)r->pix_len < (uint64_t)v[14] + (uint64_t)v[10] * v[7]) {
		EPRINTLN("virtio-gpu 3d: scanout blob %u: %zu bytes for %ux%u stride %u offset %u",
		    v[5], r->pix_len, v[6], v[7], v[10], v[14]);
		return (RESP_ERR_INVALID_PARAMETER);
	}
	if (g->so_res == 0 || g->so_width != v[6] || g->so_height != v[7] || g->so_format != v[8])
		EPRINTLN("virtio-gpu 3d: scanout blob %u: %ux%u format %u stride %u", v[5], v[6],
		    v[7], v[8], v[10]);
	g->so_map = r->pix;

	g->so_res = v[5];
	g->so_width = v[6];
	g->so_height = v[7];
	g->so_format = v[8];
	g->so_stride = v[10];
	g->so_offset = v[14];
	scanout_show(g, 0, g->so_height);
	return (RESP_OK_NODATA);
}

static uint32_t vtgpu3d_cmd1(struct vtgpu3d *g, uint8_t *req, size_t len, uint8_t *resp,
    size_t cap, size_t *rlen, uint64_t token);

uint32_t
vtgpu3d_cmd(struct vtgpu3d *g, uint8_t *req, size_t len, uint8_t *resp, size_t cap, size_t *rlen,
    uint64_t token)
{
	struct vgpu_hdr h;
	uint32_t d[6] = { 0 }, t;

	t = vtgpu3d_cmd1(g, req, len, resp, cap, rlen, token);
	if (g->debug && len >= HDR) {	/* VTGPU3D_DEBUG=1: every command and its answer */
		memcpy(&h, req, HDR);
		memcpy(d, req + HDR, MIN(len - HDR, sizeof(d)));
		EPRINTLN("virtio-gpu 3d: cmd %#x ctx %u flags %#x fence %llu ring %u len %zu: "
		    "%u %u %u %u %u %u -> %#x", h.type, h.ctx_id, h.flags,
		    (unsigned long long)h.fence_id, h.ring_idx, len - HDR, d[0], d[1], d[2], d[3],
		    d[4], d[5], t);
	}
	return (t);
}

static uint32_t
vtgpu3d_cmd1(struct vtgpu3d *g, uint8_t *req, size_t len, uint8_t *resp, size_t cap, size_t *rlen,
    uint64_t token)
{
	struct vgpu_hdr hdr;
	const struct vgpu_hdr *h = &hdr;
	const uint8_t *b;
	struct res3d *r;
	uint32_t v[8], t;

	if (len < HDR || cap < HDR)
		return (VTGPU3D_UNHANDLED);
	memcpy(&hdr, req, HDR);
	b = req + HDR;
	len -= HDR;
	*rlen = HDR;
#define	NEED(n)	do { if (len < (n)) return (RESP_ERR_INVALID_PARAMETER); memcpy(v, b, (n)); } while (0)
	switch (h->type) {
	case CMD_GET_CAPSET_INFO: {
		uint32_t out[4] = { 0 };
		NEED(4);	/* capset_index -> capset_id, max_version, max_size, padding */
		if (cap < HDR + sizeof(out))
			return (RESP_ERR_INVALID_PARAMETER);
		if (v[0] < g->ncapsets) {
			out[0] = g->capsets[v[0]];
			if (out[0] == CAPSET_VIRGL) {
				out[1] = 1;	/* ours (the blit context): version 1, nothing in it */
				out[2] = VIRGL_CAPS_V1_SIZE;
			} else
				virgl_renderer_get_cap_set(out[0], &out[1], &out[2]);
		}
		memcpy(resp + HDR, out, sizeof(out));
		*rlen = HDR + sizeof(out);
		return (RESP_OK_CAPSET_INFO);
	}
	case CMD_GET_CAPSET: {
		uint32_t ver = 0, size = 0;
		NEED(8);	/* capset_id, capset_version -> the capset */
		if (g->blt && v[0] == CAPSET_VIRGL) {
			/* no OpenGL behind it: every capability is "no" */
			if (cap < HDR + VIRGL_CAPS_V1_SIZE)
				return (RESP_ERR_INVALID_PARAMETER);
			memset(resp + HDR, 0, VIRGL_CAPS_V1_SIZE);
			*rlen = HDR + VIRGL_CAPS_V1_SIZE;
			return (RESP_OK_CAPSET);
		}
		virgl_renderer_get_cap_set(v[0], &ver, &size);
		if (size == 0 || cap < HDR + size)
			return (RESP_ERR_INVALID_PARAMETER);
		memset(resp + HDR, 0, size);
		virgl_renderer_fill_caps(v[0], v[1], resp + HDR);
		*rlen = HDR + size;
		return (RESP_OK_CAPSET);
	}
	case CMD_CTX_CREATE: {
		char name[65];
		NEED(8);	/* nlen, context_init; debug_name[64] */
		memset(name, 0, sizeof(name));
		if (len >= 8 + 64)
			memcpy(name, b + 8, 64);
		v[0] = MIN(v[0], 64u);
		if (g->blt && ((v[1] & 0xff) == CAPSET_VIRGL || (v[1] & 0xff) == CAPSET_VIRGL2)) {
			/* the blit context: ours, the renderer never hears of it */
			if (!blt_is(g, h->ctx_id)) {
				if (g->nblt == MAX_BLT_CTX)
					return (RESP_ERR_OUT_OF_MEMORY);
				g->blt_ctx[g->nblt++] = h->ctx_id;
			}
			if (g->debug)
				EPRINTLN("virtio-gpu 3d: blit: context %u (\"%s\")", h->ctx_id, name);
			return (RESP_OK_NODATA);
		}
		int crc = virgl_renderer_context_create_with_flags(h->ctx_id, v[1], v[0], name);
		if (crc != 0) {
			EPRINTLN("virtio-gpu 3d: context %u (\"%s\", init %#x) not created: %d (%s)",
			    h->ctx_id, name, v[1], crc, strerror(crc < 0 ? -crc : crc));
			return (RESP_ERR_UNSPEC);
		}
		return (fence_or_done(g, h, token));
	}
	case CMD_CTX_DESTROY:
		if (blt_is(g, h->ctx_id)) {
			uint32_t i;

			for (i = 0; i < g->nblt; i++)
				if (g->blt_ctx[i] == h->ctx_id)
					g->blt_ctx[i] = g->blt_ctx[--g->nblt];
			return (RESP_OK_NODATA);
		}
		virgl_renderer_context_destroy(h->ctx_id);
		return (RESP_OK_NODATA);
	case CMD_CTX_ATTACH_RESOURCE:
		NEED(4);
		/* (the blit context finds resources by their number; its textures are nobody else's) */
		if (blt_is(g, h->ctx_id) || ((r = res_find(g, v[0])) != NULL && r->classic))
			return (RESP_OK_NODATA);
		virgl_renderer_ctx_attach_resource(h->ctx_id, v[0]);
		return (fence_or_done(g, h, token));
	case CMD_CTX_DETACH_RESOURCE:
		NEED(4);
		if (blt_is(g, h->ctx_id) || ((r = res_find(g, v[0])) != NULL && r->classic))
			return (RESP_OK_NODATA);
		virgl_renderer_ctx_detach_resource(h->ctx_id, v[0]);
		return (fence_or_done(g, h, token));
	case CMD_SUBMIT_3D:
		NEED(8);	/* size, padding; the commands */
		if (v[0] > len - 8 || (v[0] & 3) != 0)
			return (RESP_ERR_INVALID_PARAMETER);
		if (blt_is(g, h->ctx_id)) {
			blt_submit(g, req + HDR + 8, v[0] / 4);
			return (RESP_OK_NODATA);
		}
		if (virgl_renderer_submit_cmd(req + HDR + 8, h->ctx_id, v[0] / 4) != 0)
			return (RESP_ERR_UNSPEC);
		return (fence_or_done(g, h, token));
	case CMD_RESOURCE_CREATE_BLOB:
		t = cmd_create_blob(g, h, b, len);
		return (t == RESP_OK_NODATA ? fence_or_done(g, h, token) : t);
	case CMD_RESOURCE_MAP_BLOB:
		return (cmd_map_blob(g, b, len, resp, cap, rlen));
	case CMD_RESOURCE_UNMAP_BLOB:
		NEED(4);
		if ((r = res_find(g, v[0])) == NULL)
			return (RESP_ERR_INVALID_RESOURCE_ID);
		res_unmap(g, r);
		return (RESP_OK_NODATA);
	case CMD_SET_SCANOUT_BLOB:
		return (cmd_set_scanout_blob(g, b, len));
	case CMD_TRANSFER_TO_HOST_3D:
	case CMD_TRANSFER_FROM_HOST_3D:
		/* the blit context's textures: between the guest's pages and the pixels here */
		if (!g->blt)
			return (RESP_ERR_UNSPEC);
		return (blt_transfer(g, b, len, h->type == CMD_TRANSFER_FROM_HOST_3D));
	case CMD_RESOURCE_CREATE_3D:
		if (!g->blt)
			return (RESP_ERR_UNSPEC);
		return (blt_create(g, b, len));
	case CMD_RESOURCE_ASSIGN_UUID:
		return (RESP_ERR_UNSPEC);	/* vrend's, or a feature that is not offered */

	/* the 2D device's commands, when they name one of our resources */
	case CMD_RESOURCE_UNREF:
		NEED(4);
		if ((r = res_find(g, v[0])) == NULL)
			return (VTGPU3D_UNHANDLED);
		res_free(g, r);
		return (RESP_OK_NODATA);
	case CMD_RESOURCE_ATTACH_BACKING:
	case CMD_RESOURCE_DETACH_BACKING:
		NEED(4);
		if ((r = res_find(g, v[0])) == NULL)
			return (VTGPU3D_UNHANDLED);
		if (r->classic) {
			if (h->type == CMD_RESOURCE_ATTACH_BACKING)
				return (blt_backing(g, r, b, len));
			free(r->iov);
			r->iov = NULL;
			r->niov = 0;
			return (RESP_OK_NODATA);
		}
		return (RESP_ERR_UNSPEC);	/* a blob has its memory from its creation */
	case CMD_SET_SCANOUT:
		/* a 2D resource (or none) takes the scanout: ours is over; the 2D device goes on */
		NEED(24);
		if ((r = res_find(g, v[5])) != NULL) {
			/* one of the blit context's textures (the primary before the compositor runs) */
			if (!r->classic || v[4] != 0)
				return (RESP_ERR_INVALID_RESOURCE_ID);
			if (g->so_res != r->id)
				EPRINTLN("virtio-gpu 3d: scanout texture %u: %ux%u format %u", r->id,
				    r->t_width, r->t_height, r->t_format);
			g->so_map = r->rw;
			g->so_res = r->id;
			g->so_width = r->t_width;
			g->so_height = r->t_height;
			g->so_format = r->t_format;
			g->so_stride = r->t_stride;
			g->so_offset = 0;
			scanout_present(g, 0, g->so_height);
			return (RESP_OK_NODATA);
		}
		g->so_map = NULL;
		g->so_res = 0;
		return (VTGPU3D_UNHANDLED);
	case CMD_RESOURCE_FLUSH:
		NEED(20);	/* rect, resource_id */
		if (res_find(g, v[4]) == NULL)
			return (VTGPU3D_UNHANDLED);
		if (v[4] == g->so_res)
			scanout_show(g, v[1], v[3]);
		return (fence_or_done(g, h, token));
	default:
		return (VTGPU3D_UNHANDLED);
	}
#undef NEED
}

void
vtgpu3d_reset(struct vtgpu3d *g)
{
	struct res3d *r;
	struct fence *p;

	scanout_off(g);
	while ((r = LIST_FIRST(&g->res)) != NULL)
		res_free(g, r);
	virgl_renderer_reset();
	/* their chains went with the queues: nothing to answer */
	while ((p = STAILQ_FIRST(&g->pending)) != NULL) {
		STAILQ_REMOVE_HEAD(&g->pending, link);
		free(p);
	}
}

uint64_t
vtgpu3d_features(struct vtgpu3d *g __unused)
{
	return (VIRTIO_GPU_F_VIRGL | VIRTIO_GPU_F_RESOURCE_BLOB | VIRTIO_GPU_F_CONTEXT_INIT);
}

uint32_t
vtgpu3d_num_capsets(struct vtgpu3d *g)
{
	return (g->ncapsets);
}

/* pci_emul.c's pci_emul_add_capability() is static (as in pci_virtio_gpu.c) */
static int
add_cap(struct pci_devinst *pi, u_char *capdata, int caplen)
{
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

static struct virgl_renderer_callbacks vtgpu3d_cbs = {
	.version = 3,
	.write_fence = cb_write_fence,
	.write_context_fence = cb_write_context_fence,
};

struct vtgpu3d *
vtgpu3d_init(struct pci_devinst *pi, int bar_hostmem, uint64_t hostmem,
    const struct vtgpu3d_host *host)
{
	static const uint32_t ids[] = { CAPSET_NEPTUNE, CAPSET_VENUS };
	struct vtgpu3d *g;
	pthread_t tid;
	u_char c[24];
	uint32_t ver, size, lo, hi;
	size_t i;
	int flags, error;

	if ((g = calloc(1, sizeof(*g))) == NULL)
		return (NULL);
	g->pi = pi;
	g->host = *host;
	g->bar = bar_hostmem;
	g->hostmem = hostmem;
	g->debug = getenv("VTGPU3D_DEBUG") != NULL;
	g->no_late = getenv("VTGPU3D_LATE") != NULL && strcmp(getenv("VTGPU3D_LATE"), "0") == 0;
	/* the blit context (see the top): VTGPU3D_BLT=0 is the device as it was without */
	g->blt = getenv("VTGPU3D_BLT") == NULL || strcmp(getenv("VTGPU3D_BLT"), "0") != 0;
	LIST_INIT(&g->res);
	STAILQ_INIT(&g->pending);
	STAILQ_INIT(&g->retired);
	pthread_mutex_init(&g->fmtx, NULL);
	pthread_cond_init(&g->fcond, NULL);

	/*
	 * Neptune in render server processes, no vrend. THREAD_SYNC + ASYNC_FENCE_CB: fences
	 * retire on the renderer's thread as they happen, not when we poll.
	 */
	flags = VIRGL_RENDERER_NEPTUNE | VIRGL_RENDERER_RENDER_SERVER | VIRGL_RENDERER_NO_VIRGL |
	    VIRGL_RENDERER_THREAD_SYNC | VIRGL_RENDERER_ASYNC_FENCE_CB;
	if ((error = virgl_renderer_init(g, flags, &vtgpu3d_cbs)) != 0) {
		EPRINTLN("virtio-gpu 3d: virgl_renderer_init: %d (no render server? "
		    "RENDER_SERVER_EXEC_PATH)", error);
		free(g);
		return (NULL);
	}
	for (i = 0; i < nitems(ids); i++) {
		ver = size = 0;
		virgl_renderer_get_cap_set(ids[i], &ver, &size);
		if (size != 0 && g->ncapsets < MAX_CAPSETS)
			g->capsets[g->ncapsets++] = ids[i];
	}
	if (g->blt && g->ncapsets != 0 && g->ncapsets < MAX_CAPSETS)
		g->capsets[g->ncapsets++] = CAPSET_VIRGL;
	if (g->ncapsets == 0) {
		EPRINTLN("virtio-gpu 3d: the renderer has no capset (no Direct3D library found?)");
		virgl_renderer_cleanup(g);
		free(g);
		return (NULL);
	}

	/* the window: a 64-bit BAR and the virtio shared memory capability that names it */
	pci_emul_alloc_bar(pi, bar_hostmem, PCIBAR_MEM64, hostmem);
	memset(c, 0, sizeof(c));
	c[0] = PCIY_VENDOR;
	c[2] = sizeof(c);			/* struct virtio_pci_cap64 */
	c[3] = VIRTIO_PCI_CAP_SHARED_MEMORY_CFG;
	c[4] = bar_hostmem;
	c[5] = VIRTIO_GPU_SHM_ID_HOST_VISIBLE;
	lo = (uint32_t)hostmem;
	hi = (uint32_t)(hostmem >> 32);
	memcpy(&c[12], &lo, 4);			/* length; offset 0 */
	memcpy(&c[20], &hi, 4);			/* length_hi */
	add_cap(pi, c, sizeof(c));

	pthread_create(&tid, NULL, fence_thread, g);
	EPRINTLN("virtio-gpu 3d: %u capset(s), first %u; window %llu MiB in BAR %d", g->ncapsets,
	    g->capsets[0], (unsigned long long)(hostmem >> 20), bar_hostmem);
	return (g);
}
