#!/usr/bin/env python3
"""Generate bhyve's pci_xhci.c with more than one interrupter (macOS, docs/28 X2).

    patch-xhci.py /usr/src/usr.sbin/bhyve/pci_xhci.c > pci_xhci.c

bhyve's xHCI has one interrupter (HCSPARAMS1.MaxIntrs = 1). macOS's AppleUSBXHCI refuses to
start with fewer than 3 ("1 interrupters supported, 3 are required"), so a macOS guest has no
USB and no tablet. With -o keel.xhci_intrs=N (2, 4 or 8: MSI message counts are powers of two) the controller has N interrupters, as the
spec has them: each its own register set (IMAN, IMOD, ERSTSZ, ERSTBA, ERDP) and event ring, and
MSI message i for interrupter i (the MSI capability offers N messages; with fewer enabled the
interrupt goes out on message 0). An interrupter whose IMAN.IE is off interrupts on interrupter
0's vector instead of not at all: macOS leaves IE off on its secondary interrupter (control
transfers target it) and drains that ring from interrupter 0's handler. Transfer events go to the interrupter the TRB names
(Interrupter Target, dwTrb2 bits 31:22), command completion and port status change events to
interrupter 0. An event for an interrupter whose event ring the guest has not set up goes to
interrupter 0. The runtime register space grows by 32 bytes per interrupter (the extended
capabilities move behind it). Without the option nothing changes (layout, snapshots). Stock
bhyve also asserted on a read past interrupter 0.
Control transfers: a control TD runs once its STATUS stage is in, also when earlier stages
have IOC set (macOS does that; bhyve ran the SETUP stage alone). Always on: other guests set
IOC only on the STATUS stage, where nothing changes.
LINK TRBs: with N > 1, a transfer ring's LINK TRB is followed only when its cycle bit is valid
(stock bhyve followed it regardless; macOS pre-writes zero LINKs in its 4-TRB segments).
Endpoint init: with N > 1, (re)initialising an endpoint drops the TDs pending from its old
ring (UEFI's keyboard driver leaves one; the first key press in macOS completed it instead).
Port speed: with N > 1 a USB 3 device's port reports SuperSpeed (speed ID 4). bhyve's devices
describe themselves as USB 3 (bcdUSB 3.0, 512-byte control packets, BOS) and sit on the USB 3
ports, but the ports said high speed (ID 3), which the USB 3 protocol does not have: macOS
Sonoma logged "operational speed 0 is not supported" and went on, macOS Tahoe gave up
("AppleUSBHostPort::createDevice: failed to create device", no keyboard, no tablet).
Snapshot: with N > 1 the checkpoint has, after stock's fields, N and interrupters 1..N-1
(registers, event ring position) and the pending-event mask; with N = 1 the format is stock's.
INTx: the line goes down again (stock bhyve asserted it in pci_xhci_deassert_interrupt() and never
dropped it; the firmware left it up at every start until the guest enabled MSI, docs/13).
Restore: the device context the checkpoint points at must still be one (2026-10-03, docs/09).
Stock bhyve keeps a slot's device context and endpoints until the guest disables the slot or
resets the controller; OVMF only halts the controller at ExitBootServices, so a guest without an
xHCI driver (Debian's cloud kernel) leaves the firmware's slot configured, pointing at memory the
OS has long reused. A restore re-read every endpoint context from there (pci_xhci_init_ep()) and,
when the garbage had MaxPStreams set, followed a stream-context pointer that did not translate:
SIGSEGV in about 1 of 15 resumes of a Debian guest. A restore now drops a slot whose slot
context does not name that slot and an addressed state, and an endpoint whose context is
disabled (their checkpoint bytes are read and thrown away); pci_xhci_init_ep() refuses a stream
array that does not translate (also a guest-triggerable crash of stock bhyve at Configure
Endpoint). The checkpoint format is unchanged.
Event ring and interrupts (2026-10-07, Windows reset the controller during fast drags over
RDP): a device's hci_intr runs under the controller's mutex; EHB per xHCI 4.17.2; HCRST clears
the interrupters; a pending IP interrupts when IE or USBCMD.INTE comes on; the ring's fill count
is kept right (see "fixes of 2026-10-07" below).
Debugging: -o keel.xhci_debug=true, or the file /var/run/keel-xhci-debug when bhyve starts,
turns on bhyve's DPRINTFs of the xHCI (commands, transfers, events) into bhyve.log;
-o keel.xhci_trace=true or /var/run/keel-xhci-trace a light trace (resets, commands, counters).
Each replacement must match exactly once.
"""
import sys

src = open(sys.argv[1]).read()

def rep(old, new, count=1):
    global src
    n = src.count(old)
    if n != count:
        sys.exit("patch-xhci: expected %d match(es), found %d:\n%s" % (count, n, old))
    src = src.replace(old, new)

rep('''struct pci_xhci_softc {
	struct pci_devinst *xsc_pi;
''', '''#define	KEEL_XHCI_MAXINTR	8
#define	KEEL_TRB_2_IRQ_GET(x)	(((x) >> 22) & 0x3FF)
/* keel: the speed ID in PORTSC: SuperSpeed for a USB 3 device (macOS profile) */
#define	KEEL_XHCI_SPEED(sc, dev)	keel_xhci_speed((dev)->hci.hci_usbver, \\
					    (dev)->hci.hci_speed)
static int keel_xhci_speed(int usbver, int usbspeed);

struct pci_xhci_softc {
	struct pci_devinst *xsc_pi;
	int		keel_nintr;	/* keel: interrupters */
	int		keel_target;	/* keel: interrupter of the next event */
	uint32_t	keel_pend;	/* keel: interrupters with new events */
	struct pci_xhci_rtsregs keel_rts[KEEL_XHCI_MAXINTR]; /* keel: 1..n-1 */
''')

# interrupter i's registers and ring: 0 is sc->rtsregs as before
rep('''static void
pci_xhci_assert_interrupt(struct pci_xhci_softc *sc)
{

	sc->rtsregs.intrreg.erdp |= XHCI_ERDP_LO_BUSY;
	sc->rtsregs.intrreg.iman |= XHCI_IMAN_INTR_PEND;
	sc->opregs.usbsts |= XHCI_STS_EINT;

	/* only trigger interrupt if permitted */
	if ((sc->opregs.usbcmd & XHCI_CMD_INTE) &&
	    (sc->rtsregs.intrreg.iman & XHCI_IMAN_INTR_ENA)) {
		if (pci_msi_enabled(sc->xsc_pi))
			pci_generate_msi(sc->xsc_pi, 0);
		else
			pci_lintr_assert(sc->xsc_pi);
	}
}
''', '''static struct pci_xhci_rtsregs *
keel_xhci_rts(struct pci_xhci_softc *sc, int i)
{

	return (i <= 0 || i >= sc->keel_nintr ? &sc->rtsregs : &sc->keel_rts[i]);
}

static void
pci_xhci_assert_interrupt(struct pci_xhci_softc *sc)
{
	struct pci_xhci_rtsregs *rts;
	uint32_t pend;
	int i, msg;

	/* keel: every interrupter that got an event (0 when none said) */
	pend = sc->keel_pend != 0 ? sc->keel_pend : 1;
	sc->keel_pend = 0;
	sc->opregs.usbsts |= XHCI_STS_EINT;
	for (i = 0; i < KEEL_XHCI_MAXINTR; i++) {
		if ((pend & (1u << i)) == 0)
			continue;
		rts = keel_xhci_rts(sc, i);
		rts->intrreg.erdp |= XHCI_ERDP_LO_BUSY;
		rts->intrreg.iman |= XHCI_IMAN_INTR_PEND;

		/* only trigger interrupt if permitted; an interrupter whose IE is off rings on
		   interrupter 0's vector: macOS leaves IE off on its secondary interrupter and
		   drains that ring from interrupter 0's handler */
		msg = i;
		if (i > 0 && (rts->intrreg.iman & XHCI_IMAN_INTR_ENA) == 0) {
			rts = &sc->rtsregs;
			msg = 0;
		}
		if ((sc->opregs.usbcmd & XHCI_CMD_INTE) &&
		    (rts->intrreg.iman & XHCI_IMAN_INTR_ENA)) {
			if (pci_msi_enabled(sc->xsc_pi))
				pci_generate_msi(sc->xsc_pi,
				    msg < pci_msi_maxmsgnum(sc->xsc_pi) ? msg : 0);
			else
				pci_lintr_assert(sc->xsc_pi);
		}
	}
}
''')

# INTx (docs/13 "休眠（S4）", 2026-10-03): stock bhyve's pci_xhci_deassert_interrupt() *asserts* the
# line when MSI is off, and nothing in pci_xhci.c ever deasserts it. The firmware's xHCI driver
# writes IMAN with IE = 0 at every start: from then on the controller's INTx pin was up until a
# guest enabled MSI (pci_lintr_update() then takes it down). bhyve puts slot 30's INTA on the
# I/O APIC pin of slot 6 ((4 + slot + pin) % 8), keel's e1000 slot when a guest has passthrough
# devices in 7 and 8: Windows resuming from hibernation unmasks that pin for the e1000 driver
# (still in D3, its ISR does not claim) before the xHCI's D0 enables MSI again, the vCPU the pin
# targets takes the interrupt after every EOI (~170k exits/s), and a resume hibernated soon after
# boot did not finish (0x9F, the AHCI's power IRP). Now the line follows the spec: up while
# USBCMD.INTE is set and an interrupter has IMAN.IP and IMAN.IE (keel's rule above: a secondary
# interrupter with IE off counts with interrupter 0's IE), down otherwise; re-evaluated after
# every IMAN and USBCMD write, and after an ERDP write that clears EHB: bhyve clears IMAN.IP there,
# and without it an event that came in while the guest's handler ran (consumed by the handler and
# acknowledged through ERDP) left the line up with nothing pending: FreeBSD guests with INTx took
# ~22,000 interrupts while booting instead of ~30 (upstream D60274, testplan "INTx 下中断次数比 MSI 多的原因").
rep('''static void
pci_xhci_deassert_interrupt(struct pci_xhci_softc *sc)
{

	if (!pci_msi_enabled(sc->xsc_pi))
		pci_lintr_assert(sc->xsc_pi);
}
''', '''static void
pci_xhci_deassert_interrupt(struct pci_xhci_softc *sc)
{
	struct pci_xhci_rtsregs *rts;
	uint32_t ie0;
	int i, on;

	/* keel: the INTx line follows INTE and the interrupters' IP and IE (patch-xhci.py) */
	on = 0;
	ie0 = sc->rtsregs.intrreg.iman & XHCI_IMAN_INTR_ENA;
	if (sc->opregs.usbcmd & XHCI_CMD_INTE) {
		for (i = 0; i < sc->keel_nintr && i < KEEL_XHCI_MAXINTR; i++) {
			rts = keel_xhci_rts(sc, i);
			if ((rts->intrreg.iman & XHCI_IMAN_INTR_PEND) &&
			    ((rts->intrreg.iman & XHCI_IMAN_INTR_ENA) || (i > 0 && ie0)))
				on = 1;
		}
	}
	if (!on)
		pci_lintr_deassert(sc->xsc_pi);
	else if (!pci_msi_enabled(sc->xsc_pi))
		pci_lintr_assert(sc->xsc_pi);
}
''')
rep('''		if (!(value & XHCI_IMAN_INTR_ENA))
			pci_xhci_deassert_interrupt(sc);
''', '''		pci_xhci_deassert_interrupt(sc);	/* keel: IP cleared or IE changed */
''')
rep('''		sc->opregs.usbcmd = pci_xhci_usbcmd_write(sc, value & 0x3F0F);
''', '''		sc->opregs.usbcmd = pci_xhci_usbcmd_write(sc, value & 0x3F0F);
		pci_xhci_deassert_interrupt(sc);	/* keel: INTE or a reset */
''')
rep('''			rts->intrreg.iman &= ~XHCI_IMAN_INTR_PEND;
		}

		rts->er_deq_seg = XHCI_ERDP_LO_SINDEX(value);
''', '''			rts->intrreg.iman &= ~XHCI_IMAN_INTR_PEND;
			pci_xhci_deassert_interrupt(sc);	/* keel: IP cleared */
		}

		rts->er_deq_seg = XHCI_ERDP_LO_SINDEX(value);
''')

# insert_event: into the ring of sc->keel_target (then back to 0)
rep('''	err = XHCI_TRB_ERROR_SUCCESS;

	rts = &sc->rtsregs;

	erdp = rts->intrreg.erdp & ~0xF;
''', '''	err = XHCI_TRB_ERROR_SUCCESS;

	/* keel: the interrupter the event is for, if its ring is set up */
	{
		int t = sc->keel_target;

		sc->keel_target = 0;
		if (t <= 0 || t >= sc->keel_nintr || sc->keel_rts[t].erstba_p == NULL)
			t = 0;
		rts = keel_xhci_rts(sc, t);
		sc->keel_pend |= 1u << t;
	}

	erdp = rts->intrreg.erdp & ~0xF;
''')

# transfer events: the TRB's interrupter target
rep('''		*do_intr = 1;

		err = pci_xhci_insert_event(sc, &evtrb, 0);
''', '''		*do_intr = 1;

		sc->keel_target = KEEL_TRB_2_IRQ_GET(trb->dwTrb2);	/* keel */
		err = pci_xhci_insert_event(sc, &evtrb, 0);
''')

