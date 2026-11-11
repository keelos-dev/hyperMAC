/*
 * FreeBSD's libc uuid API (uuid(3)), the calls bhyve uses, for macOS (compat/src/uuid.c).
 * macOS's own uuid_t is a byte array (libuuid style); FreeBSD's is struct uuid. A file that includes
 * this header gets FreeBSD's meaning.
 */
#ifndef _UUID_H_
#define	_UUID_H_

#include <sys/uuid.h>
#include <unistd.h>		/* macOS's uuid_t, before it is renamed below */

typedef struct uuid	bhyve_uuid_t;
#define	uuid_t		bhyve_uuid_t

#define	uuid_s_ok			0
#define	uuid_s_bad_version		1
#define	uuid_s_invalid_string_uuid	2
#define	uuid_s_no_memory		3

__BEGIN_DECLS
void	bhyve_uuid_from_string(const char *s, bhyve_uuid_t *u, uint32_t *status);
void	bhyve_uuid_enc_le(void *buf, const bhyve_uuid_t *u);
__END_DECLS
#define	uuid_from_string	bhyve_uuid_from_string
#define	uuid_enc_le		bhyve_uuid_enc_le

#endif /* _UUID_H_ */
