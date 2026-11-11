#!/usr/bin/env python3
"""Generate usb_kbd.c, a USB HID boot keyboard for bhyve's xHCI, from bhyve's usb_mouse.c.

    gen-usbkbd.py /usr/src/usr.sbin/bhyve/usb_mouse.c > usb_kbd.c

bhyve emulates only a USB tablet; keyboards are PS/2. macOS has no PS/2 driver (docs/28 X2),
and a USB keyboard is what every OS understands. The device ("-s 30,xhci,tablet,kbd") reuses the
tablet's descriptors and request handling and changes:
  - interface protocol boot keyboard, the standard boot keyboard report descriptor and 8-byte
    report (modifiers, reserved, six key usages); product 0xfb5d:0x0002, "HID Keyboard";
  - input: console_kbd_register() with priority 10, above ps2kbd (1): with the USB keyboard in
    the VM, VNC and keel-rdp keys go to it. keel-rdp passes a PC set 1 scancode (0x80 set for
    an E0 key, docs/19), VNC an X11 keysym: both are translated to HID usages;
  - every key event queues a copy of the report (up to 128), and each interrupt IN transfer
    takes one: a press and release between two polls are not lost.
Each replacement must match exactly once.
"""
import sys

src = open(sys.argv[1]).read()

def rep(old, new):
    global src
    n = src.count(old)
    if n != 1:
        sys.exit("gen-usbkbd: expected exactly one match, found %d:\n%s" % (n, old))
    src = src.replace(old, new)

rep('''#include <stdlib.h>
''', '''#include <stdlib.h>
#include <unistd.h>
''')

# --- the device ---------------------------------------------------------------------------
rep('''	"HID Tablet",
	"01",
	"HID Tablet Device",
''', '''	"HID Keyboard",
	"01",
	"HID Keyboard Device",
''')
rep('''		.bInterfaceProtocol = UIPROTO_MOUSE,''', '''		.bInterfaceProtocol = UIPROTO_BOOT_KEYBOARD,''')
rep('''	MSETW(.idProduct, 0x0001),		/* product */''', '''	MSETW(.idProduct, 0x0002),		/* product: keyboard */''')

a = src.index('static const uint8_t umouse_report_desc[] = {')
b = src.index('struct umouse_report {')
src = src[:a] + '''static const uint8_t umouse_report_desc[] = {	/* boot keyboard (HID 1.11 appendix B) */
	0x05, 0x01,		/* USAGE_PAGE (Generic Desktop)		*/
	0x09, 0x06,		/* USAGE (Keyboard)			*/
	0xa1, 0x01,		/* COLLECTION (Application)		*/
	0x05, 0x07,		/*   USAGE_PAGE (Keyboard)		*/
	0x19, 0xe0,		/*   USAGE_MINIMUM (Left Control)	*/
	0x29, 0xe7,		/*   USAGE_MAXIMUM (Right GUI)		*/
	0x15, 0x00,		/*   LOGICAL_MINIMUM (0)		*/
	0x25, 0x01,		/*   LOGICAL_MAXIMUM (1)		*/
	0x75, 0x01,		/*   REPORT_SIZE (1)			*/
	0x95, 0x08,		/*   REPORT_COUNT (8)			*/
	0x81, 0x02,		/*   INPUT (Data,Var,Abs): modifiers	*/
	0x95, 0x01,		/*   REPORT_COUNT (1)			*/
	0x75, 0x08,		/*   REPORT_SIZE (8)			*/
	0x81, 0x01,		/*   INPUT (Cnst): reserved		*/
	0x95, 0x05,		/*   REPORT_COUNT (5)			*/
	0x75, 0x01,		/*   REPORT_SIZE (1)			*/
	0x05, 0x08,		/*   USAGE_PAGE (LEDs)			*/
	0x19, 0x01,		/*   USAGE_MINIMUM (Num Lock)		*/
	0x29, 0x05,		/*   USAGE_MAXIMUM (Kana)		*/
	0x91, 0x02,		/*   OUTPUT (Data,Var,Abs): LEDs	*/
	0x95, 0x01,		/*   REPORT_COUNT (1)			*/
	0x75, 0x03,		/*   REPORT_SIZE (3)			*/
	0x91, 0x01,		/*   OUTPUT (Cnst): padding		*/
	0x95, 0x06,		/*   REPORT_COUNT (6)			*/
	0x75, 0x08,		/*   REPORT_SIZE (8)			*/
	0x15, 0x00,		/*   LOGICAL_MINIMUM (0)		*/
	0x25, 0x65,		/*   LOGICAL_MAXIMUM (101)		*/
	0x05, 0x07,		/*   USAGE_PAGE (Keyboard)		*/
	0x19, 0x00,		/*   USAGE_MINIMUM (0)			*/
	0x29, 0x65,		/*   USAGE_MAXIMUM (101)		*/
	0x81, 0x00,		/*   INPUT (Data,Ary,Abs): keys		*/
	0xc0			/* END_COLLECTION			*/
};

''' + src[b:]

