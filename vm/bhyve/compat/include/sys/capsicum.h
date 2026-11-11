/*
 * macOS has no Capsicum. bhyve limits its descriptors' rights and enters capability mode as hardening:
 * here every call succeeds and does nothing (the arguments are not evaluated, so the CAP_* names
 * need no definitions).
 */
#ifndef _SYS_CAPSICUM_H_
#define	_SYS_CAPSICUM_H_

typedef struct { int unused; } cap_rights_t;

#define	cap_rights_init(...)		((void)0)
#define	cap_rights_set(...)		((void)0)
#define	cap_rights_clear(...)		((void)0)
#define	cap_rights_limit(...)		(0)
#define	cap_ioctls_limit(...)		(0)
#define	cap_fcntls_limit(...)		(0)
#define	cap_enter()			(0)

#endif /* _SYS_CAPSICUM_H_ */
