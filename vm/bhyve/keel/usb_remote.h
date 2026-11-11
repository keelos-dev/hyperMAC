/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * Copyright (c) 2026 keelOS
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS'' AND
 * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
 * OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
 * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */

/*
 * keel: remote USB devices on the xHCI's empty ports (docs/66 §4.2, §4.3).
 * The kusb wire format (docs/66 §4.2.1): every message is a 16-byte header and
 * `length` bytes of payload, all little-endian, on a unix stream socket.
 */

#ifndef _USB_REMOTE_H_
#define	_USB_REMOTE_H_

#include <stdint.h>

#define	KUSB_VERSION		1

#define	KUSB_HELLO		0x01	/* both ways */
#define	KUSB_ATTACH		0x02	/* source -> bhyve */
#define	KUSB_DETACH		0x03	/* source -> bhyve */
#define	KUSB_COMPLETE		0x04	/* both ways */
#define	KUSB_PORT		0x10	/* bhyve -> source */
#define	KUSB_CONTROL		0x11
#define	KUSB_BULK		0x12
#define	KUSB_INTR		0x13
#define	KUSB_CANCEL		0x14
#define	KUSB_RESET		0x15
#define	KUSB_EP_HALT_CLEAR	0x16

#define	KUSB_ST_OK		0
#define	KUSB_ST_STALL		1
#define	KUSB_ST_ERROR		2
#define	KUSB_ST_CANCELLED	3
#define	KUSB_ST_NODEV		4

#define	KUSB_SPEED_LOW		1
#define	KUSB_SPEED_FULL		2
#define	KUSB_SPEED_HIGH		3
#define	KUSB_SPEED_SUPER	4

struct kusb_hdr {
	uint8_t		type;
	uint8_t		port;	/* root hub port, from 1; 0 in HELLO */
	uint8_t		ep;	/* endpoint address (bit 7 = IN): BULK, INTR, EP_HALT_CLEAR */
	uint8_t		status;	/* COMPLETE only */
	uint32_t	id;	/* request number, echoed in COMPLETE */
	uint32_t	length;	/* payload bytes */
	uint32_t	rsvd;
} __packed;

/*
 * The xHCI is set up (or restored, and the guest runs again): the socket thread starts
 * taking connections. Before that, ATTACH could reach a controller that is not there yet.
 */
void	usb_remote_ready(void);

#endif /* _USB_REMOTE_H_ */