# reset: every interrupter's ring
rep('''	sc->rtsregs.er_enq_idx = 0;
	sc->rtsregs.er_events_cnt = 0;
	sc->rtsregs.event_pcs = 1;
''', '''	sc->rtsregs.er_enq_idx = 0;
	sc->rtsregs.er_events_cnt = 0;
	sc->rtsregs.event_pcs = 1;
	for (i = 1; i < KEEL_XHCI_MAXINTR; i++) {	/* keel */
		sc->keel_rts[i].er_enq_idx = 0;
		sc->keel_rts[i].er_events_cnt = 0;
		sc->keel_rts[i].event_pcs = 1;
	}
	sc->keel_target = 0;
	sc->keel_pend = 0;
''')

# runtime register writes: interrupter offset / 32
rep('''	offset -= 0x20;		/* start of intrreg */

	rts = &sc->rtsregs;
''', '''	offset -= 0x20;		/* start of intrreg */

	/* keel: interrupter offset / 32 */
	if (offset / 32 >= (uint64_t)sc->keel_nintr)
		return;
	rts = keel_xhci_rts(sc, offset / 32);
	offset %= 32;
''')
rep('''		rts->intrreg.erstba = MASK_64_HI(sc->rtsregs.intrreg.erstba) |''',
    '''		rts->intrreg.erstba = MASK_64_HI(rts->intrreg.erstba) |''')
rep('''		    MASK_64_LO(sc->rtsregs.intrreg.erstba);''',
    '''		    MASK_64_LO(rts->intrreg.erstba);''')
rep('''		    sc->rtsregs.intrreg.erstba & ~0x3FUL);''',
    '''		    rts->intrreg.erstba & ~0x3FUL);''')
rep('''		    MASK_64_HI(sc->rtsregs.intrreg.erdp) |''',
    '''		    MASK_64_HI(rts->intrreg.erdp) |''')
rep('''		    MASK_64_LO(sc->rtsregs.intrreg.erdp);''',
    '''		    MASK_64_LO(rts->intrreg.erdp);''')

# runtime register reads
rep('''		offset -= 0x20;
		item = offset % 32;

		assert(offset < sizeof(sc->rtsregs.intrreg));

		p = &sc->rtsregs.intrreg.iman;
''', '''		offset -= 0x20;
		item = offset % 32;

		/* keel: interrupter offset / 32 */
		if (offset / 32 >= (uint64_t)sc->keel_nintr)
			return (0);
		p = &keel_xhci_rts(sc, offset / 32)->intrreg.iman;
''')

# control transfers: macOS sets IOC on every stage of a control TD (SETUP, DATA and STATUS).
# bhyve ran the request at the first IOC, i.e. with the SETUP stage alone (no data buffer),
# then the DATA stage alone (no request): SET_CONFIGURATION timed out (AppleUSBHostComposite-
# Device: "unable to set a configuration (0xe00002ed)"). Collect the TD up to its STATUS
# stage; xfer_complete() still posts an event for each TRB that has IOC.
rep('''		/* handle current batch that requires interrupt on complete */
		if (trb3 & XHCI_TRB_3_IOC_BIT) {
''', '''		/* handle current batch that requires interrupt on complete */
		if ((trb3 & XHCI_TRB_3_IOC_BIT) && epid == 1 &&
		    setup_trb != NULL && type != XHCI_TRB_TYPE_STATUS_STAGE)
			continue;	/* keel: the rest of the control TD first */
		if (trb3 & XHCI_TRB_3_IOC_BIT) {
''')

# transfer rings: bhyve followed a LINK TRB whatever its cycle bit. macOS gives the control
# endpoint ring segments of 4 TRBs, the last one a LINK it hands over (cycle bit) only when the
# ring wraps; before that the LINK is zero. bhyve followed it to guest address 0 and wrote the
# endpoint's dequeue pointer as 0x1: every later control transfer read garbage and timed out
# (AppleUSBHostCompositeDevice: "unable to set a configuration"). With keel.xhci_intrs > 1
# (the macOS profile) a LINK TRB is only followed when its cycle bit is the consumer's.
rep('''		if (type != XHCI_TRB_TYPE_LINK &&
		    (trb3 & XHCI_TRB_3_CYCLE_BIT) !=
		    (ccs & XHCI_TRB_3_CYCLE_BIT)) {
''', '''		if ((type != XHCI_TRB_TYPE_LINK ||
		    sc->keel_nintr > 1) &&	/* keel: a LINK has a cycle bit too */
		    (trb3 & XHCI_TRB_3_CYCLE_BIT) !=
		    (ccs & XHCI_TRB_3_CYCLE_BIT)) {
''')

# endpoint (re)initialisation: bhyve kept the TDs it had collected from the endpoint's old
# ring. UEFI's USB keyboard driver leaves an interrupt IN transfer pending on the keyboard;
# after macOS readdressed and reconfigured the device, the first key press completed that stale
# transfer (an event for a TRB in the firmware's ring, which macOS ignores) and moved the
# endpoint's dequeue pointer back into the firmware's ring: no key ever reached macOS. With
# keel.xhci_intrs > 1 (the macOS profile) a new ring drops what was pending on the old one.
rep('''	if (devep->ep_xfer == NULL) {
		devep->ep_xfer = malloc(sizeof(struct usb_data_xfer));
		USB_DATA_XFER_INIT(devep->ep_xfer);
	}
''', '''	if (devep->ep_xfer == NULL) {
		devep->ep_xfer = malloc(sizeof(struct usb_data_xfer));
		USB_DATA_XFER_INIT(devep->ep_xfer);
	} else if (dev->xsc->keel_nintr > 1) {	/* keel: a new ring, no stale TDs */
		USB_DATA_XFER_LOCK(devep->ep_xfer);
		USB_DATA_XFER_RESET(devep->ep_xfer);
		USB_DATA_XFER_UNLOCK(devep->ep_xfer);
	}
''')

rep('''	sc->caplength = XHCI_SET_CAPLEN(XHCI_CAPLEN) |
''', '''	/* keel: -o keel.xhci_intrs=N: N interrupters (macOS needs 3) */
	{
		const char *v = get_config_value("keel.xhci_intrs");

		sc->keel_nintr = v != NULL ? (int)strtol(v, NULL, 0) : 1;
		/* keel: bhyve's own xHCI debug output (to bhyve.log) */
		if (get_config_bool_default("keel.xhci_debug", false) ||
		    access("/var/run/keel-xhci-debug", F_OK) == 0) {
			xhci_debug = 1;
			setvbuf(stdout, NULL, _IOLBF, 0);	/* whole lines into bhyve.log */
		}
		if (sc->keel_nintr < 1 || sc->keel_nintr > KEEL_XHCI_MAXINTR ||
		    (sc->keel_nintr & (sc->keel_nintr - 1)) != 0) {	/* MSI: 2^k */
			EPRINTLN("keel.xhci_intrs: 1, 2, 4 or 8");
			sc->keel_nintr = 1;
		}
	}

	sc->caplength = XHCI_SET_CAPLEN(XHCI_CAPLEN) |
''')
rep('''	                 XHCI_SET_HCSP1_MAXINTR(1) |	/* interrupters */
''', '''	                 XHCI_SET_HCSP1_MAXINTR(sc->keel_nintr) |	/* keel */
''')
rep('''	sc->regsend = sc->rtsoff + 0x20 + 32;		/* only 1 intrpter */
''', '''	sc->regsend = sc->rtsoff + 0x20 + 32 * sc->keel_nintr;	/* keel */
''')
rep('''	pci_emul_add_msicap(pi, 1);
''', '''	pci_emul_add_msicap(pi, sc->keel_nintr);	/* keel: one per interrupter */
''')

# port speed (docs/28, Tahoe). stable/15 (48266828f5) has the port's speed and USB version in
# dev->hci (hci_speed, hci_usbver: a device's probe may set them, else the emulation's
# ue_usbspeed/ue_usbver). No device sets them, so upstream's ports still say high speed for a USB
# 3 device: keel's SuperSpeed for N > 1 stays, now computed from those fields.
rep('''XHCI_PS_SPEED_SET(dev->hci.hci_speed)''',
    '''XHCI_PS_SPEED_SET(KEEL_XHCI_SPEED(sc, dev))''', 3)

# checkpoint format (docs/34): stable/15 added hci_speed and hci_usbver in the middle of every
# device's part of the xhci section. That would misread a 14.5 checkpoint (xhci version 0). Both
# are set again from the configuration when bhyve starts (pci_xhci_parse_devices(), before a
# restore), so they stay out of the checkpoint: the format is 14.5's.
rep('''		SNAPSHOT_VAR_OR_LEAVE(dev->hci.hci_port, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(dev->hci.hci_speed, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(dev->hci.hci_usbver, meta, ret, done);
''', '''		SNAPSHOT_VAR_OR_LEAVE(dev->hci.hci_port, meta, ret, done);
		/* keel: hci_speed, hci_usbver not in the checkpoint (patch-xhci.py) */
''')

rep('''		port->portsc &= ~(XHCI_PS_PLS_MASK | XHCI_PS_PR | XHCI_PS_PRC);
''', '''		port->portsc &= ~(XHCI_PS_PLS_MASK | XHCI_PS_PR | XHCI_PS_PRC);
		port->portsc &= ~XHCI_PS_SPEED_SET(0xF);	/* keel: set below, not or-ed in */
''')

# the ports are set up before keel.xhci_intrs is read into the softc: ask the configuration
rep('''static void
pci_xhci_init_port(struct pci_xhci_softc *sc, int portn)
{
''', '''static int
keel_xhci_speed(int usbver, int usbspeed)
{
	static int n = -1;
	const char *v;

	if (n < 0) {
		v = get_config_value("keel.xhci_intrs");
		n = v != NULL ? (int)strtol(v, NULL, 0) : 1;
	}
	return (n > 1 && usbver == 3 ? 4 : usbspeed);
}

static void
pci_xhci_init_port(struct pci_xhci_softc *sc, int portn)
{
''')

# snapshot: interrupters 1..N-1 (docs/28 X4)
rep('''	SNAPSHOT_VAR_OR_LEAVE(sc->rtsregs.er_events_cnt, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->rtsregs.event_pcs, meta, ret, done);
''', '''	SNAPSHOT_VAR_OR_LEAVE(sc->rtsregs.er_events_cnt, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->rtsregs.event_pcs, meta, ret, done);
	if (meta->op == VM_SNAPSHOT_RESTORE &&
	    keel_xhci_er_restore(sc, &sc->rtsregs) != 0) {	/* keel: SA-26:72's er_size */
		ret = EINVAL;
		goto done;
	}
''')
rep('''	SNAPSHOT_VAR_OR_LEAVE(sc->usb2_port_start, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->usb3_port_start, meta, ret, done);

done:
''', '''	SNAPSHOT_VAR_OR_LEAVE(sc->usb2_port_start, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(sc->usb3_port_start, meta, ret, done);

	if (sc->keel_nintr > 1) {	/* keel */
		struct pci_xhci_rtsregs *rts;
		int n;

		n = sc->keel_nintr;
		SNAPSHOT_VAR_OR_LEAVE(n, meta, ret, done);
		if (n != sc->keel_nintr) {
			EPRINTLN("%s: %d interrupters in the checkpoint, %d configured",
			    __func__, n, sc->keel_nintr);
			ret = EINVAL;
			goto done;
		}
		SNAPSHOT_VAR_OR_LEAVE(sc->keel_pend, meta, ret, done);
		SNAPSHOT_VAR_OR_LEAVE(sc->keel_target, meta, ret, done);
		for (i = 1; i < n; i++) {
			rts = &sc->keel_rts[i];
			SNAPSHOT_VAR_OR_LEAVE(rts->intrreg.iman, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->intrreg.imod, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->intrreg.erstsz, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->intrreg.rsvd, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->intrreg.erstba, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->intrreg.erdp, meta, ret, done);
			SNAPSHOT_GUEST2HOST_ADDR_OR_LEAVE(pi->pi_vmctx, rts->erstba_p,
			    XHCI_GADDR_SIZE(rts->erstba_p), true, meta, ret, done);
			SNAPSHOT_GUEST2HOST_ADDR_OR_LEAVE(pi->pi_vmctx, rts->erst_p,
			    XHCI_GADDR_SIZE(rts->erst_p), true, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->er_deq_seg, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->er_enq_idx, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->er_enq_seg, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->er_events_cnt, meta, ret, done);
			SNAPSHOT_VAR_OR_LEAVE(rts->event_pcs, meta, ret, done);
			if (meta->op == VM_SNAPSHOT_RESTORE &&
			    keel_xhci_er_restore(sc, rts) != 0) {
				ret = EINVAL;
				goto done;
			}
		}
	}

done:
''')

# restore: a device context that is no longer one (see the docstring, docs/09)
rep('''	if (pstreams > 0) {
		DPRINTF(("init_ep %d with pstreams %u", epid, pstreams));
		assert(devep->ep_sctx_trbs == NULL);

		devep->ep_sctx = XHCI_GADDR(dev->xsc, ep_ctx->qwEpCtx2 &
		                            XHCI_EPCTX_2_TR_DQ_PTR_MASK);
''', '''	if (pstreams > 0) {	/* keel: the whole stream array must be guest memory */
		devep->ep_sctx = paddr_guest2host(dev->xsc->xsc_pi->pi_vmctx,
		    ep_ctx->qwEpCtx2 & XHCI_EPCTX_2_TR_DQ_PTR_MASK,
		    pstreams * sizeof(struct xhci_stream_ctx));
		if (devep->ep_sctx == NULL) {
			EPRINTLN("keel: xhci: endpoint %d: %u streams at 0x%lx, not guest "
			    "memory: no streams", epid, pstreams,
			    (unsigned long)(ep_ctx->qwEpCtx2 & XHCI_EPCTX_2_TR_DQ_PTR_MASK));
			pstreams = 0;
		}
	}
	if (pstreams > 0) {
		DPRINTF(("init_ep %d with pstreams %u", epid, pstreams));
		assert(devep->ep_sctx_trbs == NULL);

''')

