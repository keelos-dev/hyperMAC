/*
 * libvmmapi on Hypervisor.framework: the vm_*() calls bhyve/arm64 makes (freebsd/lib/libvmmapi/vmmapi.h),
 * which on FreeBSD are ioctls on /dev/vmm/NAME served by the kernel's vmm (sys/arm64/vmm). This file
 * is that kernel's counterpart: what it says about "FreeBSD" is what sys/arm64/vmm does.
 *
 * What Hypervisor.framework does by itself here (measured on an M4, macOS 27.2; docs/bhyve-plan.md B1):
 *  - the GICv3 (hv_gic_*): distributor and redistributor accesses never reach us;
 *  - the virtual timer: it raises PPI 27 through that GIC, no exit;
 *  - WFI: the vCPU blocks inside hv_vcpu_run() until an interrupt is pending;
 *  - the PMU registers, once ID_AA64DFR0_EL1.PMUVer says there is a PMU (else they are undefined
 *    instructions in the guest, which Windows does not survive).
 * What is left for us: guest memory, the vCPU threads' register access, decoding MMIO faults for
 * bhyve's device models, PSCI calls (HVC), and stopping vCPUs (suspend, reset, the debugger).
 *
 * Threads: Hypervisor.framework ties a vCPU to the thread that created it: only that thread may run
 * it or touch its registers. bhyve reads and writes registers from other threads (the main thread
 * sets the boot CPU's PC before its thread exists, PSCI CPU_ON sets another CPU's entry point, the
 * debugger reads everything). So each vCPU is created by its own thread on the first vm_run(), and
 * keeps a copy of bhyve's registers ("cache") that other threads use while the vCPU is stopped:
 * see vcpu_pause().
 */
#include <sys/types.h>
#include <sys/cpuset.h>
#include <sys/mman.h>

#include <assert.h>
#include <err.h>
#include <errno.h>
#include <libutil.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#include <Hypervisor/Hypervisor.h>

#include <machine/armreg.h>
#include <machine/vmm.h>
#include <machine/vmm_dev.h>
#include <dev/vmm/vmm_mem.h>
#include <vmmapi.h>

#include "vmmapi_hvf.h"

#define	MB	(1024 * 1024UL)
#define	GB	(1024 * 1024 * 1024UL)

/* The guest's RAM starts here, as in FreeBSD's libvmmapi on arm64 (nothing below 4 GB is RAM). */
#define	VM_HIGHMEM_BASE		(4 * GB)
/* Unmapped address space before and after each mapping of guest memory in our process. */
#define	VM_MMAP_GUARD_SIZE	(4 * MB)

#define	HVF_IPA_BITS		40	/* the most an M4 gives; the default is 36 */
#define	HVF_MAXCPU		64

#define	GICD_SETSPI_NSR		0x40	/* the register an MSI "writes" (FreeBSD's vgic takes MSIs there) */
#define	GICV2M_MSI_SETSPI_NS	0x40	/* the same in a GICv2m frame */
#define	GIC_FIRST_SPI		32
#define	HVF_NSPI		1024	/* room for every SPI a GICv3 can have (INTIDs 32..1019) */

struct memseg {
	char	*ptr;			/* in our address space; NULL: not allocated */
	size_t	len;
};

struct memmap {
	vm_paddr_t gpa;
	size_t	len;			/* 0: free slot */
	int	segid;
	vm_ooffset_t segoff;
	int	prot;
};

struct vcpu;

struct vmctx {
	char	*name;
	int	memflags;
	size_t	lowmem_size;
	size_t	highmem_size;
	char	*baseaddr;		/* baseaddr + gpa is the host address of guest RAM */
	uint16_t sockets, cores, threads;

	pthread_mutex_t mtx;		/* the fields below */
	pthread_cond_t cv;
	struct memseg memsegs[VM_MAX_MEMSEGS];
	struct memmap memmaps[VM_MAX_MEMMAPS];
	struct vcpu *vcpus[HVF_MAXCPU];
	int	nopened;		/* vm_vcpu_open() calls */
	int	ncreated;		/* of them, those with a Hypervisor.framework vCPU */
	cpuset_t active_cpus;
	cpuset_t debug_cpus;		/* stopped by vm_suspend_cpu(): vm_run() returns VM_EXITCODE_DEBUG */
	cpuset_t suspended_cpus;	/* have seen `suspend` */
	_Atomic int suspend;		/* enum vm_suspend_how once the VM is reset or powered off */

	bool	watch;			/* HVF_WATCH is set */
	bool	gic_attached;
	uint64_t gic_dist_start;
	uint64_t gic_msi_base;		/* the GICv2m frame; 0: none */
	size_t	gic_msi_size;
	uint32_t gic_msi_spi_base, gic_msi_spi_count;
	pthread_mutex_t spi_mtx;
	uint64_t spi_level[HVF_NSPI / 64];	/* the level each SPI's line was last set to */
};

struct vcpu {
	struct vmctx *ctx;
	int	vcpuid;

	pthread_mutex_t mtx;
	pthread_cond_t cv;
	bool	created;		/* hv is valid; set once, by the owner */
	pthread_t owner;
	hv_vcpu_t hv;
	hv_vcpu_exit_t *exit;

	/*
	 * bhyve's registers (enum vm_reg_name) for threads other than the owner. `synced`: the cache
	 * holds the vCPU's state (true until the vCPU exists, and whenever it is stopped for the
	 * debugger or a pauser). `dirty`: values written into the cache that the owner has to put
	 * into the vCPU before the guest runs again.
	 */
	uint64_t regs[VM_REG_LAST];
	uint64_t dirty;
	bool	synced;
	int	pausers;		/* threads inside vcpu_pause() ... vcpu_unpause() */

	bool	exc_pending;		/* vm_inject_exception(): taken at the next entry */
	uint64_t exc_esr, exc_far;
	int	caps;			/* 1 << VM_CAP_* */
	bool	caps_dirty;
	/* a sign-extending load from MMIO that bhyve has just emulated: finished at the next entry */
	bool	sext_pending;
	int	sext_reg, sext_bits;
	bool	sext_64;
	/* the second register of a load/store pair on MMIO: bhyve emulates it at the next vm_run() */
	bool	pair_pending;
	struct vm_exit pair_exit;
	uint64_t watch_esr, watch_far, watch_elr, watch_vbar, watch_x30;	/* HVF_WATCH: read by the owner when it syncs */
};

/*
 * Errors
 */

static int
hv_errno(hv_return_t r)
{
	switch (r) {
	case HV_SUCCESS:
		return (0);
	case HV_BAD_ARGUMENT:
	case HV_ILLEGAL_GUEST_STATE:
		return (EINVAL);
	case HV_BUSY:
		return (EBUSY);
	case HV_NO_RESOURCES:
		return (ENOMEM);
	case HV_NO_DEVICE:
		return (ENODEV);
	case HV_DENIED:
		return (EPERM);		/* the binary lacks com.apple.security.hypervisor */
	case HV_UNSUPPORTED:
		return (ENOTSUP);
	default:
		return (EIO);
	}
}

/* 0, or -1 with errno: what the ioctl would have returned */
static int
hv_ret(hv_return_t r)
{
	if (r == HV_SUCCESS)
		return (0);
	errno = hv_errno(r);
	return (-1);
}

/* A call that cannot fail unless this file is wrong. */
#define	HV_CHECK(call)	do {						\
	hv_return_t r_ = (call);					\
	if (r_ != HV_SUCCESS)						\
		errx(4, "hvf: %s: error %#x", #call, (unsigned)r_);	\
} while (0)

static void *watch_thread(void *arg);

/*
 * Lifetime
 */

