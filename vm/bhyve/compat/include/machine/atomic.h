/* FreeBSD's <machine/atomic.h>, the calls bhyve's userland uses, on the compiler's builtins. */
#ifndef _MACHINE_ATOMIC_H_
#define	_MACHINE_ATOMIC_H_

#include <stdint.h>

#define	atomic_load_32(p)	__atomic_load_n((uint32_t *)(p), __ATOMIC_SEQ_CST)
#define	atomic_load_64(p)	__atomic_load_n((uint64_t *)(p), __ATOMIC_SEQ_CST)
#define	atomic_store_rel_32(p, v)	__atomic_store_n((uint32_t *)(p), (v), __ATOMIC_SEQ_CST)
#define	atomic_store_rel_64(p, v)	__atomic_store_n((uint64_t *)(p), (v), __ATOMIC_SEQ_CST)
#define	atomic_load_acq_short(p) __atomic_load_n((unsigned short *)(p), __ATOMIC_SEQ_CST)
#define	atomic_store_short(p, v) __atomic_store_n((unsigned short *)(p), (v), __ATOMIC_SEQ_CST)
#define	atomic_store_rel_short(p, v) __atomic_store_n((unsigned short *)(p), (v), __ATOMIC_SEQ_CST)
#define	atomic_thread_fence_rel()	__atomic_thread_fence(__ATOMIC_RELEASE)
#define	atomic_thread_fence_seq_cst()	__atomic_thread_fence(__ATOMIC_SEQ_CST)
#define	mb()				__atomic_thread_fence(__ATOMIC_SEQ_CST)

#define	atomic_set_long(p, v)	(void)__atomic_fetch_or((unsigned long *)(p), (v), __ATOMIC_SEQ_CST)
#define	atomic_clear_long(p, v)	(void)__atomic_fetch_and((unsigned long *)(p), ~(unsigned long)(v), __ATOMIC_SEQ_CST)

/* Set bit v of *p; nonzero if it was already set. */
static inline int
atomic_testandset_long(volatile unsigned long *p, unsigned int v)
{
	unsigned long bit = 1UL << (v & 63);

	return ((__atomic_fetch_or(p, bit, __ATOMIC_SEQ_CST) & bit) != 0);
}

/* FreeBSD's cmpset: nonzero if *p was old and is now new. */
static inline int
atomic_cmpset_ptr(volatile uintptr_t *p, uintptr_t old, uintptr_t new)
{
	return (__atomic_compare_exchange_n(p, &old, new, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST));
}

static inline int
atomic_cmpset_long(volatile unsigned long *p, unsigned long old, unsigned long new)
{
	return (__atomic_compare_exchange_n(p, &old, new, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST));
}

#endif /* _MACHINE_ATOMIC_H_ */
