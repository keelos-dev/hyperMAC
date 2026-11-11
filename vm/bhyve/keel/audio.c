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
 *
 */

#include <sys/cdefs.h>
#include <sys/param.h>
#ifndef WITHOUT_CAPSICUM
#include <sys/capsicum.h>
#include <capsicum_helpers.h>
#endif

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <time.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <machine/atomic.h>
#include <unistd.h>
#include <assert.h>
#include <errno.h>
#include <err.h>
#include <sysexits.h>

#include "audio.h"
#include "pci_hda.h"

/*
 * Audio Player internal data structures
 */

struct audio {
	int fd;
	uint8_t dir;
	uint8_t inited;
	char dev_name[64];

	/* keel: play=keel:/keel-snd-NAME */
	struct keel_snd *ring;	/* NULL: OSS */
	uint8_t *data;
	int channels;		/* in the guest's stream */
	uint32_t vol[2];	/* Q16 */
};

/*
 * keel (docs/71 §11): the shared memory ring. An existing object of that name (the guest's
 * bhyve before, keel-rdp may still have it mapped) is reused: keel-rdp's mapping stays good
 * across a restart of the guest, a new epoch makes it resync.
 */
static int
keel_snd_open(struct audio *aud, const char *path)
{
	struct keel_snd *r;
	uint32_t epoch = 0;
	void *p;
	int fd;

	fd = shm_open(path, O_RDWR | O_CREAT, 0600);
	if (fd == -1) {
		warn("hda: shm_open %s", path);
		return (-1);
	}
	if (ftruncate(fd, KEEL_SND_HDR + KEEL_SND_RING) == -1) {
		warn("hda: ftruncate %s", path);
		close(fd);
		return (-1);
	}
	p = mmap(NULL, KEEL_SND_HDR + KEEL_SND_RING, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	close(fd);
	if (p == MAP_FAILED) {
		warn("hda: mmap %s", path);
		return (-1);
	}
	r = p;
	if (r->magic == KEEL_SND_MAGIC)
		epoch = r->epoch;	/* and the rate the guest played last */
	else
		r->rate = 48000;
	r->version = 1;
	r->size = KEEL_SND_RING;
	r->pid = getpid();
	atomic_store_rel_32(&r->epoch, epoch + 1);
	atomic_store_rel_32(&r->magic, KEEL_SND_MAGIC);
	aud->ring = r;
	aud->data = (uint8_t *)p + KEEL_SND_HDR;
	aud->vol[0] = aud->vol[1] = 65536;
	return (0);
}

int
audio_is_keel(struct audio *aud)
{
	return (aud != NULL && aud->ring != NULL);
}

void
audio_set_volume(struct audio *aud, uint32_t left, uint32_t right)
{
	aud->vol[0] = left;
	aud->vol[1] = right;
}

static inline int16_t
keel_snd_scale(int16_t s, uint32_t q16)
{
	return ((int16_t)(((int64_t)s * q16) >> 16));
}

/* 16-bit samples of aud->channels channels -> 16-bit stereo in the ring, with the volume */
static void
keel_snd_play(struct audio *aud, const uint8_t *buf, size_t count)
{
	struct keel_snd *r = aud->ring;
	int16_t out[2 * 256];
	const int16_t *in = (const int16_t *)buf;
	size_t frames = count / 2 / aud->channels, n, i, j, off, len;
	uint64_t w = r->wpos;
	struct timespec ts;

	for (i = 0; i < frames; i += n) {
		n = MIN(frames - i, nitems(out) / 2);
		for (j = 0; j < n; j++) {
			const int16_t *f = in + (i + j) * aud->channels;

			out[2 * j] = keel_snd_scale(f[0], aud->vol[0]);
			out[2 * j + 1] = keel_snd_scale(aud->channels > 1 ? f[1] : f[0], aud->vol[1]);
		}
		len = n * 4;
		off = w % KEEL_SND_RING;
		if (off + len <= KEEL_SND_RING)
			memcpy(aud->data + off, out, len);
		else {
			memcpy(aud->data + off, out, KEEL_SND_RING - off);
			memcpy(aud->data, (uint8_t *)out + (KEEL_SND_RING - off), len - (KEEL_SND_RING - off));
		}
		w += len;
	}
	clock_gettime(CLOCK_MONOTONIC, &ts);
	r->wtime = (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
	atomic_store_rel_64(&r->wpos, w);
}

/*
 * Audio Player module function definitions
 */

/*
 * audio_init - initialize an instance of audio player
 * @dev_name - the backend sound device used to play / capture
 * @dir - dir = 1 for write mode, dir = 0 for read mode
 */
struct audio *
audio_init(const char *dev_name, uint8_t dir)
{
	struct audio *aud = NULL;
#ifndef WITHOUT_CAPSICUM
	cap_rights_t rights;
	cap_ioctl_t cmds[] = {
	    SNDCTL_DSP_RESET, SNDCTL_DSP_SETFMT, SNDCTL_DSP_CHANNELS,
	    SNDCTL_DSP_SPEED,
#ifdef DEBUG_HDA
	    SNDCTL_DSP_GETOSPACE, SNDCTL_DSP_GETISPACE,
#endif
	};
#endif
	size_t nlen;

	assert(dev_name);

	aud = calloc(1, sizeof(*aud));
	if (!aud)
		return NULL;

	nlen = strlen(dev_name);
	if (nlen < sizeof(aud->dev_name))
		memcpy(aud->dev_name, dev_name, nlen + 1);
	else {
		DPRINTF("dev_name too big");
		free(aud);
		return NULL;
	}

	aud->dir = dir;

	if (strncmp(aud->dev_name, "keel:", 5) == 0) {	/* keel: the ring for keel-rdp */
		aud->fd = -1;
		if (!dir || keel_snd_open(aud, aud->dev_name + 5) != 0) {
			free(aud);
			return (NULL);
		}
		return (aud);
	}

	aud->fd = open(aud->dev_name, aud->dir ? O_WRONLY : O_RDONLY, 0);
	if (aud->fd == -1) {
		DPRINTF("Failed to open dev: %s, errno: %d",
		    aud->dev_name, errno);
		free(aud);
		return (NULL);
	}

#ifndef WITHOUT_CAPSICUM
	cap_rights_init(&rights, CAP_IOCTL, CAP_READ, CAP_WRITE);
	if (caph_rights_limit(aud->fd, &rights) == -1)
		errx(EX_OSERR, "Unable to apply rights for sandbox");
	if (caph_ioctls_limit(aud->fd, cmds, nitems(cmds)) == -1)
		errx(EX_OSERR, "Unable to limit ioctl rights for sandbox");
#endif

	return aud;
}

/*
 * audio_set_params - reset the sound device and set the audio params
 * @aud - the audio player to be configured
 * @params - the audio parameters to be set
 */
int
audio_set_params(struct audio *aud, struct audio_params *params)
{
	int audio_fd;
	int format, channels, rate;
	int err;
#if DEBUG_HDA == 1
	audio_buf_info info;
#endif

	assert(aud);
	assert(params);

	if (aud->ring != NULL) {	/* keel: 16-bit, 1 or 2 channels (the codec offers no more) */
		if (params->format != AFMT_S16_LE || params->channels < 1 ||
		    params->channels > 2)
			return (-1);
		aud->channels = params->channels;
		if (aud->ring->rate != (uint32_t)params->rate) {
			aud->ring->rate = params->rate;
		}
		atomic_store_rel_32(&aud->ring->epoch, aud->ring->epoch + 1);
		return (0);
	}

	if ((audio_fd = aud->fd) < 0) {
		DPRINTF("Incorrect audio device descriptor for %s",
		    aud->dev_name);
		return (-1);
	}

	/* Reset the device if it was previously opened */
	if (aud->inited) {
		err = ioctl(audio_fd, SNDCTL_DSP_RESET, NULL);
		if (err == -1) {
			DPRINTF("Failed to reset fd: %d, errno: %d",
			    aud->fd, errno);
			return (-1);
		}
	} else
		aud->inited = 1;

	/* Set the Format (Bits per Sample) */
	format = params->format;
	err = ioctl(audio_fd, SNDCTL_DSP_SETFMT, &format);
	if (err == -1) {
		DPRINTF("Fail to set fmt: 0x%x errno: %d",
		    params->format, errno);
		return -1;
	}

	/* The device does not support the requested audio format */
	if (format != params->format) {
		DPRINTF("Mismatch format: 0x%x params->format: 0x%x",
		    format, params->format);
		return -1;
	}

	/* Set the Number of Channels */
	channels = params->channels;
	err = ioctl(audio_fd, SNDCTL_DSP_CHANNELS, &channels);
	if (err == -1) {
		DPRINTF("Fail to set channels: %d errno: %d",
		    params->channels, errno);
		return -1;
	}

	/* The device does not support the requested no. of channels */
	if (channels != params->channels) {
		DPRINTF("Mismatch channels: %d params->channels: %d",
		    channels, params->channels);
		return -1;
	}

	/* Set the Sample Rate / Speed */
	rate = params->rate;
	err = ioctl(audio_fd, SNDCTL_DSP_SPEED, &rate);
	if (err == -1) {
		DPRINTF("Fail to set speed: %d errno: %d",
		    params->rate, errno);
		return -1;
	}

	/* The device does not support the requested rate / speed */
	if (rate != params->rate) {
		DPRINTF("Mismatch rate: %d params->rate: %d",
		    rate, params->rate);
		return -1;
	}

#if DEBUG_HDA == 1
	err = ioctl(audio_fd, aud->dir ? SNDCTL_DSP_GETOSPACE :
	    SNDCTL_DSP_GETISPACE, &info);
	if (err == -1) {
		DPRINTF("Fail to get audio buf info errno: %d", errno);
		return -1;
	}
	DPRINTF("fragstotal: 0x%x fragsize: 0x%x",
	    info.fragstotal, info.fragsize);
#endif
	return 0;
}

/*
 * audio_playback - plays samples to the sound device using blocking operations
 * @aud - the audio player used to play the samples
 * @buf - the buffer containing the samples
 * @count - the number of bytes in buffer
 */
int
audio_playback(struct audio *aud, const uint8_t *buf, size_t count)
{
	ssize_t len;
	size_t total;
	int audio_fd;

	assert(aud);
	assert(aud->dir);
	assert(buf);

	if (aud->ring != NULL) {	/* keel */
		keel_snd_play(aud, buf, count);
		return (0);
	}

	audio_fd = aud->fd;
	assert(audio_fd != -1);

	for (total = 0; total < count; total += len) {
		len = write(audio_fd, buf + total, count - total);
		if (len < 0) {
			DPRINTF("Fail to write to fd: %d, errno: %d",
			    audio_fd, errno);
			return -1;
		}
	}

	return 0;
}

/*
 * audio_record - records samples from the sound device using
 * blocking operations.
 * @aud - the audio player used to capture the samples
 * @buf - the buffer to receive the samples
 * @count - the number of bytes to capture in buffer
 * Returns -1 on error and 0 on success
 */
int
audio_record(struct audio *aud, uint8_t *buf, size_t count)
{
	ssize_t len;
	size_t total;
	int audio_fd;

	assert(aud);
	assert(!aud->dir);
	assert(buf);

	audio_fd = aud->fd;
	assert(audio_fd != -1);

	for (total = 0; total < count; total += len) {
		len = read(audio_fd, buf + total, count - total);
		if (len < 0) {
			DPRINTF("Fail to write to fd: %d, errno: %d",
			    audio_fd, errno);
			return -1;
		}
	}

	return 0;
}