rep('''	if (meta->op == VM_SNAPSHOT_RESTORE) {
		pci_xhci_init_ep(dev, idx);
		xfer = dev->eps[idx].ep_xfer;
	}
''', '''	if (meta->op == VM_SNAPSHOT_RESTORE) {	/* keel: or read into a scratch one */
		if (keel_xhci_drop_slot || XHCI_EPCTX_0_EPSTATE_GET(
		    dev->dev_ctx->ctx_ep[idx].dwEpCtx0) == XHCI_ST_EPCTX_DISABLED) {
			if (!keel_xhci_drop_slot)
				EPRINTLN("keel: xhci: endpoint %d is disabled in the guest's "
				    "context: dropped", idx);
			memset(&keel_xfer_scratch, 0, sizeof(keel_xfer_scratch));
			xfer = &keel_xfer_scratch;
		} else {
			pci_xhci_init_ep(dev, idx);
			xfer = dev->eps[idx].ep_xfer;
		}
	}
''')
rep('''	SNAPSHOT_VAR_OR_LEAVE(xfer->ndata, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(xfer->head, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(xfer->tail, meta, ret, done);

done:
''', '''	SNAPSHOT_VAR_OR_LEAVE(xfer->ndata, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(xfer->head, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(xfer->tail, meta, ret, done);

done:
	if (xfer == &keel_xfer_scratch) {	/* keel */
		free(keel_xfer_scratch.ureq);
		keel_xfer_scratch.ureq = NULL;
	}
''')
rep('''static int
pci_xhci_snapshot_ep(''', '''/*
 * keel: a restore drops a slot whose device context is not one any more: the slot context must
 * name this slot and be in the default, addressed or configured state (bhyve writes both at
 * Address Device). Garbage passes that about once in 4000; an endpoint must also not be
 * disabled, and pci_xhci_init_ep() checks the stream array.
 */
static bool keel_xhci_drop_slot;
static struct usb_data_xfer keel_xfer_scratch;

static bool
keel_xhci_ctx_ok(struct xhci_dev_ctx *ctx, int slot)
{
	uint32_t s3 = ctx->ctx_slot.dwSctx3;
	uint32_t st = XHCI_SCTX_3_SLOT_STATE_GET(s3);

	return (XHCI_SCTX_3_DEV_ADDR_GET(s3) == (uint32_t)slot &&
	    (st == XHCI_ST_SLCTX_DEFAULT || st == XHCI_ST_SLCTX_ADDRESSED ||
	    st == XHCI_ST_SLCTX_CONFIGURED));
}

static int
pci_xhci_snapshot_ep(''')
rep('''		SNAPSHOT_GUEST2HOST_ADDR_OR_LEAVE(pi->pi_vmctx, dev->dev_ctx,
			XHCI_GADDR_SIZE(dev->dev_ctx), true, meta, ret, done);
''', '''		SNAPSHOT_GUEST2HOST_ADDR_OR_LEAVE(pi->pi_vmctx, dev->dev_ctx,
			XHCI_GADDR_SIZE(dev->dev_ctx), true, meta, ret, done);
		keel_xhci_drop_slot = meta->op == VM_SNAPSHOT_RESTORE &&
		    dev->dev_ctx != NULL && !keel_xhci_ctx_ok(dev->dev_ctx, i);
		if (keel_xhci_drop_slot)	/* keel */
			EPRINTLN("keel: xhci: slot %d: its device context is not one any more "
			    "(slot word 0x%08x; the firmware's, given up): the slot is dropped", i,
			    dev->dev_ctx->ctx_slot.dwSctx3);
''')
rep('''		SNAPSHOT_VAR_OR_LEAVE(dev->dev_slotstate, meta, ret, done);
''', '''		SNAPSHOT_VAR_OR_LEAVE(dev->dev_slotstate, meta, ret, done);
		if (keel_xhci_drop_slot) {	/* keel: as after a Disable Slot */
			dev->dev_ctx = NULL;
			dev->dev_slotstate = XHCI_ST_DISABLED;
			keel_xhci_drop_slot = false;
		}
''')

# ---- fixes of 2026-10-07 (docs/70 "新 vmm.ko、bhyve、keel 之后的回归", xHCI 事件环) ----
# A Windows 10 guest's xHCI died after fast window drags over RDP: Windows reset the controller
# again and again (PnP sounds, Device Manager flickering) and gave up (yellow mark, no root hub,
# the tablet gone, PS/2 mouse left). Root cause: the tablet's reports come from the VNC/RDP input
# thread through hci_intr (pci_xhci_dev_intr()), which ran the endpoint and wrote the event ring
# without the controller's mutex, while vCPU threads (MMIO, holding it) inserted command
# completion and transfer events and recounted the ring on ERDP writes. Two writers on one ring
# lose events or leave a slot with a stale cycle bit (the guest stops there), and the event count
# drifts until bhyve posts Event Ring Full. Fixed: hci_intr takes the mutex; with it the EHB rule
# of xHCI 4.17.2 (upstream D60413), HCRST clearing the interrupters, and the ring's fill count
# kept right (the last free slot was used without counting it; the Event Ring Full Error was
# written without the cycle toggle at a wrap).

# hci_intr from the device's thread: under the controller's mutex like every MMIO access.
# Order: the device's ev_mtx -> sc->mtx -> the endpoint's xfer lock -> the device's mtx, the
# same as a vCPU's doorbell (sc->mtx -> xfer -> device); nothing calls hci_intr holding sc->mtx.
rep('''static int
pci_xhci_dev_intr(struct usb_hci *hci, int epctx)
{
''', '''static int
keel_xhci_dev_intr_locked(struct usb_hci *hci, int epctx)
{
''')
# FreeBSD-SA-26:72 takes sc->mtx in this function the same way; here the wrapper below holds it
# (a second lock of the non-recursive mutex would hang the device's thread)
rep('''	sc = dev->xsc;
	pthread_mutex_lock(&sc->mtx);
''', '''	sc = dev->xsc;
''')
rep('''done:
	pthread_mutex_unlock(&sc->mtx);
	return (error);
}''', '''done:
	return (error);
}''')
rep('''static int
pci_xhci_dev_event(struct usb_hci *hci, enum hci_usbev evid __unused,
''', '''/* keel: a device's interrupt (its own thread) runs under the controller's mutex */
static int
pci_xhci_dev_intr(struct usb_hci *hci, int epctx)
{
	struct pci_xhci_softc *sc;
	int error;

	sc = ((struct pci_xhci_dev_emu *)hci->hci_sc)->xsc;
	pthread_mutex_lock(&sc->mtx);
	error = keel_xhci_dev_intr_locked(hci, epctx);
	pthread_mutex_unlock(&sc->mtx);
	return (error);
}

static int
pci_xhci_dev_event(struct usb_hci *hci, enum hci_usbev evid __unused,
''')

# EHB (xHCI 4.17.2, as upstream D60413 and QEMU): while an interrupter's Event Handler Busy is
# set its handler is still on the ring and will find the new event; no new interrupt, IMAN and
# USBSTS untouched. A secondary interrupter with IE off (macOS drains it from interrupter 0's
# handler) keeps ringing on interrupter 0's vector as before.
rep('''	pend = sc->keel_pend != 0 ? sc->keel_pend : 1;
	sc->keel_pend = 0;
	sc->opregs.usbsts |= XHCI_STS_EINT;
	for (i = 0; i < KEEL_XHCI_MAXINTR; i++) {
		if ((pend & (1u << i)) == 0)
			continue;
		rts = keel_xhci_rts(sc, i);
		rts->intrreg.erdp |= XHCI_ERDP_LO_BUSY;
''', '''	pend = sc->keel_pend != 0 ? sc->keel_pend : 1;
	sc->keel_pend = 0;
	for (i = 0; i < KEEL_XHCI_MAXINTR; i++) {
		if ((pend & (1u << i)) == 0)
			continue;
		rts = keel_xhci_rts(sc, i);
		if ((rts->intrreg.erdp & XHCI_ERDP_LO_BUSY) &&
		    (i == 0 || (rts->intrreg.iman & XHCI_IMAN_INTR_ENA))) {
			sc->keel_held++;	/* keel: EHB, xHCI 4.17.2 */
			continue;
		}
		sc->opregs.usbsts |= XHCI_STS_EINT;
		/* keel: EHB only with an interrupt signalled (IE and INTE on); a secondary
		   interrupter with IE off as before */
		if (i > 0 && (rts->intrreg.iman & XHCI_IMAN_INTR_ENA) == 0)
			rts->intrreg.erdp |= XHCI_ERDP_LO_BUSY;
		else if ((sc->opregs.usbcmd & XHCI_CMD_INTE) &&
		    (rts->intrreg.iman & XHCI_IMAN_INTR_ENA))
			rts->intrreg.erdp |= XHCI_ERDP_LO_BUSY;
''')

# An interrupter whose IP is pending interrupts when its IE or USBCMD.INTE is turned on (xHCI
# 4.17.2: the interrupt is asserted while IP, IE and INTE are all 1; QEMU's xhci_intr_update()).
# INTx follows that level already (pci_xhci_deassert_interrupt()); an MSI was only ever sent at
# an event. Windows starts the controller with one USBCMD write of RS and INTE: the port change
# events of the run are posted inside pci_xhci_usbcmd_write() before INTE is stored, so they set
# IP and EHB and sent nothing. Without the EHB rule the next event's MSI covered that up; with it
# every later event was held back by that EHB and Windows reset the controller every 5 s (its
# Enable Slot never completed). Now the pending interrupt goes out when INTE or IE comes on.
# EHB is set only when an interrupt is signalled (IE and INTE on): macOS programs IMAN before
# HCRST (which clears it) and turns IE on again only after the run, with a write that also clears
# IP. The run's port change events had set IP and EHB with IE off; the IMAN write cleared IP,
# EHB stayed, and the No Op command's completion was held back for good (no USB in macOS with
# 1a2d0c2f). An EHB nobody was interrupted for is not "the handler is busy".
rep('''	if (!on)
		pci_lintr_deassert(sc->xsc_pi);
	else if (!pci_msi_enabled(sc->xsc_pi))
		pci_lintr_assert(sc->xsc_pi);
}
''', '''	if (!on)
		pci_lintr_deassert(sc->xsc_pi);
	else if (!pci_msi_enabled(sc->xsc_pi))
		pci_lintr_assert(sc->xsc_pi);
}

/* keel: the MSI of a pending interrupter (only: its index, -1 all) once IE/INTE come on */
static void
keel_xhci_msi_pending(struct pci_xhci_softc *sc, int only)
{
	struct pci_xhci_rtsregs *rts;
	int i;

	if ((sc->opregs.usbcmd & XHCI_CMD_INTE) == 0 || !pci_msi_enabled(sc->xsc_pi))
		return;
	for (i = 0; i < sc->keel_nintr && i < KEEL_XHCI_MAXINTR; i++) {
		if (only >= 0 && i != only)
			continue;
		rts = keel_xhci_rts(sc, i);
		if ((rts->intrreg.iman & XHCI_IMAN_INTR_PEND) &&
		    (rts->intrreg.iman & XHCI_IMAN_INTR_ENA)) {
			sc->keel_late++;
			rts->intrreg.erdp |= XHCI_ERDP_LO_BUSY;
			pci_generate_msi(sc->xsc_pi,
			    i < pci_msi_maxmsgnum(sc->xsc_pi) ? i : 0);
		}
	}
}
''')
rep('''	case 0x00:
		if (value & XHCI_IMAN_INTR_PEND)
			rts->intrreg.iman &= ~XHCI_IMAN_INTR_PEND;
		rts->intrreg.iman = (value & XHCI_IMAN_INTR_ENA) |
		                    (rts->intrreg.iman & XHCI_IMAN_INTR_PEND);

		pci_xhci_deassert_interrupt(sc);	/* keel: IP cleared or IE changed */
''', '''	case 0x00: {
		uint32_t ie0 = rts->intrreg.iman & XHCI_IMAN_INTR_ENA;	/* keel */

		if (value & XHCI_IMAN_INTR_PEND)
			rts->intrreg.iman &= ~XHCI_IMAN_INTR_PEND;
		rts->intrreg.iman = (value & XHCI_IMAN_INTR_ENA) |
		                    (rts->intrreg.iman & XHCI_IMAN_INTR_PEND);

		if (sc->keel_rtrace < 40 && (sc->keel_rtrace++, 1))
			KXT("IMAN[%d] write 0x%lx -> 0x%x (EHB %d, events %u)",
			    rts == &sc->rtsregs ? 0 : (int)(rts - sc->keel_rts), value,
			    rts->intrreg.iman, (int)(rts->intrreg.erdp & XHCI_ERDP_LO_BUSY) != 0,
			    rts->er_events_cnt);
		pci_xhci_deassert_interrupt(sc);	/* keel: IP cleared or IE changed */
		if (ie0 == 0 && (rts->intrreg.iman & XHCI_IMAN_INTR_ENA))	/* keel: IE on */
			keel_xhci_msi_pending(sc, rts == &sc->rtsregs ? 0 :
			    (int)(rts - sc->keel_rts));
	}
''')
# USBCMD: INTE takes effect before the run's port change events are posted (one write of RS and
# INTE, as Windows and macOS do, must interrupt for them like real hardware); a pending IP gets
# its MSI when INTE comes on.
rep('''static uint32_t
pci_xhci_usbcmd_write(struct pci_xhci_softc *sc, uint32_t cmd)
{''', '''static void keel_xhci_msi_pending(struct pci_xhci_softc *sc, int only);

static uint32_t
pci_xhci_usbcmd_write(struct pci_xhci_softc *sc, uint32_t cmd)
{''')
rep('''	if (cmd & XHCI_CMD_RS) {
		do_intr = (sc->opregs.usbcmd & XHCI_CMD_RS) == 0;

		sc->opregs.usbcmd |= XHCI_CMD_RS;''', '''	/* keel: INTE first, and what is pending interrupts now that it is on */
	if ((cmd & XHCI_CMD_INTE) && !(sc->opregs.usbcmd & XHCI_CMD_INTE)) {
		sc->opregs.usbcmd |= XHCI_CMD_INTE;
		keel_xhci_msi_pending(sc, -1);
	} else if (!(cmd & XHCI_CMD_INTE))
		sc->opregs.usbcmd &= ~XHCI_CMD_INTE;

	if (cmd & XHCI_CMD_RS) {
		do_intr = (sc->opregs.usbcmd & XHCI_CMD_RS) == 0;

		sc->opregs.usbcmd |= XHCI_CMD_RS;''')

