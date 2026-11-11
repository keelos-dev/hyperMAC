/*
 * FreeBSD's linker sets for Mach-O. FreeBSD collects the pointers in an ELF section named after the set
 * and walks from __start_set_NAME to __stop_set_NAME; Mach-O section names stop at 16 characters and
 * several of bhyve's sets are longer. Here each entry registers itself from a constructor
 * (compat/src/linker_set.c keeps one array per set) and SET_BEGIN/SET_LIMIT look the set up by name.
 */
#ifndef _SYS_LINKER_SET_H_
#define	_SYS_LINKER_SET_H_

#include <sys/cdefs.h>

__BEGIN_DECLS
void	  __linker_set_add(const char *set, const void *sym);
void	**__linker_set_begin(const char *set);
void	**__linker_set_limit(const char *set);
__END_DECLS

#define	__MAKE_SET(set, sym)						\
	static void __attribute__((__constructor__, __used__))		\
	__set_##set##_add_##sym(void)					\
	{								\
		__linker_set_add(#set, &(sym));				\
	}								\
	struct __hack

#define	TEXT_SET(set, sym)	__MAKE_SET(set, sym)
#define	DATA_SET(set, sym)	__MAKE_SET(set, sym)
#define	DATA_WSET(set, sym)	__MAKE_SET(set, sym)
#define	BSS_SET(set, sym)	__MAKE_SET(set, sym)

#define	SET_DECLARE(set, ptype)						\
	static inline ptype ** __attribute__((__unused__))		\
	__set_##set##_begin(void)					\
	{								\
		return ((ptype **)__linker_set_begin(#set));		\
	}								\
	static inline ptype ** __attribute__((__unused__))		\
	__set_##set##_limit(void)					\
	{								\
		return ((ptype **)__linker_set_limit(#set));		\
	}								\
	struct __hack

#define	SET_BEGIN(set)	(__set_##set##_begin())
#define	SET_LIMIT(set)	(__set_##set##_limit())
#define	SET_FOREACH(pvar, set)						\
	for (pvar = SET_BEGIN(set); pvar < SET_LIMIT(set); pvar++)
#define	SET_ITEM(set, i)	((SET_BEGIN(set))[i])
#define	SET_COUNT(set)		(SET_LIMIT(set) - SET_BEGIN(set))

#endif /* _SYS_LINKER_SET_H_ */