struct vmctx *
vm_openf(const char *name, int flags __unused)
{
	struct vmctx *ctx;
	hv_vm_config_t config;
	uint32_t max_ipa;
	hv_return_t r;

	/*
	 * One VM per process, made here and gone with the process: there is nothing to find again
	 * after a reboot, so VMMAPI_OPEN_CREATE and _REINIT mean the same.
	 */
	config = hv_vm_config_create();
	if (hv_vm_config_get_max_ipa_size(&max_ipa) != HV_SUCCESS || max_ipa < HVF_IPA_BITS) {
		warnx("hvf: this Mac's guest physical addresses are %u bits wide, %d are needed",
		    max_ipa, HVF_IPA_BITS);
		errno = ENOTSUP;
		return (NULL);
	}
	HV_CHECK(hv_vm_config_set_ipa_size(config, HVF_IPA_BITS));
	/* 4K stage-2 pages: guest memory is mapped where the guest puts it, on its 4K boundaries */
	HV_CHECK(hv_vm_config_set_ipa_granule(config, HV_IPA_GRANULE_4KB));
	r = hv_vm_create(config);
	os_release(config);
	if (r != HV_SUCCESS) {
		errno = hv_errno(r);
		return (NULL);
	}

	ctx = calloc(1, sizeof(*ctx));
	if (ctx == NULL)
		return (NULL);
	ctx->name = strdup(name);
	pthread_mutex_init(&ctx->mtx, NULL);
	pthread_cond_init(&ctx->cv, NULL);
	pthread_mutex_init(&ctx->spi_mtx, NULL);
	if (getenv("HVF_WATCH") != NULL) {
		pthread_t tid;
		uint32_t *trap;
		size_t i;

		ctx->watch = true;
		pthread_create(&tid, NULL, watch_thread, ctx);
		/*
		 * Firmware that takes an exception before it has set VBAR_EL1 jumps to address
		 * 0x0..0x7ff and is lost there. A page of "hvc #0x7777" at 0 catches it with
		 * ESR_EL1, ELR_EL1 and FAR_EL1 still telling what happened: handle_exception().
		 */
		if (getenv("HVF_WATCH_NOTRAP") == NULL) {
			trap = mmap(NULL, 0x4000, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
			for (i = 0; i < 0x4000 / 4; i++)
				trap[i] = 0xd40eeee2;
			(void)hv_vm_map(trap, 0, 0x4000, HV_MEMORY_READ | HV_MEMORY_EXEC);
		}
	}
	return (ctx);
}

void
vm_destroy(struct vmctx *ctx __unused)
{
	/*
	 * FreeBSD: remove the VM from the kernel. Here it goes with the process, and bhyve exits
	 * right after this; the vCPUs could only be destroyed by their own threads anyway.
	 */
}

int
vm_reinit(struct vmctx *ctx __unused)
{
	/* A reset is a new process (bhyve exits with BHYVE_EXIT_RESET), so the VM is always fresh. */
	return (0);
}

int
vm_limit_rights(struct vmctx *ctx __unused)
{
	return (0);
}

const char *
vm_get_name(struct vmctx *ctx)
{
	return (ctx->name);
}

/*
 * Memory
 */

int
vm_parse_memsize(const char *opt, size_t *ret_memsize)
{
	char *endptr;
	size_t optval;
	uint64_t val;
	int error;

	/* as FreeBSD's: a plain number below 1 MB is a count of megabytes */
	optval = strtoul(opt, &endptr, 0);
	if (*opt != '\0' && *endptr == '\0') {
		if (optval < MB)
			optval *= MB;
		*ret_memsize = optval;
		return (0);
	}
	error = expand_number(opt, &val);
	if (error == 0)
		*ret_memsize = val;
	return (error);
}

void
vm_set_memflags(struct vmctx *ctx, int flags)
{
	/* VM_MEM_F_WIRED and _INCORE have no effect here */
	ctx->memflags = flags;
}

int
vm_get_memflags(struct vmctx *ctx)
{
	return (ctx->memflags);
}

uint32_t
vm_get_lowmem_limit(struct vmctx *ctx __unused)
{
	return (0);
}

size_t
vm_get_lowmem_size(struct vmctx *ctx)
{
	return (ctx->lowmem_size);
}

vm_paddr_t
vm_get_highmem_base(struct vmctx *ctx __unused)
{
	return (VM_HIGHMEM_BASE);
}

size_t
vm_get_highmem_size(struct vmctx *ctx)
{
	return (ctx->highmem_size);
}

static hv_memory_flags_t
hv_prot(int prot)
{
	hv_memory_flags_t f = 0;

	if (prot & PROT_READ)
		f |= HV_MEMORY_READ;
	if (prot & PROT_WRITE)
		f |= HV_MEMORY_WRITE;
	if (prot & PROT_EXEC)
		f |= HV_MEMORY_EXEC;
	return (f);
}

/* Record [gpa, gpa + len) -> segment and put it in the guest's address space. ctx->mtx held. */
static int
memmap_add(struct vmctx *ctx, vm_paddr_t gpa, int segid, vm_ooffset_t segoff, size_t len, int prot)
{
	struct memseg *seg;
	struct memmap *mm;
	int i;

	if (segid < 0 || segid >= VM_MAX_MEMSEGS || (seg = &ctx->memsegs[segid])->ptr == NULL ||
	    segoff < 0 || (size_t)segoff + len > seg->len || len == 0 ||
	    ((gpa | len | segoff) & PAGE_MASK) != 0) {
		errno = EINVAL;
		return (-1);
	}
	for (i = 0, mm = NULL; i < VM_MAX_MEMMAPS; i++) {
		if (ctx->memmaps[i].len == 0) {
			mm = &ctx->memmaps[i];
			break;
		}
	}
	if (mm == NULL) {
		errno = ENOSPC;
		return (-1);
	}
	if (hv_ret(hv_vm_map(seg->ptr + segoff, gpa, len, hv_prot(prot))) != 0)
		return (-1);
	mm->gpa = gpa;
	mm->len = len;
	mm->segid = segid;
	mm->segoff = segoff;
	mm->prot = prot;
	return (0);
}

/*
 * `len` bytes of zeroed memory for a segment, with nothing mapped on either side so that a stray
 * access from a device model faults instead of landing in another mapping. With `base` the memory
 * goes there (inside a reservation the caller made).
 */
static char *
seg_alloc(char *base, size_t len)
{
	char *p;

	if (base == NULL) {
		p = mmap(NULL, VM_MMAP_GUARD_SIZE + len + VM_MMAP_GUARD_SIZE, PROT_NONE,
		    MAP_ANON | MAP_PRIVATE, -1, 0);
		if (p == MAP_FAILED)
			return (MAP_FAILED);
		base = p + VM_MMAP_GUARD_SIZE;
	}
	return (mmap(base, len, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE | MAP_FIXED, -1, 0));
}

int
vm_setup_memory_domains(struct vmctx *ctx, enum vm_mmap_style vms, struct vm_mem_domain *doms,
    int ndoms)
{
	size_t totalsize;
	char *reserve, *p;
	vm_paddr_t gpa;
	int i, error;

	assert(vms == VM_MMAP_ALL);
	if (doms == NULL || ndoms <= 0 || ndoms > VM_MAXMEMDOM) {
		errno = EINVAL;
		return (-1);
	}
	totalsize = 0;
	for (i = 0; i < ndoms; i++) {
		if (doms[i].size == 0 || (doms[i].size & PAGE_MASK) != 0) {
			errno = EINVAL;
			return (-1);
		}
		totalsize += doms[i].size;
	}

	/*
	 * As FreeBSD: one stretch of our address space covers the guest's physical addresses from
	 * 0 to the end of RAM, so that host address = baseaddr + guest address. Only RAM
	 * (from VM_HIGHMEM_BASE up) gets memory; the 4 GB below it stay reserved and unmapped.
	 */
	reserve = mmap(NULL, VM_MMAP_GUARD_SIZE + VM_HIGHMEM_BASE + totalsize + VM_MMAP_GUARD_SIZE,
	    PROT_NONE, MAP_ANON | MAP_PRIVATE, -1, 0);
	if (reserve == MAP_FAILED)
		return (-1);
	ctx->baseaddr = reserve + VM_MMAP_GUARD_SIZE;

	/* The domains (NUMA nodes of the guest) follow each other; a Mac has no nodes to back them with. */
	pthread_mutex_lock(&ctx->mtx);
	gpa = VM_HIGHMEM_BASE;
	error = 0;
	for (i = 0; i < ndoms && error == 0; i++) {
		p = seg_alloc(ctx->baseaddr + gpa, doms[i].size);
		if (p == MAP_FAILED) {
			error = -1;
			break;
		}
		ctx->memsegs[VM_SYSMEM + i].ptr = p;
		ctx->memsegs[VM_SYSMEM + i].len = doms[i].size;
		error = memmap_add(ctx, gpa, VM_SYSMEM + i, 0, doms[i].size, PROT_READ | PROT_WRITE | PROT_EXEC);
		gpa += doms[i].size;
	}
	pthread_mutex_unlock(&ctx->mtx);
	if (error != 0)
		return (-1);
	ctx->lowmem_size = 0;
	ctx->highmem_size = totalsize;
	return (0);
}

/*
 * As FreeBSD's: the host address of guest RAM [gaddr, gaddr + len), or NULL if any of it is not
 * RAM (bhyve's instruction emulation depends on NULL for MMIO).
 */
void *
vm_map_gpa(struct vmctx *ctx, vm_paddr_t gaddr, size_t len)
{
	size_t highsize = ctx->highmem_size;

	if (highsize > 0 && gaddr >= VM_HIGHMEM_BASE) {
		if (gaddr < VM_HIGHMEM_BASE + highsize && len <= highsize &&
		    gaddr + len <= VM_HIGHMEM_BASE + highsize)
			return (ctx->baseaddr + gaddr);
	}
	return (NULL);
}

/* Memory that is not RAM (the boot ROM, a framebuffer): allocated here, mapped by vm_mmap_memseg(). */
void *
vm_create_devmem(struct vmctx *ctx, int segid, const char *name, size_t len)
{
	struct memseg *seg;
	char *p;

	if (name == NULL || name[0] == '\0' || segid < VM_MAXSYSMEM || segid >= VM_MAX_MEMSEGS ||
	    len == 0 || (len & PAGE_MASK) != 0) {
		errno = EINVAL;
		return (MAP_FAILED);
	}
	pthread_mutex_lock(&ctx->mtx);
	seg = &ctx->memsegs[segid];
	if (seg->ptr != NULL) {
		pthread_mutex_unlock(&ctx->mtx);
		errno = EEXIST;
		return (MAP_FAILED);
	}
	p = seg_alloc(NULL, len);
	if (p != MAP_FAILED) {
		seg->ptr = p;
		seg->len = len;
	}
	pthread_mutex_unlock(&ctx->mtx);
	return (p);
}

int
vm_mmap_memseg(struct vmctx *ctx, vm_paddr_t gpa, int segid, vm_ooffset_t segoff, size_t len,
    int prot)
{
	int error;

	pthread_mutex_lock(&ctx->mtx);
	error = memmap_add(ctx, gpa, segid, segoff, len, prot);
	pthread_mutex_unlock(&ctx->mtx);
	return (error);
}

int
vm_munmap_memseg(struct vmctx *ctx, vm_paddr_t gpa, size_t len)
{
	struct memmap *mm;
	int i, error;

	pthread_mutex_lock(&ctx->mtx);
	errno = EINVAL;
	error = -1;
	for (i = 0; i < VM_MAX_MEMMAPS; i++) {
		mm = &ctx->memmaps[i];
		if (mm->len != 0 && mm->gpa == gpa && mm->len == len) {
			error = hv_ret(hv_vm_unmap(gpa, len));
			if (error == 0)
				mm->len = 0;
			break;
		}
	}
	pthread_mutex_unlock(&ctx->mtx);
	return (error);
}

/* The mapping with the lowest address at or above *gpa; ENOENT after the last one. */
int
vm_mmap_getnext(struct vmctx *ctx, vm_paddr_t *gpa, int *segid, vm_ooffset_t *segoff, size_t *len,
    int *prot, int *flags)
{
	struct memmap *mm, *best;
	int i;

	pthread_mutex_lock(&ctx->mtx);
	best = NULL;
	for (i = 0; i < VM_MAX_MEMMAPS; i++) {
		mm = &ctx->memmaps[i];
		if (mm->len == 0 || mm->gpa < *gpa)
			continue;
		if (best == NULL || mm->gpa < best->gpa)
			best = mm;
	}
	if (best != NULL) {
		*gpa = best->gpa;
		*segid = best->segid;
		*segoff = best->segoff;
		*len = best->len;
		*prot = best->prot;
		*flags = 0;
	}
	pthread_mutex_unlock(&ctx->mtx);
	if (best == NULL) {
		errno = ENOENT;
		return (-1);
	}
	return (0);
}

int
vm_gla2gpa_nofault(struct vcpu *vcpu __unused, struct vm_guest_paging *paging __unused,
    uint64_t gla __unused, int prot __unused, uint64_t *gpa __unused, int *fault __unused)
{
	/* Only the debugger asks (reading guest memory by virtual address): not done yet. */
	errno = ENOSYS;
	return (-1);
}

/*
 * vCPU registers
 */

/* The registers bhyve names that Hypervisor.framework keeps as system registers. */
static bool
reg_sysreg(int reg, hv_sys_reg_t *sr)
{
	switch (reg) {
	case VM_REG_GUEST_SP:
		*sr = HV_SYS_REG_SP_EL1;	/* FreeBSD: "SP" is SP_EL1 */
		return (true);
	case VM_REG_GUEST_SCTLR_EL1:
		*sr = HV_SYS_REG_SCTLR_EL1;
		return (true);
	case VM_REG_GUEST_TTBR0_EL1:
		*sr = HV_SYS_REG_TTBR0_EL1;
		return (true);
	case VM_REG_GUEST_TTBR1_EL1:
		*sr = HV_SYS_REG_TTBR1_EL1;
		return (true);
	case VM_REG_GUEST_TCR_EL1:
		*sr = HV_SYS_REG_TCR_EL1;
		return (true);
	case VM_REG_GUEST_MPIDR_EL1:
		*sr = HV_SYS_REG_MPIDR_EL1;
		return (true);
	default:
		return (false);
	}
}

/* Owner thread only. */
static uint64_t
hvreg_get(struct vcpu *v, int reg)
{
	hv_sys_reg_t sr;
	uint64_t val = 0;

	if (reg >= VM_REG_GUEST_X0 && reg <= VM_REG_GUEST_LR)
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_X0 + (reg - VM_REG_GUEST_X0), &val));
	else if (reg == VM_REG_GUEST_PC)
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_PC, &val));
	else if (reg == VM_REG_GUEST_CPSR)
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_CPSR, &val));
	else if (reg_sysreg(reg, &sr))
		HV_CHECK(hv_vcpu_get_sys_reg(v->hv, sr, &val));
	else
		val = v->regs[reg];	/* TCR2_EL1: Hypervisor.framework has no name for it */
	return (val);
}

