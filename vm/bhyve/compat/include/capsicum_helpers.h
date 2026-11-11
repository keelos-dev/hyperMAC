/* See sys/capsicum.h: no Capsicum on macOS, every helper succeeds and does nothing. */
#ifndef _CAPSICUM_HELPERS_H_
#define	_CAPSICUM_HELPERS_H_

#include <sys/capsicum.h>

#define	caph_rights_limit(...)		(0)
#define	caph_ioctls_limit(...)		(0)
#define	caph_fcntls_limit(...)		(0)
#define	caph_limit_stdin()		(0)
#define	caph_limit_stdout()		(0)
#define	caph_limit_stderr()		(0)
#define	caph_limit_stdio()		(0)
#define	caph_limit_stream(...)		(0)
#define	caph_enter()			(0)
#define	caph_cache_catpages()		((void)0)
#define	caph_cache_tzdata()		((void)0)

#endif /* _CAPSICUM_HELPERS_H_ */