# ERDP: recount the ring from the guest's dequeue pointer at both halves (bhyve splits the 8-byte
# write; EHB is in the low half), and when the guest clears EHB with events left on the ring,
# interrupt again (they came in while its handler ran and did not interrupt).
rep('''		if (value & XHCI_ERDP_LO_BUSY) {
			rts->intrreg.erdp &= ~XHCI_ERDP_LO_BUSY;
			rts->intrreg.iman &= ~XHCI_IMAN_INTR_PEND;
			pci_xhci_deassert_interrupt(sc);	/* keel: IP cleared */
		}

		rts->er_deq_seg = XHCI_ERDP_LO_SINDEX(value);
''', '''		rts->er_deq_seg = XHCI_ERDP_LO_SINDEX(value);
		keel_xhci_er_recount(sc, rts);
		if (sc->keel_rtrace < 40 && (sc->keel_rtrace++, 1))	/* keel: trace */
			KXT("ERDP[%d] low write 0x%lx (EHB was %d, IMAN 0x%x, events %u)",
			    rts == &sc->rtsregs ? 0 : (int)(rts - sc->keel_rts), value,
			    (int)(rts->intrreg.erdp & XHCI_ERDP_LO_BUSY) != 0,
			    rts->intrreg.iman, rts->er_events_cnt);

		if (value & XHCI_ERDP_LO_BUSY) {
			rts->intrreg.erdp &= ~XHCI_ERDP_LO_BUSY;
			rts->intrreg.iman &= ~XHCI_IMAN_INTR_PEND;
			pci_xhci_deassert_interrupt(sc);	/* keel: IP cleared */
			if (rts->er_events_cnt > 0) {	/* keel: xHCI 4.17.2 */
				sc->keel_rerun++;
				sc->keel_pend |= 1u << (rts == &sc->rtsregs ? 0 :
				    (int)(rts - sc->keel_rts));
				pci_xhci_assert_interrupt(sc);
			}
		}
''')
rep('''		rts->intrreg.erdp = (value << 32) |
		    MASK_64_LO(rts->intrreg.erdp);

		if (rts->er_events_cnt > 0) {
			void *trb;
			uint64_t erdp;
			int erdp_i;

			erdp = rts->intrreg.erdp & ~0xF;
			trb = XHCI_GADDR(sc, erdp);
			if ((uintptr_t)trb < (uintptr_t)rts->erst_p ||
			    (uintptr_t)trb >= (uintptr_t)rts->erst_p +
			    rts->er_size * sizeof(struct xhci_trb)) {
				DPRINTF(("pci_xhci: ERDP 0x%lx out of range",
				        erdp));
				break;
			}

			erdp_i = ((uintptr_t)trb - (uintptr_t)rts->erst_p) /
			    sizeof(struct xhci_trb);
			if (erdp_i <= rts->er_enq_idx)
				rts->er_events_cnt = rts->er_enq_idx - erdp_i;
			else
				rts->er_events_cnt =
				    rts->er_size - (erdp_i - rts->er_enq_idx);

			DPRINTF(("pci_xhci: erdp 0x%lx, events cnt %u",
			        erdp, rts->er_events_cnt));
		}
''', '''		rts->intrreg.erdp = (value << 32) |
		    MASK_64_LO(rts->intrreg.erdp);
		keel_xhci_er_recount(sc, rts);
''')
rep('''static void
pci_xhci_rtsregs_write(struct pci_xhci_softc *sc, uint64_t offset,
    uint64_t value)
{''', '''/*
 * keel: the events on an interrupter's ring, from the dequeue pointer the guest wrote to the
 * enqueue index (upstream D60413's pci_xhci_update_er_events_cnt()). A pointer outside the
 * segment leaves the count alone. enq == deq is an empty ring: insert_event() never fills the
 * last slot with anything but the Event Ring Full Error, which the guest consumes first.
 * The segment is the one mapped at the ERSTBA write (erst_p, er_size), not the guest's table,
 * which the guest can change at any time (FreeBSD-SA-26:72).
 */
static void
keel_xhci_er_recount(struct pci_xhci_softc *sc, struct pci_xhci_rtsregs *rts)
{
	uint64_t erdp;
	uintptr_t trb, base;
	uint32_t erdp_i;

	if (rts->er_events_cnt == 0 || rts->erstba_p == NULL || rts->erst_p == NULL)
		return;
	erdp = rts->intrreg.erdp & ~0xFUL;
	trb = (uintptr_t)XHCI_GADDR(sc, erdp);
	base = (uintptr_t)rts->erst_p;
	if (trb < base || trb >= base + rts->er_size * sizeof(struct xhci_trb))
		return;
	erdp_i = (trb - base) / sizeof(struct xhci_trb);
	if (erdp_i <= (uint32_t)rts->er_enq_idx)
		rts->er_events_cnt = rts->er_enq_idx - erdp_i;
	else
		rts->er_events_cnt = rts->er_size - (erdp_i - rts->er_enq_idx);
	DPRINTF(("pci_xhci: erdp 0x%lx, events cnt %u", erdp, rts->er_events_cnt));
}

/*
 * keel: an interrupter's ring after a restore. FreeBSD-SA-26:72 keeps the ring's size in
 * er_size from the ERSTBA write; the checkpoint does not carry it (that would change its format,
 * docs/34, docs/117), so it is worked out here with that write's checks. A segment that is no
 * longer mapped, or not where erst_p points, turns the ring off as a bad ERSTBA write does.
 * -1: the saved indexes do not fit the ring (the restore fails, the guest stays suspended).
 */
static int
keel_xhci_er_restore(struct pci_xhci_softc *sc, struct pci_xhci_rtsregs *rts)
{
	uint64_t evtap;

	rts->er_size = 0;
	if (rts->erstba_p == NULL)
		return (0);
	rts->er_size = atomic_load_32(&rts->erstba_p->dwEvrsTableSize) & 0xFFFFUL;
	if (rts->er_size > XHCI_EVR_MAX)
		rts->er_size = XHCI_EVR_MAX;
	evtap = atomic_load_64(&rts->erstba_p->qwEvrsTablePtr) & ~0x3FUL;
	if (rts->er_size == 0 || rts->erst_p == NULL ||
	    paddr_guest2host(sc->xsc_pi->pi_vmctx, evtap,
	    rts->er_size * sizeof(struct xhci_trb)) != (void *)rts->erst_p) {
		EPRINTLN("pci_xhci: restore: event ring at 0x%lx (%u TRBs) not mapped, off",
		    evtap, rts->er_size);
		rts->erstba_p = NULL;
		rts->er_size = 0;
		return (0);
	}
	if (rts->er_enq_idx < 0 || (uint32_t)rts->er_enq_idx >= rts->er_size ||
	    rts->er_events_cnt > rts->er_size) {
		EPRINTLN("pci_xhci: restore: event ring enq %d, %u events, %u TRBs",
		    rts->er_enq_idx, rts->er_events_cnt, rts->er_size);
		return (-1);
	}
	return (0);
}

static void
pci_xhci_rtsregs_write(struct pci_xhci_softc *sc, uint64_t offset,
    uint64_t value)
{''')

# HCRST returns the interrupters' registers to their defaults (EHB included): an EHB the firmware
# left set would hold back the OS driver's first interrupt now that EHB is honoured.
rep('''	sc->rtsregs.er_enq_idx = 0;
	sc->rtsregs.er_events_cnt = 0;
	sc->rtsregs.event_pcs = 1;
	for (i = 1; i < KEEL_XHCI_MAXINTR; i++) {	/* keel */
		sc->keel_rts[i].er_enq_idx = 0;
''', '''	memset(&sc->rtsregs.intrreg, 0, sizeof(sc->rtsregs.intrreg));	/* keel: HCRST */
	sc->keel_rtrace = 0;
	sc->rtsregs.er_enq_idx = 0;
	sc->rtsregs.er_events_cnt = 0;
	sc->rtsregs.event_pcs = 1;
	for (i = 1; i < KEEL_XHCI_MAXINTR; i++) {	/* keel */
		memset(&sc->keel_rts[i].intrreg, 0, sizeof(sc->keel_rts[i].intrreg));
		sc->keel_rts[i].er_enq_idx = 0;
''')

# the ring's fill count: one slot stays for the Event Ring Full Error (xHCI 4.9.4). Stock bhyve
# took the last free slot without counting it (the count then said one less than the ring held:
# the next event could overwrite one the guest had not read), decided "full" from a cycle bit
# that is never the producer's at the enqueue slot, and wrote the error event without toggling
# the cycle state when it wrapped.
rep('''	/* TODO: multi-segment table */
	if (rts->er_events_cnt >= rts->er_size) {
		DPRINTF(("pci_xhci[%d] cannot insert event; ring full",
		         __LINE__));
		err = XHCI_TRB_ERROR_EV_RING_FULL;
		goto done;
	}

	if (rts->er_events_cnt == rts->er_size - 1) {
		struct xhci_trb	errev;

		if ((evtrbptr->dwTrb3 & 0x1) == (rts->event_pcs & 0x1)) {

			DPRINTF(("pci_xhci[%d] insert evt err: ring full",
			         __LINE__));

			errev.qwTrb0 = 0;
			errev.dwTrb2 = XHCI_TRB_2_ERROR_SET(
			                    XHCI_TRB_ERROR_EV_RING_FULL);
			errev.dwTrb3 = XHCI_TRB_3_TYPE_SET(
			                    XHCI_TRB_EVENT_HOST_CTRL) |
			               rts->event_pcs;
			rts->er_events_cnt++;
			memcpy(&rts->erst_p[rts->er_enq_idx], &errev,
			       sizeof(struct xhci_trb));
			rts->er_enq_idx = (rts->er_enq_idx + 1) % rts->er_size;
			err = XHCI_TRB_ERROR_EV_RING_FULL;
			do_intr = 1;

			goto done;
		}
	} else {
		rts->er_events_cnt++;
	}
''', '''	/* TODO: multi-segment table */
	(void)evtrbptr;
	if (rts->er_events_cnt >= rts->er_size) {
		DPRINTF(("pci_xhci[%d] cannot insert event; ring full",
		         __LINE__));
		err = XHCI_TRB_ERROR_EV_RING_FULL;
		goto done;
	}

	if (rts->er_events_cnt == rts->er_size - 1) {
		/* keel: the last free slot: the Event Ring Full Error */
		struct xhci_trb	errev;

		DPRINTF(("pci_xhci[%d] insert evt err: ring full",
		         __LINE__));

		errev.qwTrb0 = 0;
		errev.dwTrb2 = XHCI_TRB_2_ERROR_SET(
		                    XHCI_TRB_ERROR_EV_RING_FULL);
		errev.dwTrb3 = XHCI_TRB_3_TYPE_SET(
		                    XHCI_TRB_EVENT_HOST_CTRL) |
		               rts->event_pcs;
		rts->er_events_cnt++;
		memcpy(&rts->erst_p[rts->er_enq_idx], &errev,
		       sizeof(struct xhci_trb));
		rts->er_enq_idx = (rts->er_enq_idx + 1) % rts->er_size;
		if (rts->er_enq_idx == 0)
			rts->event_pcs ^= 1;
		err = XHCI_TRB_ERROR_EV_RING_FULL;
		do_intr = 1;

		goto done;
	}
	rts->er_events_cnt++;
''')