/* Owner thread only. */
static void
hvreg_set(struct vcpu *v, int reg, uint64_t val)
{
	hv_sys_reg_t sr;

	if (reg >= VM_REG_GUEST_X0 && reg <= VM_REG_GUEST_LR)
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_X0 + (reg - VM_REG_GUEST_X0), val));
	else if (reg == VM_REG_GUEST_PC)
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, val));
	else if (reg == VM_REG_GUEST_CPSR)
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_CPSR, val));
	else if (reg_sysreg(reg, &sr))
		HV_CHECK(hv_vcpu_set_sys_reg(v->hv, sr, val));
	else
		v->regs[reg] = val;
}

static bool
vcpu_is_owner(struct vcpu *v)
{
	return (v->created && pthread_equal(v->owner, pthread_self()));
}

static void
vcpu_kick(struct vcpu *v)
{
	/*
	 * Makes hv_vcpu_run() return (HV_EXIT_REASON_CANCELED), also out of a WFI. A kick that
	 * comes before the vCPU enters the guest is not lost: the next hv_vcpu_run() returns at once.
	 */
	if (v->created)
		(void)hv_vcpus_exit(&v->hv, 1);
}

/*
 * For a thread that is not the vCPU's owner: stop the vCPU and wait until its registers are in the
 * cache. Returns with v->mtx held; the vCPU stays out of the guest until vcpu_unpause().
 * A vCPU that is out in bhyve handling an exit gets here when it calls vm_run() again.
 */
static void
vcpu_pause(struct vcpu *v)
{
	struct timespec ts;

	pthread_mutex_lock(&v->mtx);
	v->pausers++;
	while (!v->synced) {
		vcpu_kick(v);
		clock_gettime(CLOCK_REALTIME, &ts);
		ts.tv_nsec += 10 * 1000 * 1000;
		if (ts.tv_nsec >= 1000000000) {
			ts.tv_sec++;
			ts.tv_nsec -= 1000000000;
		}
		pthread_cond_timedwait(&v->cv, &v->mtx, &ts);
	}
}

static void
vcpu_unpause(struct vcpu *v)
{
	v->pausers--;
	pthread_cond_broadcast(&v->cv);
	pthread_mutex_unlock(&v->mtx);
}

/* Owner, v->mtx held: the vCPU's registers into the cache (values waiting in the cache stay). */
static void
vcpu_sync(struct vcpu *v)
{
	int reg;

	if (v->synced)
		return;
	for (reg = 0; reg < VM_REG_LAST; reg++) {
		if ((v->dirty & (1ul << reg)) == 0)
			v->regs[reg] = hvreg_get(v, reg);
	}
	if (v->ctx->watch) {
		(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_ESR_EL1, &v->watch_esr);
		(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_FAR_EL1, &v->watch_far);
		(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_ELR_EL1, &v->watch_elr);
		(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_VBAR_EL1, &v->watch_vbar);
		(void)hv_vcpu_get_reg(v->hv, HV_REG_LR, &v->watch_x30);
	}
	v->synced = true;
	pthread_cond_broadcast(&v->cv);
}

int
vm_set_register(struct vcpu *v, int reg, uint64_t val)
{
	if (reg < 0 || reg >= VM_REG_LAST) {
		errno = EINVAL;
		return (-1);
	}
	if (vcpu_is_owner(v)) {
		pthread_mutex_lock(&v->mtx);
		hvreg_set(v, reg, val);
		v->regs[reg] = val;
		v->dirty &= ~(1ul << reg);
		pthread_mutex_unlock(&v->mtx);
	} else {
		vcpu_pause(v);
		v->regs[reg] = val;
		v->dirty |= 1ul << reg;
		vcpu_unpause(v);
	}
	return (0);
}