rep('''struct umouse_report {
	uint8_t	buttons;	/* bits: 0 left, 1 right, 2 middle */
	int16_t	x;		/* x position */
	int16_t	y;		/* y position */
	int8_t	z;		/* z wheel position */
} __packed;
''', '''struct umouse_report {
	uint8_t	mods;		/* bits: LCtrl LShift LAlt LGUI RCtrl RShift RAlt RGUI */
	uint8_t	rsvd;
	uint8_t	keys[6];	/* usages of the keys down */
} __packed;

#define	UKBD_QLEN	128
''')

rep('''	struct umouse_report um_report;
	int	newdata;
''', '''	struct umouse_report um_report;	/* the keys down now */
	int	newdata;
	struct umouse_report q[UKBD_QLEN];	/* reports not yet sent */
	int	qhead, qlen;
''')

# --- input: set 1 scancodes (keel-rdp) and X11 keysyms (VNC) to HID usages ---------------------
a = src.index('static void\numouse_event(')
b = src.index('static void *\numouse_probe(')	# stable/15: ue_probe allocates, ue_init sets up
src = src[:a] + '''/* PC set 1 scancode to HID usage; [0x80 | code] for E0 keys (QEMU's convention, ps2kbd.c) */
static const uint8_t ukbd_set1[256] = {
	[0x01] = 0x29, [0x02] = 0x1e, [0x03] = 0x1f, [0x04] = 0x20, [0x05] = 0x21,
	[0x06] = 0x22, [0x07] = 0x23, [0x08] = 0x24, [0x09] = 0x25, [0x0a] = 0x26,
	[0x0b] = 0x27, [0x0c] = 0x2d, [0x0d] = 0x2e, [0x0e] = 0x2a, [0x0f] = 0x2b,
	[0x10] = 0x14, [0x11] = 0x1a, [0x12] = 0x08, [0x13] = 0x15, [0x14] = 0x17,
	[0x15] = 0x1c, [0x16] = 0x18, [0x17] = 0x0c, [0x18] = 0x12, [0x19] = 0x13,
	[0x1a] = 0x2f, [0x1b] = 0x30, [0x1c] = 0x28, [0x1d] = 0xe0, [0x1e] = 0x04,
	[0x1f] = 0x16, [0x20] = 0x07, [0x21] = 0x09, [0x22] = 0x0a, [0x23] = 0x0b,
	[0x24] = 0x0d, [0x25] = 0x0e, [0x26] = 0x0f, [0x27] = 0x33, [0x28] = 0x34,
	[0x29] = 0x35, [0x2a] = 0xe1, [0x2b] = 0x31, [0x2c] = 0x1d, [0x2d] = 0x1b,
	[0x2e] = 0x06, [0x2f] = 0x19, [0x30] = 0x05, [0x31] = 0x11, [0x32] = 0x10,
	[0x33] = 0x36, [0x34] = 0x37, [0x35] = 0x38, [0x36] = 0xe5, [0x37] = 0x55,
	[0x38] = 0xe2, [0x39] = 0x2c, [0x3a] = 0x39, [0x3b] = 0x3a, [0x3c] = 0x3b,
	[0x3d] = 0x3c, [0x3e] = 0x3d, [0x3f] = 0x3e, [0x40] = 0x3f, [0x41] = 0x40,
	[0x42] = 0x41, [0x43] = 0x42, [0x44] = 0x43, [0x45] = 0x53, [0x46] = 0x47,
	[0x47] = 0x5f, [0x48] = 0x60, [0x49] = 0x61, [0x4a] = 0x56, [0x4b] = 0x5c,
	[0x4c] = 0x5d, [0x4d] = 0x5e, [0x4e] = 0x57, [0x4f] = 0x59, [0x50] = 0x5a,
	[0x51] = 0x5b, [0x52] = 0x62, [0x53] = 0x63, [0x56] = 0x64, [0x57] = 0x44,
	[0x58] = 0x45,
	[0x9c] = 0x58, [0x9d] = 0xe4, [0xb5] = 0x54, [0xb7] = 0x46, [0xb8] = 0xe6,
	[0xc7] = 0x4a, [0xc8] = 0x52, [0xc9] = 0x4b, [0xcb] = 0x50, [0xcd] = 0x4f,
	[0xcf] = 0x4d, [0xd0] = 0x51, [0xd1] = 0x4e, [0xd2] = 0x49, [0xd3] = 0x4c,
	[0xdb] = 0xe3, [0xdc] = 0xe7, [0xdd] = 0x65,
};

/* printable ASCII keysyms (US layout; shifted characters on the same key) */
static const uint8_t ukbd_ascii[128] = {
	[' '] = 0x2c, ['!'] = 0x1e, ['"'] = 0x34, ['#'] = 0x20, ['$'] = 0x21,
	['%'] = 0x22, ['&'] = 0x24, ['\\''] = 0x34, ['('] = 0x26, [')'] = 0x27,
	['*'] = 0x25, ['+'] = 0x2e, [','] = 0x36, ['-'] = 0x2d, ['.'] = 0x37,
	['/'] = 0x38, ['0'] = 0x27, ['1'] = 0x1e, ['2'] = 0x1f, ['3'] = 0x20,
	['4'] = 0x21, ['5'] = 0x22, ['6'] = 0x23, ['7'] = 0x24, ['8'] = 0x25,
	['9'] = 0x26, [':'] = 0x33, [';'] = 0x33, ['<'] = 0x36, ['='] = 0x2e,
	['>'] = 0x37, ['?'] = 0x38, ['@'] = 0x1f, ['['] = 0x2f, ['\\\\'] = 0x31,
	[']'] = 0x30, ['^'] = 0x23, ['_'] = 0x2d, ['`'] = 0x35, ['{'] = 0x2f,
	['|'] = 0x31, ['}'] = 0x30, ['~'] = 0x35,
};

static uint8_t
ukbd_keysym(uint32_t keysym)
{
	static const struct { uint32_t sym; uint8_t usage; } t[] = {
		{ 0xff0d, 0x28 }, { 0xff1b, 0x29 }, { 0xff08, 0x2a }, { 0xff09, 0x2b },
		{ 0xffe5, 0x39 }, { 0xff61, 0x46 }, { 0xff14, 0x47 }, { 0xff13, 0x48 },
		{ 0xff63, 0x49 }, { 0xff50, 0x4a }, { 0xff55, 0x4b }, { 0xffff, 0x4c },
		{ 0xff57, 0x4d }, { 0xff56, 0x4e }, { 0xff53, 0x4f }, { 0xff51, 0x50 },
		{ 0xff54, 0x51 }, { 0xff52, 0x52 }, { 0xff7f, 0x53 }, { 0xff67, 0x65 },
		{ 0xff8d, 0x58 },
		{ 0xffe3, 0xe0 }, { 0xffe1, 0xe1 }, { 0xffe9, 0xe2 }, { 0xffeb, 0xe3 },
		{ 0xffe7, 0xe3 }, { 0xffe4, 0xe4 }, { 0xffe2, 0xe5 }, { 0xffea, 0xe6 },
		{ 0xfe03, 0xe6 }, { 0xffec, 0xe7 }, { 0xffe8, 0xe7 },
		{ 0, 0 }
	};

	if (keysym >= 'a' && keysym <= 'z')
		return (0x04 + keysym - 'a');
	if (keysym >= 'A' && keysym <= 'Z')
		return (0x04 + keysym - 'A');
	if (keysym < 0x80)
		return (ukbd_ascii[keysym]);
	if (keysym >= 0xffbe && keysym <= 0xffc9)	/* F1-F12 */
		return (0x3a + keysym - 0xffbe);
	for (int i = 0; t[i].sym != 0; i++)
		if (t[i].sym == keysym)
			return (t[i].usage);
	return (0);
}

static void
umouse_event(int down, uint32_t keysym, uint32_t keycode, void *arg)
{
	struct umouse_softc *sc = arg;
	struct umouse_report *r;
	uint8_t u;
	int i, j;

	u = keycode != 0 ? ukbd_set1[keycode & 0xff] : ukbd_keysym(keysym);
	DPRINTF(("ukbd: key %s keysym 0x%x keycode 0x%x -> usage 0x%x",
	    down ? "down" : "up", keysym, keycode, u));
	if (u == 0) {
		DPRINTF(("ukbd: no usage for keysym 0x%x keycode 0x%x", keysym, keycode));
		return;
	}

	pthread_mutex_lock(&sc->mtx);
	r = &sc->um_report;
	if (u >= 0xe0) {
		if (down)
			r->mods |= 1 << (u - 0xe0);
		else
			r->mods &= ~(1 << (u - 0xe0));
	} else {
		for (i = 0; i < 6 && r->keys[i] != u; i++)
			;
		if (down && i == 6) {		/* not down yet: first free slot */
			for (i = 0; i < 6 && r->keys[i] != 0; i++)
				;
			if (i < 6)
				r->keys[i] = u;
		} else if (!down && i < 6) {	/* up: close the gap */
			for (j = i; j < 5; j++)
				r->keys[j] = r->keys[j + 1];
			r->keys[5] = 0;
		}
	}
	if (sc->qlen < UKBD_QLEN) {
		sc->q[(sc->qhead + sc->qlen) % UKBD_QLEN] = *r;
		sc->qlen++;
	}
	sc->newdata = 1;
	pthread_mutex_unlock(&sc->mtx);

	pthread_mutex_lock(&sc->ev_mtx);
	sc->hci->hci_intr(sc->hci, UE_DIR_IN | UMOUSE_INTR_ENDPT);
	pthread_mutex_unlock(&sc->ev_mtx);
}

''' + src[b:]

