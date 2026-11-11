/*
 * virtio-gpu 3D for keel's virtio-gpu (pci_virtio_gpu.c): contexts, blob resources in a
 * host-visible memory window, fences and a blob scanout, executed by virglrenderer
 * (UTM's fork with Neptune: Direct3D for Windows guests with the Triton driver).
 *
 * The 2D device keeps the transport, the queues, the 2D resources and the picture. It hands
 * every control command to vtgpu3d_cmd() first and gives the calls below.
 */
#ifndef _PCI_VIRTIO_GPU_3D_H_
#define	_PCI_VIRTIO_GPU_3D_H_

#include <sys/types.h>
#include <stdint.h>

struct pci_devinst;
struct vtgpu3d;

/* what the 2D device does for the 3D part; all called without the device's lock held */
struct vtgpu3d_host {
	void	*arg;
	void	(*lock)(void *arg);
	void	(*unlock)(void *arg);
	/*
	 * A command vtgpu3d_cmd() answered VTGPU3D_DEFERRED for is done: write its response
	 * (OK_NODATA with the request's fence fields) and release its chain. Lock held.
	 */
	void	(*complete)(void *arg, uint64_t token);
	/*
	 * The scanout's pixels: `rows` rows of `width` pixels in virtio-gpu format `format`
	 * from row `y`, `stride` bytes apart; width 0 = no scanout any more. Lock held.
	 */
	void	(*present)(void *arg, const uint8_t *pix, uint32_t format, uint32_t width,
		    uint32_t height, uint32_t stride, uint32_t y, uint32_t rows);
};

#define	VTGPU3D_UNHANDLED	0u		/* not a 3D command or resource: the 2D device's */
#define	VTGPU3D_DEFERRED	0xffffffffu	/* answered later through host->complete(token) */

/*
 * Starts the renderer and adds the host-visible memory window (a 64-bit BAR `bar_hostmem` of
 * `hostmem` bytes and its virtio shared memory capability) to pi. NULL: no 3D (the reason is
 * logged), the device goes on as 2D.
 */
struct vtgpu3d *vtgpu3d_init(struct pci_devinst *pi, int bar_hostmem, uint64_t hostmem,
	    const struct vtgpu3d_host *host);
uint64_t vtgpu3d_features(struct vtgpu3d *g);		/* to offer besides the 2D ones */
uint32_t vtgpu3d_num_capsets(struct vtgpu3d *g);	/* the device config's num_capsets */
/*
 * One control command (`req`: the request with its header, gathered; 4-byte aligned). Returns
 * the response type with *rlen bytes in resp (the header included, its type and fence fields
 * are the caller's), VTGPU3D_UNHANDLED or VTGPU3D_DEFERRED. The device's lock is held.
 */
uint32_t vtgpu3d_cmd(struct vtgpu3d *g, uint8_t *req, size_t len, uint8_t *resp, size_t cap,
	    size_t *rlen, uint64_t token);
void	vtgpu3d_reset(struct vtgpu3d *g);		/* the driver reset the device; lock held */
/* the guest moved the window's BAR (pe_baraddr): the mapped blobs go to the new address */
void	vtgpu3d_baraddr(struct vtgpu3d *g, int enabled, uint64_t addr);

#endif