int
vm_get_register(struct vcpu *v, int reg, uint64_t *retval)
{
	if (reg < 0 || reg >= VM_REG_LAST) {
		errno = EINVAL;
		return (-1);
	}
	if (vcpu_is_owner(v)) {
		pthread_mutex_lock(&v->mtx);
		if (v->dirty & (1ul << reg))
			*retval = v->regs[reg];
		else
			*retval = hvreg_get(v, reg);
		pthread_mutex_unlock(&v->mtx);
	} else {
		vcpu_pause(v);
		*retval = v->regs[reg];
		vcpu_unpause(v);
	}
	return (0);
}

int
vm_set_register_set(struct vcpu *vcpu, unsigned int count, const int *regnums, uint64_t *regvals)
{
	unsigned int i;

	for (i = 0; i < count; i++)
		if (vm_set_register(vcpu, regnums[i], regvals[i]) != 0)
			return (-1);
	return (0);
}

int
vm_get_register_set(struct vcpu *vcpu, unsigned int count, const int *regnums, uint64_t *regvals)
{
	unsigned int i;

	for (i = 0; i < count; i++)
		if (vm_get_register(vcpu, regnums[i], &regvals[i]) != 0)
			return (-1);
	return (0);
}

/*
 * vCPU lifetime
 */

struct vcpu *
vm_vcpu_open(struct vmctx *ctx, int vcpuid)
{
	struct vcpu *v;

	if (vcpuid < 0 || vcpuid >= HVF_MAXCPU) {
		errno = EINVAL;
		return (NULL);
	}
	v = calloc(1, sizeof(*v));
	if (v == NULL)
		return (NULL);
	v->ctx = ctx;
	v->vcpuid = vcpuid;
	pthread_mutex_init(&v->mtx, NULL);
	pthread_cond_init(&v->cv, NULL);

	/*
	 * The state a vCPU starts in, as FreeBSD's vmm_reset.c: EL1 with its own stack pointer,
	 * interrupts masked, MMU and caches off. The MPIDR is FreeBSD's too (16 CPUs per Aff0).
	 */
	v->regs[VM_REG_GUEST_CPSR] = PSR_D | PSR_A | PSR_I | PSR_F | PSR_M_EL1h;
	v->regs[VM_REG_GUEST_SCTLR_EL1] = (SCTLR_RES1 & ~(SCTLR_M | SCTLR_C | SCTLR_I)) | SCTLR_CP15BEN;
	v->regs[VM_REG_GUEST_MPIDR_EL1] = (1ul << 31) |
	    ((uint64_t)(vcpuid & 0xf) << MPIDR_Aff0_SHIFT) |
	    ((uint64_t)((vcpuid >> 4) & 0xff) << MPIDR_Aff1_SHIFT);
	v->dirty = (1ul << VM_REG_LAST) - 1;	/* all of it goes into the vCPU when it is created */
	v->synced = true;

	pthread_mutex_lock(&ctx->mtx);
	ctx->vcpus[vcpuid] = v;
	ctx->nopened++;
	pthread_mutex_unlock(&ctx->mtx);
	return (v);
}

void
vm_vcpu_close(struct vcpu *vcpu __unused)
{
	/* bhyve never closes one while the VM lives */
}

int
vcpu_id(struct vcpu *vcpu)
{
	return (vcpu->vcpuid);
}

/*
 * The calling thread becomes the vCPU's owner: called by the first vm_run().
 *
 * The GIC gives each vCPU a redistributor when the vCPU is created, in that order, and finds the
 * vCPU by its MPIDR, which therefore has to be set (and unique) before anything else. The vCPUs
 * are created in the order of their ids so that the redistributor frames are in the order the
 * device tree implies.
 */
static void
vcpu_create(struct vcpu *v)
{
	struct vmctx *ctx = v->ctx;
	uint64_t dfr0;
	int i, lower;

	pthread_mutex_lock(&ctx->mtx);
	for (;;) {
		for (i = 0, lower = 0; i < v->vcpuid; i++)
			if (ctx->vcpus[i] != NULL && !ctx->vcpus[i]->created)
				lower++;
		if (lower == 0)
			break;
		pthread_cond_wait(&ctx->cv, &ctx->mtx);
	}
	HV_CHECK(hv_vcpu_create(&v->hv, &v->exit, NULL));
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_MPIDR_EL1, v->regs[VM_REG_GUEST_MPIDR_EL1]));
	/*
	 * Say there is a PMU (PMUv3): Hypervisor.framework then emulates its registers. Without
	 * this they are undefined instructions, and Windows reads them unconditionally.
	 */
	HV_CHECK(hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_ID_AA64DFR0_EL1, &dfr0));
	dfr0 = (dfr0 & ~ID_AA64DFR0_PMUVer_MASK) | ID_AA64DFR0_PMUVer_3;
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_ID_AA64DFR0_EL1, dfr0));

	pthread_mutex_lock(&v->mtx);
	v->owner = pthread_self();
	v->created = true;
	pthread_mutex_unlock(&v->mtx);
	ctx->ncreated++;
	pthread_cond_broadcast(&ctx->cv);
	pthread_mutex_unlock(&ctx->mtx);
}

/* No vCPU enters the guest before all of them exist: see vcpu_create() about the redistributors. */
static void
vcpus_wait_created(struct vmctx *ctx)
{
	pthread_mutex_lock(&ctx->mtx);
	while (ctx->ncreated < ctx->nopened)
		pthread_cond_wait(&ctx->cv, &ctx->mtx);
	pthread_mutex_unlock(&ctx->mtx);
}

int
vm_set_topology(struct vmctx *ctx, uint16_t sockets, uint16_t cores, uint16_t threads,
    uint16_t maxcpus)
{
	/* FreeBSD: `maxcpus` is ignored, the limit is the kernel's (see vm_get_topology()) */
	ctx->sockets = sockets;
	ctx->cores = cores;
	ctx->threads = threads;
	return (0);
}

int
vm_get_topology(struct vmctx *ctx, uint16_t *sockets, uint16_t *cores, uint16_t *threads,
    uint16_t *maxcpus)
{
	uint32_t hv_max;

	*sockets = ctx->sockets;
	*cores = ctx->cores;
	*threads = ctx->threads;
	/* the most vCPUs a VM can have: Hypervisor.framework's limit, and this file's tables */
	HV_CHECK(hv_vm_get_max_vcpu_count(&hv_max));
	*maxcpus = hv_max < HVF_MAXCPU ? hv_max : HVF_MAXCPU;
	return (0);
}

int
vm_set_capability(struct vcpu *v, enum vm_cap_type cap, int val)
{
	switch (cap) {
	case VM_CAP_BRK_EXIT:
	case VM_CAP_SS_EXIT:
	case VM_CAP_MASK_HWINTR:
		break;
	default:
		errno = ENOENT;
		return (-1);
	}
	pthread_mutex_lock(&v->mtx);
	if (val != 0)
		v->caps |= 1 << cap;
	else
		v->caps &= ~(1 << cap);
	v->caps_dirty = true;	/* the owner applies it at the next entry: vcpu_enter() */
	pthread_mutex_unlock(&v->mtx);
	return (0);
}

int
vm_get_capability(struct vcpu *v, enum vm_cap_type cap, int *retval)
{
	switch (cap) {
	case VM_CAP_UNRESTRICTED_GUEST:
		*retval = 1;
		return (0);
	case VM_CAP_BRK_EXIT:
	case VM_CAP_SS_EXIT:
	case VM_CAP_MASK_HWINTR:
		*retval = (v->caps & (1 << cap)) != 0;
		return (0);
	default:
		errno = ENOENT;
		return (-1);
	}
}

int
vm_inject_exception(struct vcpu *v, uint64_t esr, uint64_t far)
{
	pthread_mutex_lock(&v->mtx);
	v->exc_esr = esr;
	v->exc_far = far;
	v->exc_pending = true;
	pthread_mutex_unlock(&v->mtx);
	return (0);
}

/*
 * Which vCPUs run
 */

int
vm_activate_cpu(struct vcpu *v)
{
	struct vmctx *ctx = v->ctx;
	int error = 0;

	pthread_mutex_lock(&ctx->mtx);
	if (CPU_ISSET(v->vcpuid, &ctx->active_cpus)) {
		errno = EBUSY;
		error = -1;
	} else
		CPU_SET(v->vcpuid, &ctx->active_cpus);
	pthread_mutex_unlock(&ctx->mtx);
	return (error);
}

/* ctx->mtx held */
static void
kick_active(struct vmctx *ctx)
{
	int i;

	for (i = 0; i < HVF_MAXCPU; i++)
		if (ctx->vcpus[i] != NULL && CPU_ISSET(i, &ctx->active_cpus))
			vcpu_kick(ctx->vcpus[i]);
}