# ---- trace: /var/run/keel-xhci-trace at bhyve's start (or -o keel.xhci_trace=true) ----
# One line per controller reset, run/stop, port reset, command, command ring stop/abort, event
# ring setup, failed transfer and Event Ring Full, and once a second while events flow a
# counter line (events, MSIs, interrupts held back by EHB, re-raised at an ERDP write, ring
# full, inserts that raced another thread, events from a device's own thread). UTC times, into
# bhyve.log. Much lighter than keel.xhci_debug (every TRB).
rep('''#include <pthread.h>
''', '''#include <pthread.h>
#include <stdarg.h>
#include <time.h>
''')
rep('''	struct pci_xhci_rtsregs keel_rts[KEEL_XHCI_MAXINTR]; /* keel: 1..n-1 */
''', '''	struct pci_xhci_rtsregs keel_rts[KEEL_XHCI_MAXINTR]; /* keel: 1..n-1 */
	/* keel: counters for the trace (patch-xhci.py) */
	int		keel_inins, keel_inmmio;
	uint64_t	keel_nev, keel_races, keel_full, keel_msis, keel_held;
	uint64_t	keel_rerun, keel_devev, keel_late;
	uint32_t	keel_maxcnt;
	int		keel_rtrace;	/* IMAN/ERDP writes traced since the last reset */
	time_t		keel_last;
''')
rep('''static void
pci_xhci_reset(struct pci_xhci_softc *sc)
{''', '''static int keel_xhci_trace;
static __thread int keel_in_devintr;

static void keel_xlog(const char *fmt, ...) __printflike(1, 2);
static void
keel_xlog(const char *fmt, ...)
{
	struct timespec ts;
	struct tm tm;
	char t[16];
	va_list ap;

	clock_gettime(CLOCK_REALTIME, &ts);
	gmtime_r(&ts.tv_sec, &tm);
	strftime(t, sizeof(t), "%H:%M:%S", &tm);
	fprintf(stderr, "%s.%03ldZ keel-xhci: ", t, ts.tv_nsec / 1000000);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\\n', stderr);
}
#define	KXT(...)	do { if (keel_xhci_trace) keel_xlog(__VA_ARGS__); } while (0)

static void
pci_xhci_reset(struct pci_xhci_softc *sc)
{''')
# controller reset, run/stop
rep('''	int do_intr = 0;
	int i;

''', '''	int do_intr = 0;
	int i;

	if (((cmd ^ sc->opregs.usbcmd) & XHCI_CMD_RS) || (cmd & XHCI_CMD_HCRST))
		KXT("USBCMD 0x%x (was 0x%x): %s%s", cmd, sc->opregs.usbcmd,
		    cmd & XHCI_CMD_HCRST ? "HCRST " : "",
		    cmd & XHCI_CMD_RS ? "run" : "stop");
''')
rep('''		if (value & (XHCI_PS_PR | XHCI_PS_WPR)) {
			pci_xhci_reset_port(sc, port, value & XHCI_PS_WPR);
''', '''		if (value & (XHCI_PS_PR | XHCI_PS_WPR)) {
			KXT("port %d: %sreset", port,
			    value & XHCI_PS_WPR ? "warm " : "");
			pci_xhci_reset_port(sc, port, value & XHCI_PS_WPR);
''')
rep('''		if (value & XHCI_PS_PED)
			DPRINTF(("Disable port %d request", port));
''', '''		if (value & XHCI_PS_PED) {
			DPRINTF(("Disable port %d request", port));
			KXT("port %d: disable", port);
		}
''')
# commands and the command ring
rep('''			DPRINTF(("pci_xhci: command 0x%x result: 0x%x",
			        type, cmderr));
''', '''			DPRINTF(("pci_xhci: command 0x%x result: 0x%x",
			        type, cmderr));
			KXT("command type %u slot %u ep %u: completion %u", type, slot,
			    XHCI_TRB_3_EP_GET(trb->dwTrb3), cmderr);
''')
rep('''	case XHCI_CRCR_LO:
		if (sc->opregs.crcr & XHCI_CRCR_LO_CRR) {''', '''	case XHCI_CRCR_LO:
		if (value & (XHCI_CRCR_LO_CS | XHCI_CRCR_LO_CA))
			KXT("CRCR low 0x%lx: %s%s (ring %s)", value,
			    value & XHCI_CRCR_LO_CS ? "stop " : "",
			    value & XHCI_CRCR_LO_CA ? "abort" : "",
			    sc->opregs.crcr & XHCI_CRCR_LO_CRR ? "running" : "idle");
		if (sc->opregs.crcr & XHCI_CRCR_LO_CRR) {''')
# event ring setup
rep('''		DPRINTF(("pci_xhci: wr erstba erst (%p) ptr 0x%lx, sz %u",
		        rts->erstba_p, evtap, rts->er_size));''', '''		KXT("event ring: ERSTSZ %u, segment 0 at 0x%lx with %u TRBs%s",
		    rts->intrreg.erstsz, (unsigned long)evtap, rts->er_size,
		    rts->erst_p == NULL ? " (not mapped: ring off)" : "");
		DPRINTF(("pci_xhci: wr erstba erst (%p) ptr 0x%lx, sz %u",
		        rts->erstba_p, evtap, rts->er_size));''')
# failed transfers
rep('''		*do_intr = 1;

		sc->keel_target = KEEL_TRB_2_IRQ_GET(trb->dwTrb2);	/* keel */
''', '''		*do_intr = 1;
		if (err != XHCI_TRB_ERROR_SUCCESS && err != XHCI_TRB_ERROR_SHORT_PKT)
			KXT("transfer slot %u ep %u: completion %d", slot, epid, err);

		sc->keel_target = KEEL_TRB_2_IRQ_GET(trb->dwTrb2);	/* keel */
''')
# MSIs
rep('''				pci_generate_msi(sc->xsc_pi,
				    msg < pci_msi_maxmsgnum(sc->xsc_pi) ? msg : 0);''', '''				sc->keel_msis++, pci_generate_msi(sc->xsc_pi,
				    msg < pci_msi_maxmsgnum(sc->xsc_pi) ? msg : 0);''')
# MMIO writes in progress (for the race count)
rep('''	pthread_mutex_lock(&sc->mtx);
	if (offset < XHCI_CAPLEN)	/* read only registers */''', '''	pthread_mutex_lock(&sc->mtx);
	__atomic_store_n(&sc->keel_inmmio, 1, __ATOMIC_SEQ_CST);	/* keel: trace */
	if (offset < XHCI_CAPLEN)	/* read only registers */''')
rep('''		WPRINTF(("pci_xhci: write invalid offset %ld", offset));

	pthread_mutex_unlock(&sc->mtx);''', '''		WPRINTF(("pci_xhci: write invalid offset %ld", offset));

	__atomic_store_n(&sc->keel_inmmio, 0, __ATOMIC_SEQ_CST);	/* keel: trace */
	pthread_mutex_unlock(&sc->mtx);''')
# a device's own thread
rep('''static int
pci_xhci_dev_event(struct usb_hci *hci, enum hci_usbev evid __unused,
''', '''/* keel: trace: marks the device thread's events */
static int
keel_xhci_dev_intr_t(struct usb_hci *hci, int epctx)
{
	int error;

	keel_in_devintr = 1;
	error = pci_xhci_dev_intr(hci, epctx);
	keel_in_devintr = 0;
	return (error);
}

static int
pci_xhci_dev_event(struct usb_hci *hci, enum hci_usbev evid __unused,
''')
rep('''hci.hci_intr = pci_xhci_dev_intr;''', '''hci.hci_intr = keel_xhci_dev_intr_t;''')
# insert_event: races, ring full, the counter line
rep('''	err = XHCI_TRB_ERROR_SUCCESS;

	/* keel: the interrupter the event is for, if its ring is set up */
''', '''	err = XHCI_TRB_ERROR_SUCCESS;

	/* keel: trace: another thread in here, or a vCPU in an MMIO write meanwhile */
	if (__atomic_fetch_add(&sc->keel_inins, 1, __ATOMIC_SEQ_CST) != 0 ||
	    (keel_in_devintr && __atomic_load_n(&sc->keel_inmmio, __ATOMIC_SEQ_CST))) {
		if (sc->keel_races++ < 20)
			KXT("insert_event: another thread is on the event ring");
	}
	sc->keel_nev++;
	if (keel_in_devintr)
		sc->keel_devev++;

	/* keel: the interrupter the event is for, if its ring is set up */
''')
rep('''done:
	if (do_intr)
		pci_xhci_assert_interrupt(sc);

	return (err);
}
''', '''done:
	if (do_intr)
		pci_xhci_assert_interrupt(sc);

	/* keel: trace */
	if (err == XHCI_TRB_ERROR_EV_RING_FULL && sc->keel_full++ < 20)
		KXT("event ring full: %u events of %u, enq %d, ERDP 0x%lx",
		    rts->er_events_cnt, rts->er_size, rts->er_enq_idx,
		    (unsigned long)rts->intrreg.erdp);
	if (rts->er_events_cnt > sc->keel_maxcnt)
		sc->keel_maxcnt = rts->er_events_cnt;
	if (keel_xhci_trace) {
		struct timespec now;

		clock_gettime(CLOCK_MONOTONIC, &now);
		if (now.tv_sec != sc->keel_last) {
			sc->keel_last = now.tv_sec;
			keel_xlog("events %ju (device thread %ju), MSIs %ju (+%ju at IE/INTE on), "
			    "held by EHB %ju, raised at ERDP %ju, ring full %ju, races %ju; "
			    "ring max %u of %u",
			    (uintmax_t)sc->keel_nev, (uintmax_t)sc->keel_devev,
			    (uintmax_t)sc->keel_msis, (uintmax_t)sc->keel_late,
			    (uintmax_t)sc->keel_held,
			    (uintmax_t)sc->keel_rerun, (uintmax_t)sc->keel_full,
			    (uintmax_t)sc->keel_races, sc->keel_maxcnt,
			    rts->er_size);
			sc->keel_maxcnt = 0;
		}
	}
	__atomic_fetch_sub(&sc->keel_inins, 1, __ATOMIC_SEQ_CST);

	return (err);
}
''')
rep('''		if (sc->keel_nintr < 1 || sc->keel_nintr > KEEL_XHCI_MAXINTR ||''', '''		/* keel: the light trace (patch-xhci.py) */
		if (get_config_bool_default("keel.xhci_trace", false) ||
		    access("/var/run/keel-xhci-trace", F_OK) == 0)
			keel_xhci_trace = 1;
		if (sc->keel_nintr < 1 || sc->keel_nintr > KEEL_XHCI_MAXINTR ||''')


