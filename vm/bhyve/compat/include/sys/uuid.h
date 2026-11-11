/* FreeBSD's struct uuid (the DCE 1.1 layout), for macOS. */
#ifndef _SYS_UUID_H_
#define	_SYS_UUID_H_

#include <sys/types.h>
#include <stdint.h>

#define	_UUID_NODE_LEN		6

struct uuid {
	uint32_t	time_low;
	uint16_t	time_mid;
	uint16_t	time_hi_and_version;
	uint8_t		clock_seq_hi_and_reserved;
	uint8_t		clock_seq_low;
	uint8_t		node[_UUID_NODE_LEN];
};

#endif /* _SYS_UUID_H_ */