int
vm_suspend_cpu(struct vcpu *v)
{
	struct vmctx *ctx = v->ctx;
	int error = 0;

	pthread_mutex_lock(&ctx->mtx);
	if (!CPU_ISSET(v->vcpuid, &ctx->active_cpus)) {
		errno = EINVAL;
		error = -1;
	} else {
		CPU_SET(v->vcpuid, &ctx->debug_cpus);
		vcpu_kick(v);
	}
	pthread_mutex_unlock(&ctx->mtx);
	return (error);
}

int
vm_resume_cpu(struct vcpu *v)
{
	struct vmctx *ctx = v->ctx;
	int error = 0;

	pthread_mutex_lock(&ctx->mtx);
	if (!CPU_ISSET(v->vcpuid, &ctx->debug_cpus)) {
		errno = EINVAL;
		error = -1;
	} else
		CPU_CLR(v->vcpuid, &ctx->debug_cpus);
	pthread_mutex_unlock(&ctx->mtx);
	return (error);
}

int
vm_suspend_all_cpus(struct vmctx *ctx)
{
	pthread_mutex_lock(&ctx->mtx);
	CPU_COPY(&ctx->active_cpus, &ctx->debug_cpus);
	kick_active(ctx);
	pthread_mutex_unlock(&ctx->mtx);
	return (0);
}

int
vm_resume_all_cpus(struct vmctx *ctx)
{
	pthread_mutex_lock(&ctx->mtx);
	CPU_ZERO(&ctx->debug_cpus);
	pthread_mutex_unlock(&ctx->mtx);
	return (0);
}

int
vm_active_cpus(struct vmctx *ctx, cpuset_t *cpus)
{
	pthread_mutex_lock(&ctx->mtx);
	CPU_COPY(&ctx->active_cpus, cpus);
	pthread_mutex_unlock(&ctx->mtx);
	return (0);
}

int
vm_debug_cpus(struct vmctx *ctx, cpuset_t *cpus)
{
	pthread_mutex_lock(&ctx->mtx);
	CPU_COPY(&ctx->debug_cpus, cpus);
	pthread_mutex_unlock(&ctx->mtx);
	return (0);
}

int
vm_suspended_cpus(struct vmctx *ctx, cpuset_t *cpus)
{
	pthread_mutex_lock(&ctx->mtx);
	CPU_COPY(&ctx->suspended_cpus, cpus);
	pthread_mutex_unlock(&ctx->mtx);
	return (0);
}

/* The whole VM stops (reset, power off): every vCPU's vm_run() returns VM_EXITCODE_SUSPENDED. */
int
vm_suspend(struct vmctx *ctx, enum vm_suspend_how how)
{
	int expected = 0;

	if (how <= VM_SUSPEND_NONE || how >= VM_SUSPEND_LAST) {
		errno = EINVAL;
		return (-1);
	}
	if (!atomic_compare_exchange_strong(&ctx->suspend, &expected, how)) {
		errno = EALREADY;
		return (-1);
	}
	pthread_mutex_lock(&ctx->mtx);
	kick_active(ctx);
	pthread_mutex_unlock(&ctx->mtx);
	return (0);
}

/*
 * Debugging aid: with HVF_WATCH=SECONDS in the environment a thread prints where every vCPU is
 * (PC, PSTATE, and for an exception taken in the guest its ESR_EL1/FAR_EL1/ELR_EL1) that often.
 * For firmware that says nothing: is it running, waiting, or stuck in its exception handler?
 */
static void *
watch_thread(void *arg)
{
	struct vmctx *ctx = arg;
	struct vcpu *v;
	unsigned int secs = atoi(getenv("HVF_WATCH"));
	uint64_t pc, cpsr;
	int i;

	for (;;) {
		sleep(secs > 0 ? secs : 2);
		for (i = 0; i < HVF_MAXCPU; i++) {
			if ((v = ctx->vcpus[i]) == NULL || !v->created)
				continue;
			if (vm_get_register(v, VM_REG_GUEST_PC, &pc) != 0 ||
			    vm_get_register(v, VM_REG_GUEST_CPSR, &cpsr) != 0)
				continue;
			warnx("hvf: watch: vcpu %d pc %#llx pstate %#llx esr_el1 %#llx far_el1 %#llx elr_el1 %#llx "
			    "sctlr_el1 %#llx vbar_el1 %#llx lr %#llx",
			    i, (unsigned long long)pc, (unsigned long long)cpsr,
			    (unsigned long long)v->watch_esr, (unsigned long long)v->watch_far,
			    (unsigned long long)v->watch_elr, (unsigned long long)v->regs[VM_REG_GUEST_SCTLR_EL1],
			    (unsigned long long)v->watch_vbar, (unsigned long long)v->watch_x30);
		}
	}
	return (NULL);
}

/*
 * Running a vCPU
 */

/*
 * Make the guest take a synchronous exception to EL1, as the CPU would: FreeBSD's vmm does this
 * in its world switch when vm_inject_exception() left one pending.
 */
static void
vcpu_take_exception(struct vcpu *v, uint64_t esr, uint64_t far)
{
	uint64_t cpsr, pc, vbar, off;

	HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_CPSR, &cpsr));
	HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_PC, &pc));
	HV_CHECK(hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_VBAR_EL1, &vbar));
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_ELR_EL1, pc));
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_SPSR_EL1, cpsr));
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_ESR_EL1, esr));
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_FAR_EL1, far));
	/* the vector: from EL0 (AArch64), from EL1 on SP_EL1, from EL1 on SP_EL0 */
	if ((cpsr & PSR_M_MASK) == PSR_M_EL0t)
		off = 0x400;
	else if ((cpsr & PSR_M_MASK) == PSR_M_EL1h)
		off = 0x200;
	else
		off = 0x000;
	HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, vbar + off));
	HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_CPSR, PSR_D | PSR_A | PSR_I | PSR_F | PSR_M_EL1h));
}

/* An undefined-instruction exception at the current PC. */
static void
vcpu_take_undef(struct vcpu *v)
{
	vcpu_take_exception(v, ((uint64_t)EXCP_UNKNOWN << ESR_ELx_EC_SHIFT) | ESR_ELx_IL, 0);
}

/* Owner, v->mtx held: everything other threads (and earlier exits) left for the vCPU. */
static void
vcpu_enter(struct vcpu *v)
{
	uint64_t val, cpsr, mdscr;
	int reg;

	if (v->dirty != 0) {
		for (reg = 0; reg < VM_REG_LAST; reg++)
			if (v->dirty & (1ul << reg))
				hvreg_set(v, reg, v->regs[reg]);
		v->dirty = 0;
	}
	if (v->sext_pending) {
		/*
		 * The load bhyve emulated was a sign-extending one (LDRSB/LDRSH/LDRSW): bhyve's
		 * vmm_emulate_instruction() stores what the device returned as it is.
		 */
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_X0 + v->sext_reg, &val));
		val = (uint64_t)((int64_t)(val << (64 - v->sext_bits)) >> (64 - v->sext_bits));
		if (!v->sext_64)
			val &= 0xffffffff;
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_X0 + v->sext_reg, val));
		v->sext_pending = false;
	}
	if (v->caps_dirty) {
		/* the debugger's: breakpoints (BRK) and single steps come to us, not to the guest */
		HV_CHECK(hv_vcpu_set_trap_debug_exceptions(v->hv,
		    (v->caps & ((1 << VM_CAP_BRK_EXIT) | (1 << VM_CAP_SS_EXIT))) != 0));
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_CPSR, &cpsr));
		HV_CHECK(hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_MDSCR_EL1, &mdscr));
		if (v->caps & (1 << VM_CAP_SS_EXIT)) {
			cpsr |= PSR_SS;
			mdscr |= MDSCR_SS;
		} else {
			cpsr &= ~PSR_SS;
			mdscr &= ~MDSCR_SS;
		}
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_CPSR, cpsr));
		HV_CHECK(hv_vcpu_set_sys_reg(v->hv, HV_SYS_REG_MDSCR_EL1, mdscr));
		v->caps_dirty = false;
	}
	if (v->exc_pending) {
		vcpu_take_exception(v, v->exc_esr, v->exc_far);
		v->exc_pending = false;
	}
	v->synced = false;
}

static uint64_t
vcpu_pc(struct vcpu *v)
{
	uint64_t pc;

	HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_PC, &pc));
	return (pc);
}

/*
 * A system register access Hypervisor.framework did not take care of. FreeBSD's vmm emulates a few
 * debug registers and hands the rest to bhyve, which stops the VM. Here: the same few read as zero
 * and ignore writes; anything else is an undefined instruction, which is what the access is on a
 * CPU without that register, and is logged the first times so that we learn what guests touch.
 */