# ---- keel: remote USB devices (usb_remote.c, docs/66 §4.3, phase 1) ----
# remote2/remote3 are empty ports: no CCS until the device's hci_event(USBDEV_ATTACH), then a
# Port Status Change; USBDEV_REMOVE disconnects. Their transfers take a TD path of their own
# (keel_xhci_rkick/rfinish): one TD per endpoint in flight, the dequeue pointer moves when it
# is finished, USB_PENDING leaves it open and the endpoint Running; hci_complete() finishes it
# with Success/Short Packet/Stall/Transaction Error events (the stock pci_xhci_xfer_complete()
# reports Success for everything, which a real device's driver cannot live with). Stop/Reset
# Endpoint, Disable Slot, Reset Device, HCRST, a port reset and endpoint reconfiguration cancel
# what is in flight (Stop Endpoint reports the TD Stopped). Enable Slot still hands out slots in
# configuration order; Address Device binds the slot to the device on the root port the input
# slot context names when that one or the slot's current device is remote (hot plug), bhyve's
# own devices keep their slots as before. Checkpoints: the format does not change (8 blocks per
# endpoint, the live ones first; a remote endpoint's TD is not kept); after a restore a remote
# port that had a device shows a disconnect (event once the guest runs again).
rep('''#include "usb_emul.h"
''', '''#include "usb_emul.h"
#include "usb_remote.h"	/* keel */
#include "mevent.h"	/* keel */
int keel_snap_version_max(struct vm_snapshot_meta *meta, const char *name, int want);	/* snapshot.c */
''')
rep('''	struct usb_data_xfer *ep_xfer;	/* transfer chain */
};
''', '''	struct usb_data_xfer *ep_xfer;	/* transfer chain */

	/* keel: remote devices: a TD in flight, the ring position after it */
	int		keel_busy;
	uint64_t	keel_end;
	uint32_t	keel_endccs;
};
''')
rep('''	struct usb_hci		hci;
};
''', '''	struct usb_hci		hci;

	int			keel_remote;	/* keel: an asynchronous (remote) device */
	int			keel_conn;	/* keel: ... with a device on its port */
};
''')
rep('''#define	KEEL_XHCI_SPEED(sc, dev)	keel_xhci_speed((dev)->hci.hci_usbver, \\
					    (dev)->hci.hci_speed)
static int keel_xhci_speed(int usbver, int usbspeed);
''', '''#define	KEEL_XHCI_SPEED(sc, dev)	((dev)->keel_remote ? keel_xhci_rpsi(dev) : \\
					    keel_xhci_speed((dev)->hci.hci_usbver, \\
					    (dev)->hci.hci_speed))
static int keel_xhci_speed(int usbver, int usbspeed);
static int keel_xhci_rpsi(struct pci_xhci_dev_emu *dev);
''')
rep('''	uint32_t	keel_pend;	/* keel: interrupters with new events */
''', '''	uint32_t	keel_pend;	/* keel: interrupters with new events */
	uint32_t	keel_rpend;	/* keel: remote ports: change event after a restore */
	struct mevent	*keel_rtev;
''')
rep('''static int pci_xhci_validate_slot(uint32_t slot);
''', '''static int pci_xhci_validate_slot(uint32_t slot);
/* keel: remote devices */
static void keel_xhci_rcancel(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev,
    uint32_t slot, int epid, int stopped);
static void keel_xhci_rcancel_all(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev);
static void keel_xhci_rbind(struct pci_xhci_softc *sc, uint32_t slot,
    struct xhci_slot_ctx *islot);
''')
# HCRST: what is in flight goes
rep('''	for (i = 1; i <= XHCI_MAX_SLOTS; i++) {
		pci_xhci_reset_slot(sc, i);
	}
}
''', '''	for (i = 1; sc->devices != NULL && i <= XHCI_MAX_DEVS; i++)	/* keel */
		if (XHCI_DEVINST_PTR(sc, i) != NULL && XHCI_DEVINST_PTR(sc, i)->keel_remote)
			keel_xhci_rcancel_all(sc, XHCI_DEVINST_PTR(sc, i));

	for (i = 1; i <= XHCI_MAX_SLOTS; i++) {
		pci_xhci_reset_slot(sc, i);
	}
}
''')
# run: an empty remote port is not connected
rep('''				if ((dev = XHCI_DEVINST_PTR(sc, i)) == NULL)
					continue;
''', '''				if ((dev = XHCI_DEVINST_PTR(sc, i)) == NULL ||
				    (dev->keel_remote && !dev->keel_conn))	/* keel */
					continue;
''')
rep('''		if (XHCI_DEVINST_PTR(sc, port))
			p->portsc |= XHCI_PS_CCS;
''', '''		if (XHCI_DEVINST_PTR(sc, port) &&
		    (!XHCI_DEVINST_PTR(sc, port)->keel_remote ||
		    XHCI_DEVINST_PTR(sc, port)->keel_conn))	/* keel */
			p->portsc |= XHCI_PS_CCS;
''')
rep('''	dev = XHCI_DEVINST_PTR(sc, portn);
	if (dev) {
		port->portsc &= ~(XHCI_PS_PLS_MASK | XHCI_PS_PR | XHCI_PS_PRC);
''', '''	dev = XHCI_DEVINST_PTR(sc, portn);
	if (dev != NULL && dev->keel_remote) {	/* keel: the source resets its device */
		if (!dev->keel_conn)
			dev = NULL;
		else {
			keel_xhci_rcancel_all(sc, dev);
			if (dev->dev_ue->ue_event != NULL)
				dev->dev_ue->ue_event(dev->dev_sc, KEEL_UE_PORT_RESET, 0);
		}
	}
	if (dev) {
		port->portsc &= ~(XHCI_PS_PLS_MASK | XHCI_PS_PR | XHCI_PS_PRC);
''')
rep('''	dev = XHCI_DEVINST_PTR(sc, portn);
	if (dev) {
		port->portsc = XHCI_PS_CCS |		/* connected */
''', '''	dev = XHCI_DEVINST_PTR(sc, portn);
	if (dev != NULL && dev->keel_remote && !dev->keel_conn)
		dev = NULL;	/* keel: an empty remote port */
	if (dev) {
		port->portsc = XHCI_PS_CCS |		/* connected */
''')
rep('''	devep = &dev->eps[epid];
	if (devep->ep_MaxPStreams > 0)
		free(devep->ep_sctx_trbs);
''', '''	devep = &dev->eps[epid];
	if (dev->keel_remote)	/* keel: before the transfer is freed */
		keel_xhci_rcancel(dev->xsc, dev, 0, epid, 0);
	if (devep->ep_MaxPStreams > 0)
		free(devep->ep_sctx_trbs);
''')
rep('''	if (devep->ep_xfer == NULL) {
		devep->ep_xfer = malloc(sizeof(struct usb_data_xfer));
''', '''	if (dev->keel_remote)	/* keel: a new ring, nothing in flight */
		keel_xhci_rcancel(dev->xsc, dev, 0, epid, 0);
	if (devep->ep_xfer == NULL) {
		devep->ep_xfer = malloc(sizeof(struct usb_data_xfer));
''')
rep('''		} else {
			dev->dev_slotstate = XHCI_ST_DISABLED;
			/* TODO: reset events and endpoints */
		}
''', '''		} else {
			dev->dev_slotstate = XHCI_ST_DISABLED;
			/* TODO: reset events and endpoints */
			if (dev->keel_remote)	/* keel */
				keel_xhci_rcancel_all(sc, dev);
		}
''')
rep('''		dev->dev_slotstate = XHCI_ST_DEFAULT;
''', '''		dev->dev_slotstate = XHCI_ST_DEFAULT;
		if (dev->keel_remote)	/* keel */
			keel_xhci_rcancel_all(sc, dev);
''')
rep('''	dev = XHCI_SLOTDEV_PTR(sc, slot);
	assert(dev != NULL);

	dev->hci.hci_address = slot;
	dev->dev_ctx = dev_ctx;
''', '''	keel_xhci_rbind(sc, slot, islot_ctx);	/* keel: the device on the port the guest names */
	dev = XHCI_SLOTDEV_PTR(sc, slot);
	if (dev == NULL) {	/* keel: was assert(dev != NULL), guest-triggerable */
		cmderr = XHCI_TRB_ERROR_SLOT_NOT_ON;
		goto done;
	}
	if (dev->keel_remote)
		keel_xhci_rcancel_all(sc, dev);

	dev->hci.hci_address = slot;
	dev->dev_ctx = dev_ctx;
''')
rep('''	devep = &dev->eps[epid];
	if (devep->ep_xfer != NULL)
		USB_DATA_XFER_RESET(devep->ep_xfer);
''', '''	devep = &dev->eps[epid];
	if (dev->keel_remote)	/* keel: in flight goes; Stop Endpoint reports it Stopped */
		keel_xhci_rcancel(sc, dev, slot, epid, type == XHCI_TRB_TYPE_STOP_EP);
	if (devep->ep_xfer != NULL)
		USB_DATA_XFER_RESET(devep->ep_xfer);
''')
rep('''	if (type == XHCI_TRB_TYPE_RESET_EP &&
	    (dev->dev_ue->ue_reset == NULL ||
''', '''	if (type == XHCI_TRB_TYPE_RESET_EP && dev->keel_remote) {
		/* keel: not a device reset; the source may reset its side of the pipe */
		if (dev->dev_ue->ue_event != NULL)
			dev->dev_ue->ue_event(dev->dev_sc, KEEL_UE_EP_RESET, epid == 1 ? 0 :
			    (int)((epid / 2) | (epid & 1 ? 0x80 : 0)));
	} else if (type == XHCI_TRB_TYPE_RESET_EP &&
	    (dev->dev_ue->ue_reset == NULL ||
''')

