/* The part of FreeBSD/arm64's <machine/cpu.h> userland uses: the affinity fields of MPIDR_EL1. */
#ifndef _MACHINE_CPU_H_
#define	_MACHINE_CPU_H_

#define	CPU_AFF0_MASK	0xffUL
#define	CPU_AFF1_MASK	0xff00UL
#define	CPU_AFF2_MASK	0xff0000UL
#define	CPU_AFF3_MASK	0xff00000000UL
#define	CPU_AFF_MASK	(CPU_AFF0_MASK | CPU_AFF1_MASK | CPU_AFF2_MASK | CPU_AFF3_MASK)

#endif /* _MACHINE_CPU_H_ */