static void
handle_sysreg(struct vcpu *v, uint64_t syndrome, uint64_t pc)
{
	static _Atomic int logged;
	uint32_t iss = syndrome & ESR_ELx_ISS_MASK;
	u_int op0 = ISS_MSR_OP0(iss), op1 = ISS_MSR_OP1(iss), crn = ISS_MSR_CRn(iss);
	u_int crm = ISS_MSR_CRm(iss), op2 = ISS_MSR_OP2(iss), rt = ISS_MSR_Rt(iss);
	bool read = (iss & ISS_MSR_DIR) != 0;
	bool known;

	/* OSLAR_EL1, OSLSR_EL1, OSDLR_EL1, DBGPRCR_EL1 */
	known = op0 == 2 && op1 == 0 && crn == 1 && op2 == 4 &&
	    (crm == 0 || crm == 1 || crm == 3 || crm == 4);
	if (known) {
		if (read && rt != 31)
			HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_X0 + rt, 0));
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, pc + 4));
		return;
	}
	if (atomic_fetch_add(&logged, 1) < 32)
		warnx("hvf: vcpu %d: %s of system register op0 %u op1 %u CRn %u CRm %u op2 %u at pc %#llx: undefined",
		    v->vcpuid, read ? "read" : "write", op0, op1, crn, crm, op2, (unsigned long long)pc);
	vcpu_take_undef(v);
}

/*
 * Instructions on device memory that the CPU does not describe in the syndrome (ISV = 0): a
 * load or store with writeback, and load/store pairs. Linux and FreeBSD keep to plain accesses
 * on I/O memory; Windows' drivers do not (tpm.sys reads the TPM's registers with pairs), so
 * these two families are decoded from the instruction itself.
 *
 * guest_va2gpa(): the guest's stage-1 translation by its own tables (TTBRn_EL1, TCR_EL1), any
 * granule, up to 48 bits. No permission checks: the CPU has just executed from there.
 */
static bool
guest_va2gpa(struct vcpu *v, uint64_t va, uint64_t *gpa)
{
	uint64_t sctlr, tcr, ttbr, desc, *p, base;
	u_int tsz, tg, g, stride, va_bits, shift, idx;
	int level;
	bool high = (va >> 55) & 1;

	HV_CHECK(hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_SCTLR_EL1, &sctlr));
	if ((sctlr & 1) == 0) {
		*gpa = va;
		return (true);
	}
	HV_CHECK(hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_TCR_EL1, &tcr));
	HV_CHECK(hv_vcpu_get_sys_reg(v->hv, high ? HV_SYS_REG_TTBR1_EL1 : HV_SYS_REG_TTBR0_EL1, &ttbr));
	if (high) {
		tsz = (tcr >> 16) & 0x3f;
		tg = (tcr >> 30) & 3;			/* TG1: 1 = 16K, 2 = 4K, 3 = 64K */
		g = tg == 1 ? 14 : tg == 3 ? 16 : 12;
	} else {
		tsz = tcr & 0x3f;
		tg = (tcr >> 14) & 3;			/* TG0: 0 = 4K, 1 = 64K, 2 = 16K */
		g = tg == 2 ? 14 : tg == 1 ? 16 : 12;
	}
	va_bits = 64 - tsz;
	if (va_bits > 48 || va_bits <= g)
		return (false);
	stride = g - 3;
	level = 4 - (int)((va_bits - g + stride - 1) / stride);
	/*
	 * Not rounded to the granule: a first-level table smaller than a page (Windows: 47 bits,
	 * 256 entries) may sit anywhere on its own size. Bit 0 is CnP, the top 16 the ASID.
	 */
	base = ttbr & 0x0000fffffffffffeul;
	va &= (1ul << va_bits) - 1;
	for (; level <= 3; level++) {
		shift = g + (3 - level) * stride;
		idx = (va >> shift) & ((1u << stride) - 1);
		p = vm_map_gpa(v->ctx, base + idx * 8, 8);
		if (p == NULL)
			return (false);
		desc = *p;
		if ((desc & 1) == 0)
			return (false);
		if (level == 3 || (desc & 2) == 0) {
			if (level == 3 && (desc & 2) == 0)
				return (false);
			*gpa = (desc & 0x0000fffffffff000ul & ~((1ul << shift) - 1)) | (va & ((1ul << shift) - 1));
			return (true);
		}
		base = desc & 0x0000fffffffff000ul & ~((1ul << g) - 1);
	}
	return (false);
}

/* Xn or the stack pointer in use, for a base register's writeback. */
static void
base_reg_add(struct vcpu *v, u_int rn, int64_t add)
{
	uint64_t val, cpsr;
	hv_sys_reg_t sp;

	if (rn != 31) {
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_X0 + rn, &val));
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_X0 + rn, val + add));
		return;
	}
	HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_CPSR, &cpsr));
	sp = (cpsr & 0xf) == 5 ? HV_SYS_REG_SP_EL1 : HV_SYS_REG_SP_EL0;	/* EL1h : EL1t, EL0t */
	HV_CHECK(hv_vcpu_get_sys_reg(v->hv, sp, &val));
	HV_CHECK(hv_vcpu_set_sys_reg(v->hv, sp, val + add));
}

/*
 * Decode the instruction at pc for an access to gpa. True: *vme is the access (the first of
 * two for a pair: the second waits in v->pair_exit), the base register is written back.
 */
static bool
decode_unsyndromed(struct vcpu *v, uint64_t pc, uint64_t gpa, struct vm_exit *vme, uint32_t *insnp)
{
	uint64_t pc_gpa;
	uint32_t insn, *ip;
	struct vie *vie = &vme->u.inst_emul.vie;
	u_int size, opc, rn, rt, rt2;
	int64_t imm;

	*insnp = 0;
	if (!guest_va2gpa(v, pc, &pc_gpa) || (ip = vm_map_gpa(v->ctx, pc_gpa, 4)) == NULL)
		return (false);
	insn = *insnp = *ip;
	rn = (insn >> 5) & 31;
	rt = insn & 31;
	vme->exitcode = VM_EXITCODE_INST_EMUL;
	vme->inst_length = 4;
	vme->u.inst_emul.gpa = gpa;
	memset(&vme->u.inst_emul.paging, 0, sizeof(vme->u.inst_emul.paging));
	memset(vie, 0, sizeof(*vie));

	if ((insn & 0x3f200400) == 0x38000400) {
		/* LDR/STR (and the B, H, SB, SH, SW forms), immediate, post- or pre-indexed */
		size = insn >> 30;
		opc = (insn >> 22) & 3;
		imm = (int64_t)((uint64_t)((insn >> 12) & 0x1ff) << 55) >> 55;
		if (opc >= 2 && size == 3)
			return (false);			/* prefetch, unallocated */
		vie->access_size = 1 << size;
		vie->dir = opc == 0 ? VM_DIR_WRITE : VM_DIR_READ;
		vie->reg = rt;
		vie->sign_extend = opc >= 2;
		if (vie->sign_extend && rt != 31) {
			v->sext_pending = true;
			v->sext_reg = rt;
			v->sext_bits = 8 << size;
			v->sext_64 = opc == 2;
		}
		base_reg_add(v, rn, imm);
		return (true);
	}
	if ((insn & 0x3e000000) == 0x28000000) {
		/* LDP/STP (and the no-allocate forms), 32 or 64 bits: offset, post- or pre-indexed */
		opc = insn >> 30;
		if (opc != 0 && opc != 2)
			return (false);			/* LDPSW, STGP */
		size = opc == 2 ? 3 : 2;
		rt2 = (insn >> 10) & 31;
		imm = ((int64_t)((uint64_t)((insn >> 15) & 0x7f) << 57) >> 57) * (1 << size);
		vie->access_size = 1 << size;
		vie->dir = (insn & (1u << 22)) != 0 ? VM_DIR_READ : VM_DIR_WRITE;
		vie->reg = rt;
		v->pair_exit = *vme;
		v->pair_exit.u.inst_emul.gpa = gpa + (1 << size);
		v->pair_exit.u.inst_emul.vie.reg = rt2;
		v->pair_pending = true;
		if ((insn & (1u << 23)) != 0)		/* post- (01) or pre-indexed (11): writeback */
			base_reg_add(v, rn, imm);
		return (true);
	}
	return (false);
}

/*
 * A stage-2 fault. Returns true with *vme filled if bhyve has to emulate the access (an address
 * with no memory behind it: a device).
 */
