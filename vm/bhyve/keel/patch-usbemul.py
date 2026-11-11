#!/usr/bin/env python3
"""Generate bhyve's usb_emul.h for asynchronous (remote) USB devices (docs/66 §4.3, §10.1).

    patch-usbemul.py /usr/src/usr.sbin/bhyve/usb_emul.h > usb_emul.h

- USB_MAX_XFER_BLOCKS 8 -> 256: a TD of a real device (Linux/Windows bulk transfers with
  scatter/gather) has tens of TRBs; bhyve's own devices never queue more than a few. The
  checkpoint keeps its 8 blocks per endpoint (patch-xhci.py writes the live blocks first, so the
  format does not change).
- USB_PENDING: ue_request()/ue_data() of a device that answers later (usb_remote.c) return it;
  the xHCI leaves the TD open and the endpoint Running. Not a usb_error_t (those are < 0x100).
- struct usb_devemu: ue_cancel(sc, xfer) forgets the request of that transfer (Stop/Reset
  Endpoint, Disable Slot, HCRST, endpoint reconfigured); ue_event(sc, ev, arg) tells the device
  about a port reset (KEEL_UE_PORT_RESET) or a Reset Endpoint command (KEEL_UE_EP_RESET, arg =
  endpoint address). NULL for bhyve's own devices.
- struct usb_hci: hci_complete(hci, epctx, xfer): the device has the answer for the request it
  returned USB_PENDING for; epctx is the xHCI endpoint index (DCI: 1 = control, 2*n+1 = IN n,
  2*n = OUT n). The controller takes its lock and calls ue_request()/ue_data() again with the
  same xfer, which now returns the result. hci_event(hci, USBDEV_ATTACH, &speed) and
  (hci, USBDEV_REMOVE, NULL) connect and disconnect the port (speed: USB_SPEED_*).
Each replacement must match exactly once.
"""
import sys

src = open(sys.argv[1]).read()


def rep(old, new, count=1):
    global src
    n = src.count(old)
    if n != count:
        sys.exit("patch-usbemul: expected %d match(es), found %d:\n%s" % (count, n, old))
    src = src.replace(old, new)


rep('''#define	USB_MAX_XFER_BLOCKS	8
''', '''#define	USB_MAX_XFER_BLOCKS	256	/* keel: 8 (patch-usbemul.py) */
#define	KEEL_SNAP_XFER_BLOCKS	8	/* keel: blocks per endpoint in the checkpoint */

/* keel: ue_request()/ue_data(): the answer comes later through hci_complete() */
#define	USB_PENDING		0x1000

/* keel: ue_event() */
#define	KEEL_UE_PORT_RESET	1	/* the guest reset the port */
#define	KEEL_UE_EP_RESET	2	/* Reset Endpoint command; arg = endpoint address */
''')

rep('''	int	(*ue_snapshot)(void *scarg, struct vm_snapshot_meta *meta);
};
''', '''	int	(*ue_snapshot)(void *scarg, struct vm_snapshot_meta *meta);

	/* keel: asynchronous devices (usb_remote.c); NULL for the others */
	void	(*ue_cancel)(void *sc, struct usb_data_xfer *xfer);
	void	(*ue_event)(void *sc, int ev, int arg);
};
''')

rep('''	int	(*hci_event)(struct usb_hci *hci, enum hci_usbev evid,
		             void *param);
''', '''	int	(*hci_event)(struct usb_hci *hci, enum hci_usbev evid,
		             void *param);
	/* keel: a USB_PENDING request of endpoint epctx (xHCI DCI) has its answer */
	int	(*hci_complete)(struct usb_hci *hci, int epctx,
		             struct usb_data_xfer *xfer);
''')

sys.stdout.write(src)
