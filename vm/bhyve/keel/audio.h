/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * Copyright (c) 2016 Alex Teaca <iateaca@FreeBSD.org>
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
 * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND
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

#ifndef _AUDIO_EMUL_H_
#define _AUDIO_EMUL_H_

#include <sys/types.h>
#include <sys/soundcard.h>

/*
 * Audio Player data structures
 */

struct audio;

struct audio_params {
	int channels;
	int format;
	int rate;
};

/*
 * Audio Player API
 */

/*
 * audio_init - initialize an instance of audio player
 * @dev_name - the backend sound device used to play / capture
 * @dir - dir = 1 for write mode, dir = 0 for read mode
 * Returns NULL on error and the address of the audio player instance
 */
struct audio *audio_init(const char *dev_name, uint8_t dir);

/*
 * audio_set_params - reset the sound device and set the audio params
 * @aud - the audio player to be configured
 * @params - the audio parameters to be set
 * Returns -1 on error and 0 on success
 */
int audio_set_params(struct audio *aud, struct audio_params *params);

/*
 * audio_playback - plays samples to the sound device using blocking operations
 * @aud - the audio player used to play the samples
 * @buf - the buffer containing the samples
 * @count - the number of bytes in buffer
 * Returns -1 on error and 0 on success
 */
int audio_playback(struct audio *aud, const uint8_t *buf, size_t count);

/*
 * audio_record - records samples from the sound device using blocking
 * operations.
 * @aud - the audio player used to capture the samples
 * @buf - the buffer to receive the samples
 * @count - the number of bytes to capture in buffer
 * Returns -1 on error and 0 on success
 */
int audio_record(struct audio *aud, uint8_t *buf, size_t count);

/*
 * keel (docs/71 §11): play=keel:/keel-snd-NAME writes the output into a POSIX shared memory
 * ring for keel-rdp instead of an OSS device. The ring (struct keel_snd below, then the data)
 * always holds 16-bit little-endian stereo at the guest's rate; the reader follows wpos.
 */
#define	KEEL_SND_MAGIC		0x314e534bu	/* "KSN1" */
#define	KEEL_SND_RING		(256 * 1024)	/* bytes of samples, a power of 2 (~1.4 s at 48 kHz) */
#define	KEEL_SND_HDR		64		/* the data starts here */

struct keel_snd {
	uint32_t magic;		/* KEEL_SND_MAGIC, written last when the ring is made */
	uint32_t version;	/* 1 */
	uint32_t size;		/* KEEL_SND_RING */
	uint32_t rate;		/* frames a second of what is in the ring (44100, 48000) */
	uint32_t epoch;		/* changes when the stream starts or the rate changes: resync */
	uint32_t pid;		/* the bhyve that writes */
	uint64_t wpos;		/* bytes written in all; the data of byte n is at n % size */
	uint64_t wtime;		/* CLOCK_MONOTONIC ns when wpos was last moved */
	uint32_t pad[6];
};

/* 1: aud plays into a keel ring (it does not block: the codec paces itself) */
int audio_is_keel(struct audio *aud);

/* the output amplifier, Q16 per channel (65536 = 0 dB, 0 = muted); only the keel backend */
void audio_set_volume(struct audio *aud, uint32_t left, uint32_t right);

#endif  /* _AUDIO_EMUL_H_ */