static bool
handle_abort(struct vcpu *v, bool data, struct vm_exit *vme, uint64_t pc)
{
	static _Atomic int logged;
	uint64_t syndrome = v->exit->exception.syndrome, cpsr;
	uint32_t iss = syndrome & ESR_ELx_ISS_MASK;
	struct vie *vie;
	u_int sas;

	if (!data) {
		if (v->ctx->watch && atomic_fetch_add(&logged, 1) < 8)
			warnx("hvf: watch: vcpu %d executes at %#llx, where there is no memory (ESR %#llx)",
			    v->vcpuid, (unsigned long long)pc, (unsigned long long)syndrome);
		/* Executing from an address without memory: an external abort, as FreeBSD */
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_CPSR, &cpsr));
		vcpu_take_exception(v,
		    ((uint64_t)((cpsr & PSR_M_MASK) == PSR_M_EL0t ? EXCP_INSN_ABORT_L : EXCP_INSN_ABORT) <<
		    ESR_ELx_EC_SHIFT) | (syndrome & ESR_ELx_IL) | ISS_DATA_DFSC_EXT,
		    v->exit->exception.virtual_address);
		return (false);
	}
	if ((iss & ISS_DATA_ISV) == 0) {
		/*
		 * The CPU did not decode the access (a load/store pair, one with writeback, ...):
		 * decode_unsyndromed() knows those two. Anything else (SIMD, exclusives): tell loudly
		 * and let bhyve give the guest an exception.
		 */
		uint32_t insn;

		if (decode_unsyndromed(v, pc, v->exit->exception.physical_address, vme, &insn)) {
			vme->pc = pc;
			HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, pc + 4));
			return (true);
		}
		v->pair_pending = v->sext_pending = false;
		if (atomic_fetch_add(&logged, 1) < 32)
			warnx("hvf: vcpu %d: access to %#llx at pc %#llx cannot be emulated (ESR %#llx has no ISV, instruction %#x)",
			    v->vcpuid, (unsigned long long)v->exit->exception.physical_address,
			    (unsigned long long)pc, (unsigned long long)syndrome, insn);
		vme->exitcode = VM_EXITCODE_HYP;
		vme->u.hyp.esr_el2 = syndrome;
		vme->u.hyp.far_el2 = v->exit->exception.virtual_address;
		return (true);
	}

	if (v->ctx->watch && v->exit->exception.physical_address < 0x08000000 &&
	    atomic_fetch_add(&logged, 1) < 40)
		warnx("hvf: watch: vcpu %d %s of %#llx at pc %#llx",
		    v->vcpuid, (iss & ISS_DATA_WnR) != 0 ? "write" : "read",
		    (unsigned long long)v->exit->exception.physical_address, (unsigned long long)pc);
	sas = (iss & ISS_DATA_SAS_MASK) >> ISS_DATA_SAS_SHIFT;
	vme->exitcode = VM_EXITCODE_INST_EMUL;
	vme->inst_length = 4;
	vme->u.inst_emul.gpa = v->exit->exception.physical_address;
	/* bhyve's emulation does not look at the guest's paging state: left zeroed */
	memset(&vme->u.inst_emul.paging, 0, sizeof(vme->u.inst_emul.paging));
	vie = &vme->u.inst_emul.vie;
	vie->access_size = 1 << sas;
	vie->sign_extend = (iss & ISS_DATA_SSE) != 0;
	vie->dir = (iss & ISS_DATA_WnR) != 0 ? VM_DIR_WRITE : VM_DIR_READ;
	vie->reg = (iss & ISS_DATA_SRT_MASK) >> ISS_DATA_SRT_SHIFT;
	if (vie->dir == VM_DIR_READ && vie->sign_extend && vie->reg != 31) {
		v->sext_pending = true;
		v->sext_reg = vie->reg;
		v->sext_bits = 8 << sas;
		v->sext_64 = (iss & ISS_DATA_SF) != 0;
	}
	/* FreeBSD's vm_run(): the guest continues after the instruction */
	HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, pc + 4));
	return (true);
}

/* Returns true if the exit goes to bhyve (*vme filled). */
static bool
handle_exception(struct vcpu *v, struct vm_exit *vme)
{
	uint64_t syndrome = v->exit->exception.syndrome;
	u_int ec = (syndrome >> ESR_ELx_EC_SHIFT) & 0x3f;
	uint64_t pc = vcpu_pc(v);
	int i;

	vme->pc = pc;
	vme->inst_length = 4;
	switch (ec) {
	case EXCP_DATA_ABORT_L:
		return (handle_abort(v, true, vme, pc));
	case EXCP_INSN_ABORT_L:
		return (handle_abort(v, false, vme, pc));
	case EXCP_HVC:
	case 0x17:	/* SMC: taken as the same call; the PC is still at the instruction */
		if (v->ctx->watch && (syndrome & 0xffff) == 0x7777) {
			uint64_t esr, elr, far, sctlr, vbar, x[4];

			(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_ESR_EL1, &esr);
			(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_ELR_EL1, &elr);
			(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_FAR_EL1, &far);
			(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_SCTLR_EL1, &sctlr);
			(void)hv_vcpu_get_sys_reg(v->hv, HV_SYS_REG_VBAR_EL1, &vbar);
			for (i = 0; i < 4; i++)
				(void)hv_vcpu_get_reg(v->hv, HV_REG_X0 + i, &x[i]);
			errx(4, "hvf: watch: vcpu %d took an exception with no vectors (VBAR_EL1 %#llx): "
			    "vector %#llx, ESR_EL1 %#llx (class %#llx), ELR_EL1 %#llx, FAR_EL1 %#llx, "
			    "SCTLR_EL1 %#llx, x0-x3 %#llx %#llx %#llx %#llx",
			    v->vcpuid, (unsigned long long)vbar, (unsigned long long)pc - 4,
			    (unsigned long long)esr, (unsigned long long)(esr >> 26) & 0x3f,
			    (unsigned long long)elr, (unsigned long long)far, (unsigned long long)sctlr,
			    (unsigned long long)x[0], (unsigned long long)x[1], (unsigned long long)x[2],
			    (unsigned long long)x[3]);
		}
		if ((syndrome & 0xffff) != 0) {
			if (ec == EXCP_HVC)	/* an HVC's PC is already past it */
				HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, pc - 4));
			vcpu_take_undef(v);
			return (false);
		}
		if (ec != EXCP_HVC)
			HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, pc + 4));
		vme->exitcode = VM_EXITCODE_SMCCC;
		HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_X0, &vme->u.smccc_call.func_id));
		for (i = 0; i < (int)nitems(vme->u.smccc_call.args); i++)
			HV_CHECK(hv_vcpu_get_reg(v->hv, HV_REG_X1 + i, &vme->u.smccc_call.args[i]));
		return (true);
	case EXCP_MSR:
		handle_sysreg(v, syndrome, pc);
		return (false);
	case EXCP_TRAP_WFI_WFE:
		/* Hypervisor.framework waits in WFI itself; should one come out, it is a no-op */
		HV_CHECK(hv_vcpu_set_reg(v->hv, HV_REG_PC, pc + 4));
		sched_yield();
		return (false);
	case EXCP_BRK:
		vme->exitcode = VM_EXITCODE_BRK;
		return (true);
	case EXCP_SOFTSTP_EL0:
	case EXCP_SOFTSTP_EL1:
		vme->exitcode = VM_EXITCODE_SS;
		return (true);
	default:
		if (v->ctx->watch)
			warnx("hvf: watch: vcpu %d exception class %#x at pc %#llx (ESR %#llx, address %#llx)",
			    v->vcpuid, ec, (unsigned long long)pc, (unsigned long long)syndrome,
			    (unsigned long long)v->exit->exception.virtual_address);
		/* bhyve's vmexit_hyp() gives the guest an "unknown reason" exception */
		vme->exitcode = VM_EXITCODE_HYP;
		vme->u.hyp.exception_nr = ec;
		vme->u.hyp.esr_el2 = syndrome;
		vme->u.hyp.far_el2 = v->exit->exception.virtual_address;
		return (true);
	}
}

/*
 * The VM is suspended: as FreeBSD's vm_handle_suspend(), wait until every active vCPU has seen it,
 * so that bhyve's exit paths do not race with a vCPU still in the guest.
 */
static void
vcpu_suspended(struct vcpu *v)
{
	struct vmctx *ctx = v->ctx;
	struct timespec ts;

	pthread_mutex_lock(&ctx->mtx);
	CPU_SET(v->vcpuid, &ctx->suspended_cpus);
	pthread_cond_broadcast(&ctx->cv);
	while (CPU_CMP(&ctx->suspended_cpus, &ctx->active_cpus) != 0) {
		kick_active(ctx);
		clock_gettime(CLOCK_REALTIME, &ts);
		ts.tv_sec++;
		pthread_cond_timedwait(&ctx->cv, &ctx->mtx, &ts);
	}
	pthread_mutex_unlock(&ctx->mtx);
}

