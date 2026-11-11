/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * A TPM 2.0 inside the bhyve process: libtpms (the library swtpm is built on) linked in, so no
 * second program runs beside the VM.
 *
 *	-o tpm.type=libtpms -o tpm.path=DIR -o tpm.version=2.0
 *
 * DIR holds the TPM's state (its seeds, keys and NV indices: "tpm2.permall" and what else libtpms
 * asks to store), one file per name, each replaced by a rename. A new bhyve process starts the TPM
 * as a power-on does: the firmware here does not send TPM2_Startup, so this does (as swtpm's
 * "startup-clear" flag).
 *
 * Built only when libtpms is there (Makefile: LIBTPMS).
 */

#include <sys/types.h>
#include <sys/stat.h>

#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <libtpms/tpm_error.h>
#include <libtpms/tpm_library.h>
#include <libtpms/tpm_nvfilename.h>
#include <libtpms/tpm_types.h>

#include "config.h"
#include "tpm_device.h"
#include "tpm_emul.h"

/* libtpms keeps one TPM per process, and its callbacks carry no argument. */
static char tpm_dir[PATH_MAX];
static int tpm_inited;

static void
state_path(char *buf, size_t size, const char *name, const char *suffix)
{

	snprintf(buf, size, "%s/tpm2.%s%s", tpm_dir, name, suffix);
}

/*
 * The state swtpm kept for this VM (DIR/tpm2-00.permall), from when its TPM was an swtpm process:
 * a 10-byte header (version, minimum version, header size, flags, total length; big-endian), then
 * libtpms's own blob as a tagged part. Taken over once, so the VM keeps the TPM it had (its keys, what the guest
 * stored in it). Not when swtpm encrypted it (flags) or the header is not what is known here.
 */
static TPM_RESULT
swtpm_state(unsigned char **data, uint32_t *length)
{
	char path[PATH_MAX];
	unsigned char hdr[10], tlv[6], *buf;
	struct stat sb;
	uint32_t hdrsize, len, off, total;
	int fd;

	snprintf(path, sizeof(path), "%s/tpm2-00.permall", tpm_dir);
	fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return (TPM_RETRY);
	if (fstat(fd, &sb) != 0 || read(fd, hdr, sizeof(hdr)) != sizeof(hdr))
		goto unknown;
	hdrsize = hdr[2] << 8 | hdr[3];
	total = (uint32_t)hdr[6] << 24 | hdr[7] << 16 | hdr[8] << 8 | hdr[9];
	if (hdr[0] != 2 || hdrsize < sizeof(hdr) || hdrsize > 64 || (hdr[4] << 8 | hdr[5]) != 0 ||
	    total != sb.st_size || total <= hdrsize)
		goto unknown;
	/* then tagged parts (tag 16 bits, length 32 bits); tag 1 is libtpms's blob */
	for (off = hdrsize; off + 6 <= total; off += 6 + len) {
		if (pread(fd, tlv, sizeof(tlv), off) != sizeof(tlv))
			goto unknown;
		len = (uint32_t)tlv[2] << 24 | tlv[3] << 16 | tlv[4] << 8 | tlv[5];
		if (len > total - off - 6)
			goto unknown;
		if ((tlv[0] << 8 | tlv[1]) != 1)
			continue;
		buf = malloc(len);
		if (buf == NULL || pread(fd, buf, len, off + 6) != (ssize_t)len) {
			free(buf);
			goto unknown;
		}
		close(fd);
		warnx("tpm: taking over swtpm's state (%s)", path);
		*data = buf;
		*length = len;
		return (TPM_SUCCESS);
	}
unknown:
	close(fd);
	warnx("tpm: %s is not a state this can take over: the VM gets a new TPM", path);
	return (TPM_RETRY);
}

static TPM_RESULT
nvram_init(void)
{

	return (TPM_SUCCESS);
}

static TPM_RESULT
nvram_loaddata(unsigned char **data, uint32_t *length, uint32_t tpm_number __unused,
    const char *name)
{
	char path[PATH_MAX];
	struct stat sb;
	unsigned char *buf;
	ssize_t n;
	int fd;

	state_path(path, sizeof(path), name, "");
	fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) {
		/* TPM_RETRY: "no such state", libtpms then makes a new TPM */
		if (errno == ENOENT)
			return (strcmp(name, TPM_PERMANENT_ALL_NAME) == 0 ? swtpm_state(data, length) : TPM_RETRY);
		warn("tpm: %s", path);
		return (TPM_FAIL);
	}
	if (fstat(fd, &sb) != 0 || sb.st_size <= 0 || sb.st_size > 16 * 1024 * 1024) {
		warnx("tpm: %s: not a state file", path);
		close(fd);
		return (TPM_FAIL);
	}
	buf = malloc(sb.st_size);	/* libtpms frees it */
	if (buf == NULL) {
		close(fd);
		return (TPM_FAIL);
	}
	n = read(fd, buf, sb.st_size);
	close(fd);
	if (n != sb.st_size) {
		warnx("tpm: %s: short read", path);
		free(buf);
		return (TPM_FAIL);
	}
	*data = buf;
	*length = (uint32_t)n;
	return (TPM_SUCCESS);
}

