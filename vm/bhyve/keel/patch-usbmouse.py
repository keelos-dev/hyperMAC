#!/usr/bin/env python3
"""Generate bhyve's usb_mouse.c (the xHCI tablet) whose clicks are not lost.

    patch-usbmouse.py /usr/src/usr.sbin/bhyve/usb_mouse.c > usb_mouse.c

Stock bhyve's tablet keeps one report: every pointer event (VNC, keel-rdp through the display
channel) overwrites it, and the guest's next interrupt IN transfer takes whatever is there. A
button press and its release that both come before that transfer leave a report with the button
up: the guest never sees the click. The interval between transfers is the guest's: it queues
the next one when it has handled the last, so a busy guest (Windows 11 at "Getting devices
ready", 2026-10-05) loses short clicks, and an RDP client that sends press and release in one
input PDU (a touchpad tap) loses them on an idle guest too. The wheel had the same problem
(keel-rdp's input.rs sends a wheel step without its "release" for that reason; VNC's wheel, a
press and release of button 4/5, scrolled only when the guest happened to poll in between).

Here every event that changes the buttons or turns the wheel queues its own report (up to
UMOUSE_QLEN), and each interrupt IN transfer takes the oldest; a move with the same buttons
replaces the newest queued report instead (moves are not piled up, the pointer does not lag).
The queue full: the newest report is replaced, so the last state still arrives. A reset (the
guest's driver starting) empties the queue. um_report stays the latest state (GET_REPORT, the
checkpoint). The checkpoint format is unchanged: the queue is not saved, a restored tablet
sends the latest state (a click in the last poll interval before a suspend is lost, no button
stays down).
Debugging: with /var/run/keel-xhci-debug at bhyve's start (as patch-xhci.py) the tablet logs
events in, reports out, moves merged and reports dropped (queue full) to bhyve.log.
Each replacement must match exactly once.
"""
import sys

src = open(sys.argv[1]).read()

def rep(old, new):
    global src
    n = src.count(old)
    if n != 1:
        sys.exit("patch-usbmouse: expected exactly one match, found %d:\n%s" % (n, old))
    src = src.replace(old, new)

rep('''#include <stdlib.h>
''', '''#include <stdlib.h>
#include <unistd.h>
''')

rep('''#define	UMOUSE_INTR_ENDPT	1
''', '''#define	UMOUSE_INTR_ENDPT	1

/* keel: reports not yet sent (patch-usbmouse.py) */
#define	UMOUSE_QLEN		64
''')

rep('''	struct umouse_report um_report;
	int	newdata;
''', '''	struct umouse_report um_report;	/* the latest state */
	int	newdata;			/* keel: = qlen > 0 */
	struct umouse_report q[UMOUSE_QLEN];	/* keel: reports not yet sent, oldest first */
	int	qhead, qlen;
	uint64_t ev_in, rep_out, merged, dropped;	/* keel: counters for the debug log */
''')

# --- input: queue a report per button change or wheel step --------------------------------
rep('''	pthread_mutex_lock(&sc->mtx);

	sc->um_report.buttons = 0;
	sc->um_report.z = 0;
''', '''	pthread_mutex_lock(&sc->mtx);

	sc->ev_in++;
	sc->um_report.buttons = 0;
	sc->um_report.z = 0;
''')
rep('''	sc->um_report.y = MOUSE_MAX_Y * y / gc->height;
	sc->newdata = 1;
	pthread_mutex_unlock(&sc->mtx);
''', '''	sc->um_report.y = MOUSE_MAX_Y * y / gc->height;
	umouse_queue(sc, &sc->um_report);
	sc->newdata = 1;
	pthread_mutex_unlock(&sc->mtx);
''')
rep('''static void
umouse_event(uint8_t button, int x, int y, void *arg)
{''', '''/*
 * keel: queue r for the guest (sc->mtx held). A report with the buttons of the newest queued one
 * and no wheel (both) is a move: it replaces that one. Anything else gets its own report, so
 * the guest sees every press, release and wheel step, however fast they came.
 */
static void
umouse_queue(struct umouse_softc *sc, const struct umouse_report *r)
{
	struct umouse_report *last;

	last = sc->qlen > 0 ?
	    &sc->q[(sc->qhead + sc->qlen - 1) % UMOUSE_QLEN] : NULL;
	if (last != NULL && last->buttons == r->buttons && last->z == 0 &&
	    r->z == 0) {
		*last = *r;
		sc->merged++;
	} else if (sc->qlen < UMOUSE_QLEN) {
		sc->q[(sc->qhead + sc->qlen) % UMOUSE_QLEN] = *r;
		sc->qlen++;
	} else {
		*last = *r;		/* full: the newest state still arrives */
		sc->dropped++;
	}
}

static void
umouse_event(uint8_t button, int x, int y, void *arg)
{''')

rep('''	sc->hci = hci;
''', '''	sc->hci = hci;
	if (access("/var/run/keel-xhci-debug", F_OK) == 0)	/* keel: as patch-xhci.py */
		umouse_debug = 1;
''')

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
			int n = len < 6 ? len : 6;

			if (sc->qlen > 0) {
				r = &sc->q[sc->qhead];
				sc->qhead = (sc->qhead + 1) % UMOUSE_QLEN;
				sc->qlen--;
			}
			sc->newdata = sc->qlen > 0;
			sc->rep_out++;
			DPRINTF(("umouse: report buttons 0x%x x %d y %d z %d, "
			    "%d queued; events %ju reports %ju merged %ju "
			    "dropped %ju", r->buttons, r->x, r->y, r->z,
			    sc->qlen, (uintmax_t)sc->ev_in,
			    (uintmax_t)sc->rep_out, (uintmax_t)sc->merged,
			    (uintmax_t)sc->dropped));

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
	sc->qhead = sc->qlen = 0;

	return (0);''')

# the checkpoint keeps stock's format (no queue): restored, the latest state is sent once
rep('''	SNAPSHOT_VAR_OR_LEAVE(sc->newdata, meta, ret, done);
''', '''	SNAPSHOT_VAR_OR_LEAVE(sc->newdata, meta, ret, done);
	if (meta->op == VM_SNAPSHOT_RESTORE)
		sc->qhead = sc->qlen = 0;	/* keel: newdata sends um_report */
''')

hdr = '''/*
 * Generated by keelOS root/bhyve/patch-usbmouse.py from bhyve's usb_mouse.c: every click
 * and wheel step reaches the guest. Do not edit; change the generator.
 */
'''
sys.stdout.write(hdr + src)