rep('''	sc->hci = hci;
''', '''	sc->hci = hci;
	if (access("/var/run/keel-xhci-debug", F_OK) == 0)	/* keel: as patch-xhci.py */
		umouse_debug = 1;
''')
rep('''	console_ptr_register(umouse_event, sc, 10);''',
    '''	console_kbd_register(umouse_event, sc, 10);	/* above ps2kbd (1) */''')

# --- interrupt IN: one queued report per transfer ------------------------------------------
rep('''		if (len > 0) {
			sc->newdata = 0;

			data->processed = 1;
			data->bdone += 6;
			memcpy(udata, &sc->um_report, 6);
			data->blen = len - 6;
			if (data->blen > 0)
				err = USB_ERR_SHORT_XFER;
		}
''', '''		if (len > 0) {
			struct umouse_report *r = &sc->um_report;
			int n = sizeof(*r) < (size_t)len ? (int)sizeof(*r) : len;

			if (sc->qlen > 0) {
				r = &sc->q[sc->qhead];
				sc->qhead = (sc->qhead + 1) % UKBD_QLEN;
				sc->qlen--;
			}
			sc->newdata = sc->qlen > 0;

			data->processed = 1;
			data->bdone += n;
			memcpy(udata, r, n);
			data->blen = len - n;
			if (data->blen > 0)
				err = USB_ERR_SHORT_XFER;
		}
''')