# the remote TD path
rep('''/*
 * Outstanding transfer still in progress (device NAK'd earlier) so retry
 * the transfer again to see if it succeeds.
 */
''', '''/*
 * keel: remote devices (usb_remote.c, docs/66 §4.3): their own TD path, see patch-xhci.py.
 */
static int
keel_xhci_rpsi(struct pci_xhci_dev_emu *dev)
{

	if (dev->hci.hci_usbver == 3)
		return (XHCI_PS_SPEED_SS);
	switch (dev->hci.hci_speed) {
	case USB_SPEED_LOW:
		return (XHCI_PS_SPEED_LOW);
	case USB_SPEED_FULL:
		return (XHCI_PS_SPEED_FULL);
	case USB_SPEED_SUPER:
		return (XHCI_PS_SPEED_SS);
	default:
		return (XHCI_PS_SPEED_HIGH);
	}
}

static uint32_t
keel_xhci_slot_of(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev)
{
	uint32_t i;

	for (i = 1; i <= XHCI_MAX_SLOTS; i++)
		if (XHCI_SLOTDEV_PTR(sc, i) == dev)
			return (i);
	return (0);
}

/* a Transfer Event for the TRB at trbaddr (ed: an Event Data TRB's, len = EDTLA) */
static int
keel_xhci_rev(struct pci_xhci_softc *sc, uint32_t slot, uint32_t epid, uint64_t trbaddr,
    struct xhci_trb *trb, uint32_t code, uint32_t len, int ed)
{
	struct xhci_trb ev;

	if (sc->rtsregs.erstba_p == NULL)
		return (0);
	ev.qwTrb0 = ed ? trb->qwTrb0 : trbaddr;
	ev.dwTrb2 = XHCI_TRB_2_ERROR_SET(code) | XHCI_TRB_2_REM_SET(len);
	ev.dwTrb3 = XHCI_TRB_3_TYPE_SET(XHCI_TRB_EVENT_TRANSFER) | XHCI_TRB_3_SLOT_SET(slot) |
	    XHCI_TRB_3_EP_SET(epid) | (ed ? XHCI_TRB_3_ED_BIT : 0);
	if (code != XHCI_TRB_ERROR_SUCCESS && code != XHCI_TRB_ERROR_SHORT_PKT)
		KXT("remote transfer slot %u ep %u: completion %u", slot, epid, code);
	sc->keel_target = KEEL_TRB_2_IRQ_GET(trb->dwTrb2);
	pci_xhci_insert_event(sc, &ev, 0);
	return (1);
}

/* the TD in devep->ep_xfer is answered (usberr): events, and the dequeue pointer behind it */
static void
keel_xhci_rfinish(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev, uint32_t slot,
    uint32_t epid, int usberr)
{
	struct pci_xhci_dev_ep *devep = &dev->eps[epid];
	struct usb_data_xfer *xfer = devep->ep_xfer;
	struct usb_data_xfer_block *b;
	struct xhci_dev_ctx *dev_ctx;
	struct xhci_endp_ctx *ep_ctx;
	struct xhci_trb *trb;
	uint32_t err, edtla, type;
	int i, idx, in, isshort, pend, nev, last;

	devep->keel_busy = 0;
	if (xfer == NULL)
		return;
	dev_ctx = slot != 0 && XHCI_SLOTDEV_PTR(sc, slot) == dev ?
	    pci_xhci_get_dev_ctx(sc, slot) : NULL;
	if (dev_ctx == NULL) {
		USB_DATA_XFER_RESET(xfer);
		return;
	}
	ep_ctx = &dev_ctx->ctx_ep[epid];
	in = epid == 1 ? (xfer->ureq != NULL && (xfer->ureq->bmRequestType & UT_READ) != 0) :
	    (int)(epid & 1);
	switch (usberr) {
	case USB_ERR_NORMAL_COMPLETION:
	case USB_ERR_SHORT_XFER:
		err = XHCI_TRB_ERROR_SUCCESS;
		break;
	case USB_ERR_STALLED:
		err = XHCI_TRB_ERROR_STALL;
		break;
	default:
		err = XHCI_TRB_ERROR_XACT;
		break;
	}
	edtla = 0;
	isshort = pend = nev = 0;
	for (i = 0, idx = xfer->head; i < xfer->ndata;
	    i++, idx = (idx + 1) % USB_MAX_XFER_BLOCKS) {
		b = &xfer->data[idx];
		last = i == xfer->ndata - 1;
		trb = XHCI_GADDR(sc, (uint64_t)b->hci_data);
		if (trb == NULL)
			continue;
		type = XHCI_TRB_3_TYPE_GET(trb->dwTrb3);
		if (err != XHCI_TRB_ERROR_SUCCESS) {
			/* on the first data TRB, else on the last TRB; the endpoint halts and
			   the guest moves the dequeue pointer (Set TR Dequeue Pointer) */
			if (b->buf == NULL && !last)
				continue;
			nev += keel_xhci_rev(sc, slot, epid, (uint64_t)b->hci_data, trb, err,
			    b->blen, 0);
			ep_ctx->dwEpCtx0 = (ep_ctx->dwEpCtx0 & ~0x7) | XHCI_ST_EPCTX_HALTED;
			ep_ctx->qwEpCtx2 = devep->ep_ringaddr | devep->ep_ccs;
			goto out;
		}
		edtla += b->bdone;
		if (type == XHCI_TRB_TYPE_EVENT_DATA) {
			if (trb->dwTrb3 & XHCI_TRB_3_IOC_BIT) {
				nev += keel_xhci_rev(sc, slot, epid, (uint64_t)b->hci_data, trb,
				    isshort ? XHCI_TRB_ERROR_SHORT_PKT : XHCI_TRB_ERROR_SUCCESS,
				    edtla, 1);
				pend = 0;
			}
			edtla = 0;
			continue;
		}
		if (isshort && type != XHCI_TRB_TYPE_STATUS_STAGE) {
			/* the rest of a short TD; its last TRB reports when the short one did not */
			if (pend && last && (trb->dwTrb3 & XHCI_TRB_3_IOC_BIT)) {
				nev += keel_xhci_rev(sc, slot, epid, (uint64_t)b->hci_data, trb,
				    XHCI_TRB_ERROR_SHORT_PKT, b->blen, 0);
				pend = 0;
			}
			continue;
		}
		if (in && b->buf != NULL && b->blen > 0 && type != XHCI_TRB_TYPE_STATUS_STAGE) {
			isshort = 1;
			if (trb->dwTrb3 & (XHCI_TRB_3_IOC_BIT | XHCI_TRB_3_ISP_BIT))
				nev += keel_xhci_rev(sc, slot, epid, (uint64_t)b->hci_data, trb,
				    XHCI_TRB_ERROR_SHORT_PKT, b->blen, 0);
			else
				pend = 1;
			continue;
		}
		if (trb->dwTrb3 & XHCI_TRB_3_IOC_BIT)
			nev += keel_xhci_rev(sc, slot, epid, (uint64_t)b->hci_data, trb,
			    XHCI_TRB_ERROR_SUCCESS, b->blen, 0);
	}
	pci_xhci_update_ep_ring(sc, dev, devep, ep_ctx, 0, devep->keel_end,
	    devep->keel_endccs);
out:
	USB_DATA_XFER_RESET(xfer);
	if (nev > 0)
		pci_xhci_assert_interrupt(sc);
}

/* the next TD of a remote device's endpoint to the device, if one is on the ring */
static void
keel_xhci_rkick(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev, uint32_t slot,
    uint32_t epid)
{
	struct pci_xhci_dev_ep *devep = &dev->eps[epid];
	struct usb_data_xfer *xfer;
	struct usb_data_xfer_block *b;
	struct xhci_dev_ctx *dev_ctx;
	struct xhci_endp_ctx *ep_ctx;
	struct xhci_trb *trb;
	uint64_t addr, val;
	uint32_t ccs, fl, type, st;
	int n, setup, status, end, err, rounds, ndata;

	ep_ctx = NULL;
	xfer = NULL;
	trb = NULL;
	addr = 0;
	for (rounds = 0; rounds < 64; rounds++) {
		if (devep->keel_busy || devep->ep_xfer == NULL || devep->ep_MaxPStreams != 0 ||
		    XHCI_SLOTDEV_PTR(sc, slot) != dev)
			return;
		dev_ctx = pci_xhci_get_dev_ctx(sc, slot);
		if (dev_ctx == NULL)
			return;
		ep_ctx = &dev_ctx->ctx_ep[epid];
		st = XHCI_EPCTX_0_EPSTATE_GET(ep_ctx->dwEpCtx0);
		if (st == XHCI_ST_EPCTX_DISABLED || st == XHCI_ST_EPCTX_HALTED ||
		    st == XHCI_ST_EPCTX_ERROR)
			return;
		ep_ctx->dwEpCtx0 = FIELD_REPLACE(ep_ctx->dwEpCtx0, XHCI_ST_EPCTX_RUNNING,
		    0x7, 0);
		xfer = devep->ep_xfer;
		USB_DATA_XFER_RESET(xfer);
		addr = devep->ep_ringaddr;
		ccs = devep->ep_ccs;
		trb = XHCI_GADDR(sc, addr);
		setup = status = end = ndata = 0;
		for (n = 0; trb != NULL && n < 2 * USB_MAX_XFER_BLOCKS; n++) {
			fl = trb->dwTrb3;
			if ((fl & XHCI_TRB_3_CYCLE_BIT) != (ccs & XHCI_TRB_3_CYCLE_BIT))
				break;
			type = XHCI_TRB_3_TYPE_GET(fl);
			if (type == XHCI_TRB_TYPE_LINK) {
				if (fl & XHCI_TRB_3_TC_BIT)
					ccs ^= 0x1;
				trb = pci_xhci_trb_next(sc, trb, type, &addr);
				continue;
			}
			b = NULL;
			switch (type) {
			case XHCI_TRB_TYPE_SETUP_STAGE:
				if (epid != 1 || xfer->ndata != 0 ||
				    (fl & XHCI_TRB_3_IDT_BIT) == 0 ||
				    XHCI_TRB_2_BYTES_GET(trb->dwTrb2) != 8)
					goto bad;
				val = trb->qwTrb0;
				if (xfer->ureq == NULL)
					xfer->ureq = malloc(sizeof(struct usb_device_request));
				memcpy(xfer->ureq, &val, sizeof(struct usb_device_request));
				b = usb_data_xfer_append(xfer, NULL, 0, (void *)addr, ccs);
				if (b != NULL)
					b->processed = 1;
				setup = 1;
				break;
			case XHCI_TRB_TYPE_DATA_STAGE:
			case XHCI_TRB_TYPE_NORMAL:
				if ((type == XHCI_TRB_TYPE_DATA_STAGE) != (epid == 1) ||
				    (epid == 1 && !setup))
					goto bad;
				b = usb_data_xfer_append(xfer,
				    (fl & XHCI_TRB_3_IDT_BIT) ? (void *)&trb->qwTrb0 :
				    XHCI_GADDR(sc, trb->qwTrb0), trb->dwTrb2 & 0x1FFFF,
				    (void *)addr, ccs);
				ndata++;
				break;
			case XHCI_TRB_TYPE_STATUS_STAGE:
				if (epid != 1 || !setup)
					goto bad;
				b = usb_data_xfer_append(xfer, NULL, 0, (void *)addr, ccs);
				status = 1;
				break;
			case XHCI_TRB_TYPE_EVENT_DATA:
			case XHCI_TRB_TYPE_NOOP:
				if (epid == 1 && !setup)
					goto bad;
				b = usb_data_xfer_append(xfer, NULL, 0, (void *)addr, ccs);
				if (b != NULL)
					b->processed = 1;
				break;
			default:	/* isochronous and the rest: not for a remote device */
				goto bad;
			}
			if (b == NULL)	/* more TRBs than USB_MAX_XFER_BLOCKS */
				goto bad;
			b->streamid = 0;
			trb = pci_xhci_trb_next(sc, trb, type, &addr);
			b->trbnext = addr;
			if ((fl & XHCI_TRB_3_CHAIN_BIT) == 0 && (epid != 1 || status)) {
				end = 1;
				break;
			}
		}
		if (!end) {	/* nothing (more) on the ring yet */
			USB_DATA_XFER_RESET(xfer);
			return;
		}
		devep->keel_end = addr;
		devep->keel_endccs = ccs;
		devep->keel_busy = 1;
		if (epid != 1 && ndata == 0) {
			/* No-Op (and Event Data) TRBs only: Linux turns a cancelled TD it cannot
			   skip into No-Ops; nothing for the device, the ring moves on here */
			keel_xhci_rfinish(sc, dev, slot, epid, USB_ERR_NORMAL_COMPLETION);
			continue;
		}
		if (epid == 1)
			err = dev->dev_ue->ue_request(dev->dev_sc, xfer);
		else
			err = dev->dev_ue->ue_data(dev->dev_sc, xfer,
			    epid & 1 ? USB_XFER_IN : USB_XFER_OUT, epid / 2);
		if (err == USB_PENDING)
			return;
		keel_xhci_rfinish(sc, dev, slot, epid, err);
	}
	return;

bad:
	KXT("remote slot %u ep %u: bad TRB at 0x%lx", slot, epid, (unsigned long)addr);
	if (trb != NULL && keel_xhci_rev(sc, slot, epid, addr, trb, XHCI_TRB_ERROR_TRB, 0, 0))
		pci_xhci_assert_interrupt(sc);
	if (ep_ctx != NULL)
		ep_ctx->dwEpCtx0 = (ep_ctx->dwEpCtx0 & ~0x7) | XHCI_ST_EPCTX_HALTED;
	if (xfer != NULL)
		USB_DATA_XFER_RESET(xfer);
}

/* what is in flight on a remote endpoint goes (stopped: a Stopped event for its TD) */
static void
keel_xhci_rcancel(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev, uint32_t slot,
    int epid, int stopped)
{
	struct pci_xhci_dev_ep *devep = &dev->eps[epid];
	struct usb_data_xfer *xfer = devep->ep_xfer;
	struct xhci_trb *trb;
	uint64_t a;

	if (!devep->keel_busy || xfer == NULL) {
		devep->keel_busy = 0;
		return;
	}
	devep->keel_busy = 0;
	if (dev->dev_ue->ue_cancel != NULL)
		dev->dev_ue->ue_cancel(dev->dev_sc, xfer);
	if (stopped && slot != 0 && xfer->ndata > 0) {
		a = (uint64_t)xfer->data[xfer->head].hci_data;
		trb = XHCI_GADDR(sc, a);
		/* Stopped - Length Invalid: the TD starts again at its first TRB */
		if (trb != NULL && keel_xhci_rev(sc, slot, epid, a, trb, XHCI_TRB_ERROR_LENGTH,
		    0, 0))
			pci_xhci_assert_interrupt(sc);
	}
	USB_DATA_XFER_RESET(xfer);
}

static void
keel_xhci_rcancel_all(struct pci_xhci_softc *sc, struct pci_xhci_dev_emu *dev)
{
	int i;

	for (i = 1; i < XHCI_MAX_ENDPOINTS; i++)
		keel_xhci_rcancel(sc, dev, 0, i, 0);
}

/*
 * Address Device: the slot gets the device on the root port the input slot context names when
 * that device or the slot's present one is remote (Enable Slot hands out slots in configuration
 * order, not by port; hot plug breaks that order). Both slots swap their devices.
 */
static void
keel_xhci_rbind(struct pci_xhci_softc *sc, uint32_t slot, struct xhci_slot_ctx *islot)
{
	struct pci_xhci_dev_emu *cur, *want;
	uint32_t port, other;

	port = XHCI_SCTX_1_RH_PORT_GET(islot->dwSctx1);
	if (slot == 0 || slot > XHCI_MAX_SLOTS || port < 1 || port > XHCI_MAX_DEVS)
		return;
	cur = XHCI_SLOTDEV_PTR(sc, slot);
	want = XHCI_DEVINST_PTR(sc, port);
	if (cur == NULL || want == NULL || cur == want ||
	    (!cur->keel_remote && !want->keel_remote))
		return;
	other = keel_xhci_slot_of(sc, want);
	if (other == 0)
		return;
	if (want->keel_remote)
		keel_xhci_rcancel_all(sc, want);
	if (cur->keel_remote)
		keel_xhci_rcancel_all(sc, cur);
	XHCI_SLOTDEV_PTR(sc, slot) = want;
	XHCI_SLOTDEV_PTR(sc, other) = cur;
	want->dev_slotstate = cur->dev_slotstate;	/* enabled by Enable Slot */
	want->hci.hci_address = slot;
	cur->dev_slotstate = XHCI_ST_DISABLED;
	KXT("slot %u: bound to the device on port %u (was slot %u)", slot, port, other);
}

/* a remote port's device came or went */
static void
keel_xhci_rport(struct pci_xhci_softc *sc, int portn, int conn, int event)
{
	struct pci_xhci_portregs *p;
	struct pci_xhci_dev_emu *dev;
	struct xhci_trb ev;

	if (sc->portregs == NULL)
		return;
	p = XHCI_PORTREG_PTR(sc, portn);
	dev = XHCI_DEVINST_PTR(sc, portn);
	p->portsc &= ~(XHCI_PS_CCS | XHCI_PS_PED | XHCI_PS_PLS_MASK | XHCI_PS_SPEED_MASK |
	    XHCI_PS_PR);
	p->portsc |= XHCI_PS_PP | XHCI_PS_CSC;
	if (conn) {
		p->portsc |= XHCI_PS_CCS | XHCI_PS_SPEED_SET(KEEL_XHCI_SPEED(sc, dev));
		if (dev->hci.hci_usbver == 2)
			p->portsc |= XHCI_PS_PLS_SET(UPS_PORT_LS_POLL);
		else
			p->portsc |= XHCI_PS_PED | XHCI_PS_PLS_SET(UPS_PORT_LS_U0);
	} else
		p->portsc |= XHCI_PS_PLS_SET(UPS_PORT_LS_RX_DET);
	sc->opregs.usbsts |= XHCI_STS_PCD;
	KXT("port %d: remote device %s", portn, conn ? "connected" : "gone");
	if (event && (sc->opregs.usbcmd & XHCI_CMD_RS) && sc->rtsregs.erstba_p != NULL) {
		pci_xhci_set_evtrb(&ev, portn, XHCI_TRB_ERROR_SUCCESS,
		    XHCI_TRB_EVENT_PORT_STS_CHANGE);
		pci_xhci_insert_event(sc, &ev, 1);
	}
}

/* hci_complete: the device has the answer for endpoint epid's request */
static int
keel_xhci_dev_complete(struct usb_hci *hci, int epid, struct usb_data_xfer *xfer)
{
	struct pci_xhci_dev_emu *dev = hci->hci_sc;
	struct pci_xhci_softc *sc = dev->xsc;
	struct pci_xhci_dev_ep *devep;
	uint32_t slot;
	int err;

	if (epid < 1 || epid >= XHCI_MAX_ENDPOINTS)
		return (-1);
	pthread_mutex_lock(&sc->mtx);
	devep = &dev->eps[epid];
	slot = keel_xhci_slot_of(sc, dev);
	if (devep->keel_busy && devep->ep_xfer == xfer) {
		if (epid == 1)
			err = dev->dev_ue->ue_request(dev->dev_sc, xfer);
		else
			err = dev->dev_ue->ue_data(dev->dev_sc, xfer,
			    epid & 1 ? USB_XFER_IN : USB_XFER_OUT, epid / 2);
		if (err != USB_PENDING) {
			keel_xhci_rfinish(sc, dev, slot, epid, err);
			if (slot != 0)
				keel_xhci_rkick(sc, dev, slot, epid);
		}
	}
	pthread_mutex_unlock(&sc->mtx);
	return (0);
}

/* after a restore: a remote port that had a device has none now; the event once the guest runs */
static void
keel_xhci_rtimer(int fd __unused, enum ev_type type __unused, void *arg)
{
	struct pci_xhci_softc *sc = arg;
	struct xhci_trb ev;
	int i;

	pthread_mutex_lock(&sc->mtx);
	if (sc->keel_rtev != NULL) {
		mevent_delete(sc->keel_rtev);
		sc->keel_rtev = NULL;
	}
	for (i = 1; i <= XHCI_MAX_DEVS; i++) {
		if ((sc->keel_rpend & (1u << i)) == 0)
			continue;
		if ((sc->opregs.usbcmd & XHCI_CMD_RS) && sc->rtsregs.erstba_p != NULL) {
			pci_xhci_set_evtrb(&ev, i, XHCI_TRB_ERROR_SUCCESS,
			    XHCI_TRB_EVENT_PORT_STS_CHANGE);
			pci_xhci_insert_event(sc, &ev, 1);
		}
	}
	sc->keel_rpend = 0;
	pthread_mutex_unlock(&sc->mtx);
	usb_remote_ready();
}

static void
keel_xhci_rrestored(struct pci_xhci_softc *sc)
{
	struct pci_xhci_dev_emu *dev;
	uint32_t slot;
	int i, j;

	pthread_mutex_lock(&sc->mtx);
	for (i = 1; i <= XHCI_MAX_DEVS; i++) {
		dev = XHCI_DEVINST_PTR(sc, i);
		if (dev == NULL || !dev->keel_remote)
			continue;
		for (j = 1; j < XHCI_MAX_ENDPOINTS; j++)
			dev->eps[j].keel_busy = 0;
		/* its slot was written as unassigned: one of the free ones again */
		if (keel_xhci_slot_of(sc, dev) == 0) {
			for (slot = 1; slot <= XHCI_MAX_SLOTS; slot++)
				if (XHCI_SLOTDEV_PTR(sc, slot) == NULL)
					break;
			if (slot <= XHCI_MAX_SLOTS) {
				XHCI_SLOTDEV_PTR(sc, slot) = dev;
				dev->dev_ctx = NULL;
				dev->hci.hci_address = slot;
				dev->dev_slotstate = sc->opregs.dcbaa_p != NULL &&
				    sc->opregs.dcbaa_p->dcba[slot] != 0 ? XHCI_ST_ENABLED :
				    XHCI_ST_DISABLED;
			}
		}
		/* the device is gone (the source attaches again): a connect change, also when
		   there was none (the port registers are not in the checkpoint) */
		dev->keel_conn = 0;
		keel_xhci_rport(sc, i, 0, 0);
		sc->keel_rpend |= 1u << i;
	}
	pthread_mutex_unlock(&sc->mtx);
	/* not now: the guest's local APICs are restored after the devices; an MSI sent now is lost */
	sc->keel_rtev = mevent_add(200, EVF_TIMER, keel_xhci_rtimer, sc);
	if (sc->keel_rtev == NULL)
		usb_remote_ready();
}

/*
 * Outstanding transfer still in progress (device NAK'd earlier) so retry
 * the transfer again to see if it succeeds.
 */
''')

