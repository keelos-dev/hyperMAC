/* keel: virtio-gpu for bhyve (pci_virtio_gpu.c, docs/25), as fbuf's virtio-vga part too. */
#ifndef _KEEL_PCI_VIRTIO_GPU_H_
#define	_KEEL_PCI_VIRTIO_GPU_H_

#include <sys/types.h>
#include <sys/nv.h>

struct pci_devinst;

/*
 * The shared memory /keel-gpu-<vm name> for keel-rdp (rdp/src/guest.rs): this header, the pixels
 * at KGPU_PIXELS (BGRX, stride = width * 4), the cursor (64x64 BGRA) after them. Written by
 * keel's virtio-gpu and, for a guest with an Intel onboard GPU, by keel_igpu.c (docs/71 §3).
 */
#define	KGPU_MAXW	3840
#define	KGPU_MAXH	2160
#define	KGPU_MAGIC	0x3170676bu	/* "kgp1" */
#define	KGPU_PIXELS	4096
#define	KGPU_CURSOR	(KGPU_PIXELS + KGPU_MAXW * KGPU_MAXH * 4)
#define	KGPU_SIZE	(KGPU_CURSOR + 64 * 64 * 4)
#define	KGPU_HOT_UNKNOWN	0xffffffffu
struct kgpu_shm {
	uint32_t magic;
	volatile uint32_t seq;		/* bumped after the pixels changed */
	volatile uint32_t active;	/* the guest's driver shows a scanout */
	volatile uint32_t width, height;	/* of the scanout; stride = width * 4, BGRX */
	volatile uint32_t req_seq;	/* keel-rdp: bumped with a new req_width/height */
	volatile uint32_t req_width, req_height;
	volatile uint32_t cur_seq;	/* cursor image or position changed */
	volatile uint32_t cur_visible;
	volatile int32_t cur_x, cur_y;
	volatile uint32_t cur_hot_x, cur_hot_y;	/* KGPU_HOT_UNKNOWN: the source has none
							 * (an Intel cursor plane, keel_igpu.c: x, y
							 * are the image's top left; keel-rdp's
							 * pointer.rs estimates it) */
	volatile uint32_t pref_width, pref_height;	/* what the guest was told */
	volatile uint32_t debug;	/* set from outside: 1 log every command to bhyve.log, 2 also
					 * the queues' state twice a second */
	volatile uint32_t unreadable;	/* keel_igpu.c: the guest's plane is on but cannot be read
					 * (render-compressed on Gen12, docs/71 §3.7); active is 0.
					 * keel-rdp says so instead of "display is off" */
};

void	*keel_vtgpu_attach(struct pci_devinst *pi, nvlist_t *nvl, int bar_regs, int bar_msix);
int	keel_vtgpu_owns(void *sc, int baridx);
void	keel_vtgpu_barwrite(void *sc, struct pci_devinst *pi, int baridx, uint64_t off, int size,
	    uint64_t value);
uint64_t keel_vtgpu_barread(void *sc, struct pci_devinst *pi, int baridx, uint64_t off, int size);
int	keel_vtgpu_scanout(void *sc, void **pix, int *w, int *h);
#ifdef BHYVE_SNAPSHOT
struct vm_snapshot_meta;
int	keel_vtgpu_snapshot(void *sc, struct vm_snapshot_meta *meta);
#endif

#endif
