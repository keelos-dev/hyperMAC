/* uuid_from_string() and uuid_enc_le() as in FreeBSD's libc (compat/include/uuid.h). */
#include <stdio.h>
#include <string.h>

#include <uuid.h>

void
bhyve_uuid_from_string(const char *s, bhyve_uuid_t *u, uint32_t *status)
{
	unsigned int f[11];
	int n, i;

	if (status != NULL)
		*status = uuid_s_invalid_string_uuid;
	memset(u, 0, sizeof(*u));
	/* FreeBSD: an empty or missing string is the nil UUID */
	if (s == NULL || *s == '\0') {
		if (status != NULL)
			*status = uuid_s_ok;
		return;
	}
	if (strlen(s) != 36 || s[8] != '-' || s[13] != '-' || s[18] != '-' || s[23] != '-')
		return;
	n = sscanf(s, "%8x-%4x-%4x-%2x%2x-%2x%2x%2x%2x%2x%2x", &f[0], &f[1], &f[2], &f[3], &f[4],
	    &f[5], &f[6], &f[7], &f[8], &f[9], &f[10]);
	if (n != 11)
		return;
	u->time_low = f[0];
	u->time_mid = f[1];
	u->time_hi_and_version = f[2];
	u->clock_seq_hi_and_reserved = f[3];
	u->clock_seq_low = f[4];
	for (i = 0; i < _UUID_NODE_LEN; i++)
		u->node[i] = f[5 + i];
	/* the variant, as FreeBSD checks it: 0xxx NCS, 10xx DCE, 110x Microsoft; 111x is not valid */
	if ((u->clock_seq_hi_and_reserved & 0xe0) == 0xe0) {
		if (status != NULL)
			*status = uuid_s_bad_version;
		return;
	}
	if (status != NULL)
		*status = uuid_s_ok;
}

void
bhyve_uuid_enc_le(void *buf, const bhyve_uuid_t *u)
{
	uint8_t *p = buf;
	int i;

	p[0] = u->time_low;
	p[1] = u->time_low >> 8;
	p[2] = u->time_low >> 16;
	p[3] = u->time_low >> 24;
	p[4] = u->time_mid;
	p[5] = u->time_mid >> 8;
	p[6] = u->time_hi_and_version;
	p[7] = u->time_hi_and_version >> 8;
	p[8] = u->clock_seq_hi_and_reserved;
	p[9] = u->clock_seq_low;
	for (i = 0; i < _UUID_NODE_LEN; i++)
		p[10 + i] = u->node[i];
}