static TPM_RESULT
nvram_storedata(const unsigned char *data, uint32_t length, uint32_t tpm_number __unused,
    const char *name)
{
	char path[PATH_MAX], tmp[PATH_MAX];
	ssize_t n;
	int fd;

	state_path(path, sizeof(path), name, "");
	state_path(tmp, sizeof(tmp), name, ".new");
	fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
	if (fd < 0) {
		warn("tpm: %s", tmp);
		return (TPM_FAIL);
	}
	n = write(fd, data, length);
	if (n != (ssize_t)length || fsync(fd) != 0) {
		warn("tpm: writing %s", tmp);
		close(fd);
		unlink(tmp);
		return (TPM_FAIL);
	}
	close(fd);
	if (rename(tmp, path) != 0) {
		warn("tpm: %s", path);
		unlink(tmp);
		return (TPM_FAIL);
	}
	return (TPM_SUCCESS);
}

static TPM_RESULT
nvram_deletename(uint32_t tpm_number __unused, const char *name, TPM_BOOL must_exist)
{
	char path[PATH_MAX];

	state_path(path, sizeof(path), name, "");
	if (unlink(path) != 0 && (errno != ENOENT || must_exist))
		return (TPM_FAIL);
	return (TPM_SUCCESS);
}

static TPM_RESULT
io_init(void)
{

	return (TPM_SUCCESS);
}

static TPM_RESULT
io_getlocality(TPM_MODIFIER_INDICATOR *locality, uint32_t tpm_number __unused)
{

	*locality = 0;
	return (TPM_SUCCESS);
}

static TPM_RESULT
io_getphysicalpresence(TPM_BOOL *present, uint32_t tpm_number __unused)
{

	*present = FALSE;
	return (TPM_SUCCESS);
}

static int
tpm_libtpms_execute_cmd(void *sc __unused, void *cmd, uint32_t cmd_size, void *rsp,
    uint32_t rsp_size)
{
	unsigned char *out;
	uint32_t out_len, out_size;
	TPM_RESULT res;

	out = NULL;
	out_len = out_size = 0;
	res = TPMLIB_Process(&out, &out_len, &out_size, cmd, cmd_size);
	if (res != TPM_SUCCESS || out == NULL || out_len > rsp_size) {
		warnx("tpm: command failed (libtpms %#x, %u bytes for a buffer of %u)", res, out_len,
		    rsp_size);
		free(out);
		return (EFAULT);
	}
	memcpy(rsp, out, out_len);
	free(out);
	return (0);
}

static int
tpm_libtpms_init(void **sc, nvlist_t *nvl)
{
	/* TPM2_Startup(TPM_SU_CLEAR) */
	static unsigned char startup[] = { 0x80, 0x01, 0, 0, 0, 0x0c, 0, 0, 0x01, 0x44, 0, 0 };
	struct libtpms_callbacks cbs = {
		.sizeOfStruct = sizeof(cbs),
		.tpm_nvram_init = nvram_init,
		.tpm_nvram_loaddata = nvram_loaddata,
		.tpm_nvram_storedata = nvram_storedata,
		.tpm_nvram_deletename = nvram_deletename,
		.tpm_io_init = io_init,
		.tpm_io_getlocality = io_getlocality,
		.tpm_io_getphysicalpresence = io_getphysicalpresence,
	};
	unsigned char rsp[64];
	const char *path;
	TPM_RESULT res;

	if (tpm_inited) {
		warnx("tpm: one libtpms TPM per VM");
		return (EBUSY);
	}
	path = get_config_value_node(nvl, "path");
	if (path == NULL) {
		warnx("tpm: libtpms needs tpm.path=DIR (where the TPM's state is kept)");
		return (ENOENT);
	}
	if (mkdir(path, 0700) != 0 && errno != EEXIST) {
		warn("tpm: %s", path);
		return (errno);
	}
	strlcpy(tpm_dir, path, sizeof(tpm_dir));

	if ((res = TPMLIB_ChooseTPMVersion(TPMLIB_TPM_VERSION_2)) != TPM_SUCCESS ||
	    (res = TPMLIB_RegisterCallbacks(&cbs)) != TPM_SUCCESS ||
	    (res = TPMLIB_MainInit()) != TPM_SUCCESS) {
		warnx("tpm: libtpms did not start (%#x); is %s the state of another TPM?", res, path);
		return (EIO);
	}
	tpm_inited = 1;
	if (tpm_libtpms_execute_cmd(NULL, startup, sizeof(startup), rsp, sizeof(rsp)) != 0)
		warnx("tpm: TPM2_Startup failed");
	*sc = &tpm_inited;
	return (0);
}

static void
tpm_libtpms_deinit(void *sc __unused)
{

	if (tpm_inited) {
		TPMLIB_Terminate();
		tpm_inited = 0;
	}
}

static const struct tpm_emul tpm_emul_libtpms = {
	.name = "libtpms",
	.init = tpm_libtpms_init,
	.deinit = tpm_libtpms_deinit,
	.execute_cmd = tpm_libtpms_execute_cmd,
};
TPM_EMUL_SET(tpm_emul_libtpms);