# doorbell: remote devices take their own path
rep('''	dev = XHCI_SLOTDEV_PTR(sc, slot);
	devep = &dev->eps[epid];
	dev_ctx = pci_xhci_get_dev_ctx(sc, slot);
	if (!dev_ctx) {
		return;
	}
	ep_ctx = &dev_ctx->ctx_ep[epid];

	sctx_tr = NULL;
''', '''	dev = XHCI_SLOTDEV_PTR(sc, slot);
	if (dev == NULL)	/* keel: a slot without a device (pci_xhci_get_dev_ctx asserts) */
		return;
	devep = &dev->eps[epid];
	dev_ctx = pci_xhci_get_dev_ctx(sc, slot);
	if (!dev_ctx) {
		return;
	}
	ep_ctx = &dev_ctx->ctx_ep[epid];

	sctx_tr = NULL;
	if (dev->keel_remote) {	/* keel: asynchronous, a TD path of its own */
		if (ep_ctx->qwEpCtx2 != 0 && streamid == 0)
			keel_xhci_rkick(sc, dev, slot, epid);
		return;
	}
''')

rep('''static int
pci_xhci_dev_event(struct usb_hci *hci, enum hci_usbev evid __unused,
    void *param __unused)
{
	DPRINTF(("xhci device event port %d", hci->hci_port));
	return (0);
}
''', '''static int
pci_xhci_dev_event(struct usb_hci *hci, enum hci_usbev evid, void *param)
{
	struct pci_xhci_dev_emu *dev = hci->hci_sc;
	struct pci_xhci_softc *sc = dev->xsc;
	struct pci_xhci_dev_ep *devep;
	uint32_t slot;
	int i, err;

	DPRINTF(("xhci device event port %d", hci->hci_port));
	/* keel: a remote device came or went (usb_remote.c) */
	if (!dev->keel_remote || (evid != USBDEV_ATTACH && evid != USBDEV_REMOVE))
		return (0);
	pthread_mutex_lock(&sc->mtx);
	if (evid == USBDEV_ATTACH) {
		if (param != NULL)
			dev->hci.hci_speed = *(int *)param;
		dev->keel_conn = 1;
	} else {
		dev->keel_conn = 0;
		/* what is still in flight ends with an error (the device says so now) */
		slot = keel_xhci_slot_of(sc, dev);
		for (i = 1; i < XHCI_MAX_ENDPOINTS; i++) {
			devep = &dev->eps[i];
			if (!devep->keel_busy || devep->ep_xfer == NULL)
				continue;
			err = i == 1 ? dev->dev_ue->ue_request(dev->dev_sc, devep->ep_xfer) :
			    dev->dev_ue->ue_data(dev->dev_sc, devep->ep_xfer,
			    i & 1 ? USB_XFER_IN : USB_XFER_OUT, i / 2);
			if (err == USB_PENDING || slot == 0)
				keel_xhci_rcancel(sc, dev, 0, i, 0);
			else
				keel_xhci_rfinish(sc, dev, slot, i, err);
		}
	}
	keel_xhci_rport(sc, hci->hci_port, dev->keel_conn, 1);
	pthread_mutex_unlock(&sc->mtx);
	return (0);
}
''')
rep('''		dev->hci.hci_event = pci_xhci_dev_event;
''', '''		dev->hci.hci_event = pci_xhci_dev_event;
		dev->hci.hci_complete = keel_xhci_dev_complete;	/* keel */
''')
rep('''		dev->dev_ue = ue;
''', '''		dev->dev_ue = ue;
		dev->keel_remote = ue->ue_cancel != NULL;	/* keel: usb_remote.c */
''')
# remote2=N, remote3=N
rep('''	int slot;

	if (opts == NULL)
		return (0);
''', '''	int slot, nrep;

	if (opts == NULL)
		return (0);
''')
rep('''		snprintf(node_name, sizeof(node_name), "%d", slot);
		slot++;
		slot_nvl = create_relative_config_node(slots_nvl, node_name);
		set_config_value_node(slot_nvl, "device", opt);
''', '''		/* keel: remote2=N, remote3=N: N empty ports of that kind (usb_remote.c) */
		nrep = 1;
		if (cp != NULL && (strcmp(opt, "remote2") == 0 || strcmp(opt, "remote3") == 0)) {
			nrep = (int)strtol(cp, NULL, 10);
			if (nrep < 1 || nrep > XHCI_MAX_DEVS / 2)
				nrep = 1;
			cp = NULL;
		}
		do {
			snprintf(node_name, sizeof(node_name), "%d", slot);
			slot++;
			slot_nvl = create_relative_config_node(slots_nvl, node_name);
			set_config_value_node(slot_nvl, "device", opt);
		} while (--nrep > 0);
''')
rep('''	pthread_mutex_init(&sc->mtx, NULL);

done:
''', '''	pthread_mutex_init(&sc->mtx, NULL);

	/* keel: remote ports take sources now; after a restore only when the guest runs */
#ifdef BHYVE_SNAPSHOT
	if (restore_file == NULL)
#endif
		usb_remote_ready();

done:
''')
# checkpoint: 8 blocks as before, the live ones first; a remote endpoint's TD is not kept
rep('''	struct usb_data_xfer *xfer;
	struct usb_data_xfer_block *xfer_block;

	/* some sanity checks */
''', '''	struct usb_data_xfer *xfer;
	struct usb_data_xfer_block *xfer_block;
	struct usb_data_xfer_block kblk[KEEL_SNAP_XFER_BLOCKS];	/* keel */
	int kn = 0, kh = 0, kt = 0;

	memset(kblk, 0, sizeof(kblk));

	/* some sanity checks */
''')
rep('''	/* save / restore proper */
	for (k = 0; k < USB_MAX_XFER_BLOCKS; k++) {
		xfer_block = &xfer->data[k];
''', '''	/* save / restore proper; keel: KEEL_SNAP_XFER_BLOCKS (8) blocks, as before
	   USB_MAX_XFER_BLOCKS grew to 256 (patch-usbemul.py): the live ones first */
	if (meta->op == VM_SNAPSHOT_SAVE) {
		kn = xfer->ndata;
		if (kn < 0 || kn > KEEL_SNAP_XFER_BLOCKS || dev->keel_remote)
			kn = 0;
		for (k = 0; k < kn; k++)
			kblk[k] = xfer->data[(xfer->head + k) % USB_MAX_XFER_BLOCKS];
		kt = kn;
	}
	for (k = 0; k < KEEL_SNAP_XFER_BLOCKS; k++) {
		xfer_block = &kblk[k];
''')
rep('''	SNAPSHOT_VAR_OR_LEAVE(xfer->ndata, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(xfer->head, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(xfer->tail, meta, ret, done);
''', '''	SNAPSHOT_VAR_OR_LEAVE(kn, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(kh, meta, ret, done);
	SNAPSHOT_VAR_OR_LEAVE(kt, meta, ret, done);
	if (meta->op == VM_SNAPSHOT_RESTORE) {	/* keel: an old checkpoint wraps at 8 */
		USB_DATA_XFER_RESET(xfer);
		if (kn < 0 || kn > KEEL_SNAP_XFER_BLOCKS || kh < 0 ||
		    kh >= KEEL_SNAP_XFER_BLOCKS || dev->keel_remote)
			kn = 0;
		for (k = 0; k < kn; k++)
			xfer->data[k] = kblk[(kh + k) % KEEL_SNAP_XFER_BLOCKS];
		xfer->ndata = kn;
		xfer->head = 0;
		xfer->tail = kn;
	}
''')
rep('''done:
	return (ret);
}
#endif

static const struct pci_devemu pci_de_xhci = {
''', '''	if (meta->op == VM_SNAPSHOT_RESTORE)	/* keel: remote ports are empty now */
		keel_xhci_rrestored(sc);

done:
	return (ret);
}
#endif

static const struct pci_devemu pci_de_xhci = {
''')

# checkpoints (docs/66 §10.7, 2026-10-07): a remote device is not in the checkpoint at all: not in
# the device list, its port registers not, its slot written as unassigned (maps 0, no slot data).
# The layout is that of the same guest without remote ports, so the bhyve before them restores
# it (stock bhyve skips a slot whose port has no device without reading its data: with the
# remote devices in the list it read the rest of the stream misaligned and crashed, SIGSEGV).
# xhci version 1 (same layout) marks a checkpoint in which a remote device held a slot the guest
# uses: the bhyve before would assert on the guest's next doorbell for that slot, so it must
# refuse the checkpoint (exit 65, kept). Format level 4 (keel/internal/format/levels.json).
# After a restore the remote devices get the slots left unassigned again (enabled where the
# guest's DCBAA has a context), every remote port shows a connect change, the event 200 ms later.
rep('''	pi = meta->dev_data;
	sc = pi->pi_arg;

	SNAPSHOT_VAR_OR_LEAVE(sc->caplength, meta, ret, done);
''', '''	pi = meta->dev_data;
	sc = pi->pi_arg;

	{	/* keel: the version (on a save: 1 only when a remote device holds a slot) */
		int used = 0;

		for (i = 1; meta->op == VM_SNAPSHOT_SAVE && i <= XHCI_MAX_SLOTS; i++) {
			dev = XHCI_SLOTDEV_PTR(sc, i);
			if (dev != NULL && dev->keel_remote && dev->dev_slotstate != XHCI_ST_DISABLED)
				used = 1;
		}
		if (keel_snap_version_max(meta, "xhci", used) < used)
			EPRINTLN("keel: xhci: a remote USB device holds a slot, but the checkpoint "
			    "is capped at xhci:0 (keel.snap_write): a bhyve without remote ports "
			    "will trip on that slot");
	}

	SNAPSHOT_VAR_OR_LEAVE(sc->caplength, meta, ret, done);
''')
rep('''	for (i = 1; i <= XHCI_MAX_DEVS; i++) {
		dev = XHCI_DEVINST_PTR(sc, i);
		if (dev == NULL)
			continue;

		if (meta->op == VM_SNAPSHOT_SAVE)
			restore_idx = i;
''', '''	for (i = 1; i <= XHCI_MAX_DEVS; i++) {
		dev = XHCI_DEVINST_PTR(sc, i);
		if (dev == NULL || dev->keel_remote)	/* keel: not in the checkpoint */
			continue;

		if (meta->op == VM_SNAPSHOT_SAVE)
			restore_idx = i;
''')
rep('''		port = XHCI_PORTREG_PTR(sc, i);
		dev = XHCI_DEVINST_PTR(sc, i);

		if (dev == NULL)
			continue;
''', '''		port = XHCI_PORTREG_PTR(sc, i);
		dev = XHCI_DEVINST_PTR(sc, i);

		if (dev == NULL || dev->keel_remote)	/* keel */
			continue;
''')
rep('''	if (meta->op == VM_SNAPSHOT_SAVE)
		pci_xhci_map_devs_slots(sc, maps);
''', '''	if (meta->op == VM_SNAPSHOT_SAVE) {
		pci_xhci_map_devs_slots(sc, maps);
		for (i = 1; i <= XHCI_MAX_SLOTS; i++)	/* keel: a remote device's slot: unassigned */
			if (XHCI_SLOTDEV_PTR(sc, i) != NULL && XHCI_SLOTDEV_PTR(sc, i)->keel_remote)
				maps[i] = 0;
	}
''')
rep('''		if (meta->op == VM_SNAPSHOT_SAVE) {
			dev = XHCI_SLOTDEV_PTR(sc, i);
		} else if''', '''		if (meta->op == VM_SNAPSHOT_SAVE) {
			dev = maps[i] != 0 ? XHCI_SLOTDEV_PTR(sc, i) : NULL;	/* keel */
		} else if''')

# Remote wakeup of a suspended port (docs/28 §16, the P330, 2026-10-11): macOS suspends the
# tablet's and keyboard's ports (PLS U3) when the display sleeps, and waits for them to wake it.
# bhyve set PLC on that software-asked U3 entry too, which the xHCI spec does not do, so macOS
# never cleared it; on the next input pci_xhci_dev_intr() moved the port to Resume, found PLC
# already set and returned: no Port Status Change event and the input dropped. The display never
# came back (the guest ran on: network fine, all vCPUs idle). Now U3 entry leaves PLC alone and
# a device's input on a U3 port always signals the resume (once: the port is in Resume then).
rep('''				p->portsc |= XHCI_PS_PLS_SET(newpls) |
				             XHCI_PS_PLC;
''', '''				p->portsc |= XHCI_PS_PLS_SET(newpls) |
				             (newpls == 3 ? 0 : XHCI_PS_PLC);	/* keel: not on suspend */
''')
rep('''		p->portsc |= XHCI_PS_PLS_SET(UPS_PORT_LS_RESUME);
		if ((p->portsc & XHCI_PS_PLC) != 0)
			goto done;

		p->portsc |= XHCI_PS_PLC;
''', '''		p->portsc |= XHCI_PS_PLS_SET(UPS_PORT_LS_RESUME);
		/* keel: always tell the driver (a PLC left set used to drop the wakeup and the input) */
		p->portsc |= XHCI_PS_PLC;
''')

sys.stdout.write(src)