int
vm_run(struct vcpu *v, struct vm_run *vmrun)
{
	struct vmctx *ctx = v->ctx;
	struct vm_exit *vme = vmrun->vm_exit;
	hv_return_t r;
	bool active, debug;
	int how;

	pthread_mutex_lock(&ctx->mtx);
	active = CPU_ISSET(v->vcpuid, &ctx->active_cpus);
	pthread_mutex_unlock(&ctx->mtx);
	if (!active) {
		errno = EINVAL;
		return (-1);
	}
	if (!v->created)
		vcpu_create(v);
	assert(pthread_equal(v->owner, pthread_self()));

	memset(vme, 0, sizeof(*vme));
	if (v->pair_pending) {
		/* the second half of a load/store pair on MMIO (decode_unsyndromed()) */
		v->pair_pending = false;
		*vme = v->pair_exit;
		return (0);
	}
	for (;;) {
		pthread_mutex_lock(&ctx->mtx);
		debug = CPU_ISSET(v->vcpuid, &ctx->debug_cpus);
		pthread_mutex_unlock(&ctx->mtx);
		how = atomic_load(&ctx->suspend);

		pthread_mutex_lock(&v->mtx);
		if (v->pausers > 0) {
			/* another thread wants the registers: hand them over and wait until it is done */
			vcpu_sync(v);
			while (v->pausers > 0)
				pthread_cond_wait(&v->cv, &v->mtx);
		}
		if (how != VM_SUSPEND_NONE || debug) {
			/* stopped: the cache is what the debugger and PSCI CPU_ON read and write */
			vcpu_sync(v);
			vme->pc = v->regs[VM_REG_GUEST_PC];
			pthread_mutex_unlock(&v->mtx);
			if (how != VM_SUSPEND_NONE) {
				vcpu_suspended(v);
				vme->exitcode = VM_EXITCODE_SUSPENDED;
				vme->u.suspended.how = how;
			} else
				vme->exitcode = VM_EXITCODE_DEBUG;
			return (0);
		}
		vcpu_enter(v);
		pthread_mutex_unlock(&v->mtx);

		vcpus_wait_created(ctx);
		r = hv_vcpu_run(v->hv);
		if (r != HV_SUCCESS) {
			warnx("hvf: vcpu %d: hv_vcpu_run: error %#x", v->vcpuid, (unsigned)r);
			errno = hv_errno(r);
			return (-1);
		}

		switch (v->exit->reason) {
		case HV_EXIT_REASON_CANCELED:
			/* kicked: the top of the loop finds out why */
			break;
		case HV_EXIT_REASON_EXCEPTION:
			if (handle_exception(v, vme)) {
				if (vme->exitcode == VM_EXITCODE_BRK || vme->exitcode == VM_EXITCODE_SS) {
					pthread_mutex_lock(&v->mtx);
					vcpu_sync(v);
					pthread_mutex_unlock(&v->mtx);
				}
				return (0);
			}
			break;
		case HV_EXIT_REASON_VTIMER_ACTIVATED:
			/*
			 * Without a GIC the timer interrupt would be ours to deliver. With hv_gic it
			 * is not expected at all; if it comes, unmask so the timer keeps working.
			 */
			HV_CHECK(hv_vcpu_set_vtimer_mask(v->hv, false));
			break;
		default:
			warnx("hvf: vcpu %d: unknown exit reason %u", v->vcpuid, (unsigned)v->exit->reason);
			errno = EIO;
			return (-1);
		}
	}
}

/*
 * Interrupts
 */

int
vm_attach_vgic(struct vmctx *ctx, uint64_t dist_start, size_t dist_size, uint64_t redist_start,
    size_t redist_size)
{
	hv_gic_config_t config;
	size_t hv_dist_size, hv_redist_size;
	hv_return_t r;

	/*
	 * The sizes are Hypervisor.framework's to decide; bhyve's layout (64K, and 2 x 64K per vCPU)
	 * is the same. A difference would mean the device tree describes a GIC that is not there.
	 */
	HV_CHECK(hv_gic_get_distributor_size(&hv_dist_size));
	HV_CHECK(hv_gic_get_redistributor_size(&hv_redist_size));
	if (dist_size != hv_dist_size || redist_size % hv_redist_size != 0) {
		warnx("hvf: GIC sizes: bhyve %#zx/%#zx, Hypervisor.framework %#zx and %#zx per CPU",
		    dist_size, redist_size, hv_dist_size, hv_redist_size);
		errno = EINVAL;
		return (-1);
	}
	config = hv_gic_config_create();
	HV_CHECK(hv_gic_config_set_distributor_base(config, dist_start));
	HV_CHECK(hv_gic_config_set_redistributor_base(config, redist_start));
	if (ctx->gic_msi_base != 0) {
		HV_CHECK(hv_gic_config_set_msi_region_base(config, ctx->gic_msi_base));
		HV_CHECK(hv_gic_config_set_msi_interrupt_range(config, ctx->gic_msi_spi_base,
		    ctx->gic_msi_spi_count));
	}
	r = hv_gic_create(config);
	os_release(config);
	if (r != HV_SUCCESS)
		return (hv_ret(r));
	ctx->gic_dist_start = dist_start;
	ctx->gic_attached = true;
	return (0);
}

/*
 * `irq` is the interrupt's INTID (an SPI: 32 and up); the line stays up until deasserted.
 *
 * Device models set the level far more often than it changes (the PL011 lowers its line on every
 * register access), and hv_gic_set_spi() is a system call: only changes go to the GIC.
 */
static int
spi_set_level(struct vmctx *ctx, uint32_t irq, bool level)
{
	hv_return_t r = HV_SUCCESS;
	uint64_t *word, bit;

	if (irq < GIC_FIRST_SPI || irq >= GIC_FIRST_SPI + HVF_NSPI) {
		errno = EINVAL;
		return (-1);
	}
	word = &ctx->spi_level[(irq - GIC_FIRST_SPI) / 64];
	bit = 1ul << ((irq - GIC_FIRST_SPI) % 64);
	pthread_mutex_lock(&ctx->spi_mtx);
	if (((*word & bit) != 0) != level) {
		r = hv_gic_set_spi(irq, level);
		if (r == HV_SUCCESS)
			*word ^= bit;
	}
	pthread_mutex_unlock(&ctx->spi_mtx);
	return (hv_ret(r));
}

int
vm_assert_irq(struct vmctx *ctx, uint32_t irq)
{
	return (spi_set_level(ctx, irq, true));
}

int
vm_deassert_irq(struct vmctx *ctx, uint32_t irq)
{
	return (spi_set_level(ctx, irq, false));
}

int
vm_hvf_set_msi_frame(struct vmctx *ctx, uint64_t base, size_t size, uint32_t spi_base,
    uint32_t spi_count)
{
	size_t hv_size, hv_align;
	uint32_t hv_spi_base, hv_spi_count;

	HV_CHECK(hv_gic_get_msi_region_size(&hv_size));
	HV_CHECK(hv_gic_get_msi_region_base_alignment(&hv_align));
	HV_CHECK(hv_gic_get_spi_interrupt_range(&hv_spi_base, &hv_spi_count));
	if (ctx->gic_attached || size != hv_size || base % hv_align != 0 || spi_count == 0 ||
	    spi_base < hv_spi_base || spi_base + spi_count > hv_spi_base + hv_spi_count) {
		errno = EINVAL;
		return (-1);
	}
	ctx->gic_msi_base = base;
	ctx->gic_msi_size = size;
	ctx->gic_msi_spi_base = spi_base;
	ctx->gic_msi_spi_count = spi_count;
	return (0);
}

/*
 * An MSI: the device "writes" `msg`, an SPI's number, to a register of the interrupt controller.
 *
 * With a GICv2m frame (vm_hvf_set_msi_frame()) that register is the frame's MSI_SETSPI_NS and the
 * SPI one of the frame's. Without one it is the distributor's GICD_SETSPI_NSR, as with FreeBSD's
 * vgic (the device tree's "mbi-ranges"); the guest makes those SPIs edge-triggered, and a rising
 * edge on the SPI's line is the same thing.
 */
int
vm_raise_msi(struct vmctx *ctx, uint64_t addr, uint64_t msg, int bus __unused, int slot __unused,
    int func __unused)
{
	if (!ctx->gic_attached) {
		errno = EINVAL;
		return (-1);
	}
	if (ctx->gic_msi_base != 0) {
		if (addr != ctx->gic_msi_base + GICV2M_MSI_SETSPI_NS || msg < ctx->gic_msi_spi_base ||
		    msg >= ctx->gic_msi_spi_base + ctx->gic_msi_spi_count) {
			errno = EINVAL;
			return (-1);
		}
		return (hv_ret(hv_gic_send_msi(addr, (uint32_t)msg)));
	}
	if (addr != ctx->gic_dist_start + GICD_SETSPI_NSR || msg < GIC_FIRST_SPI) {
		errno = EINVAL;
		return (-1);
	}
	/* not through spi_set_level(): these SPIs are no one's level-triggered line */
	if (hv_ret(hv_gic_set_spi((uint32_t)msg, true)) != 0)
		return (-1);
	return (hv_ret(hv_gic_set_spi((uint32_t)msg, false)));
}