rep('''	sc->newdata = 0;

	return (0);''', '''	sc->newdata = 0;
	sc->qlen = 0;
	memset(&sc->um_report, 0, sizeof(sc->um_report));

	return (0);''')

# --- names ---------------------------------------------------------------------------------
# the reports not yet sent are in the checkpoint too (a lost key-up would leave the key down)
rep('''	SNAPSHOT_VAR_OR_LEAVE(sc->newdata, meta, ret, done);
''', '''	SNAPSHOT_VAR_OR_LEAVE(sc->newdata, meta, ret, done);
	SNAPSHOT_BUF_OR_LEAVE(sc->q, sizeof(sc->q), meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->qhead, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->qlen, meta, ret, done);
	if (meta->op == VM_SNAPSHOT_RESTORE &&
	    (sc->qhead < 0 || sc->qhead >= UKBD_QLEN || sc->qlen < 0 || sc->qlen > UKBD_QLEN))
		sc->qhead = sc->qlen = 0;
''')
rep('''	.ue_emu =	"tablet",''', '''	.ue_emu =	"kbd",''')
src = src.replace('ue_mouse', 'ue_kbd')
src = src.replace('umouse', 'ukbd').replace('UMOUSE', 'UKBD')

hdr = '''/*
 * Generated by keelOS root/bhyve/gen-usbkbd.py from bhyve's usb_mouse.c: a USB HID boot
 * keyboard (docs/28). Do not edit; change the generator.
 */
'''
sys.stdout.write(hdr + src)
