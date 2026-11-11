/* The libutil functions bhyve uses; built from FreeBSD's own sources (freebsd/lib/libutil). */
#ifndef _LIBUTIL_H_
#define	_LIBUTIL_H_

#include <sys/cdefs.h>
#include <sys/types.h>
#include <sys/cpuset.h>
#include <sys/domainset.h>
#include <stdint.h>

#define	HD_COLUMN_MASK		0xff
#define	HD_DELIM_MASK		0xff00
#define	HD_OMIT_COUNT		(1 << 16)
#define	HD_OMIT_HEX		(1 << 17)
#define	HD_OMIT_CHARS		(1 << 18)

#define	CPUSET_PARSE_OK			0
#define	CPUSET_PARSE_GETAFFINITY	-1
#define	CPUSET_PARSE_ERROR		-2
#define	CPUSET_PARSE_OUT_OF_RANGE	-3
#define	CPUSET_PARSE_GETDOMAIN		-4
#define	CPUSET_PARSE_INVALID_CPU	CPUSET_PARSE_OUT_OF_RANGE

__BEGIN_DECLS
int	cpuset_parselist(const char *_list, cpuset_t *_mask);
int	domainset_parselist(const char *_list, domainset_t *_mask, int *_policyp);
int	expand_number(const char *_buf, int64_t *_num);
int	expand_unsigned(const char *_buf, uint64_t *_num);
#define	expand_number(_buf, _num)					\
	_Generic((_num),						\
		 uint64_t *: expand_unsigned,				\
		 default: expand_number)((_buf), (_num))
void	hexdump(const void *_ptr, int _length, const char *_hdr, int _flags);
__END_DECLS

#endif /* _LIBUTIL_H_ */
