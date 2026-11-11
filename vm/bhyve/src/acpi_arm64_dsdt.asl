/*
 * The DSDT of bhyve's arm64 machine: everything in the namespace that does not depend on the
 * VM's configuration (the processors are in an SSDT that acpi_arm64.c generates).
 *
 * Compiled ahead of time, on any machine with ACPICA's compiler:
 *	iasl -tc -p acpi_arm64_dsdt acpi_arm64_dsdt.asl	 ->  acpi_arm64_dsdt.hex
 * (kept as acpi_arm64_dsdt.hex: bhyve on macOS has no iasl to run at start as FreeBSD's does).
 *
 * The addresses and interrupts are those of aarch64/bhyverun_machdep.c and pci_emul.c:
 *	UART (PL011)	0x09000000, 4K, SPI 32
 *	fw_cfg		0x09020000, 0x18 bytes
 *	PCI ECAM	0xe0000000, 256 MB (256 buses)
 *	PCI memory	0xa0000000 - 0xdeffffff
 *	PCI memory, 64-bit	32 GB at 0x4000000000 (256 GB)
 *	PCI I/O		ports 0 - 0xffff at 0xdf000000
 *	PCI INTA-D	SPI 34 - 37, rotated by one per slot
 */
DefinitionBlock ("", "DSDT", 2, "BHYVE ", "BVDSDT  ", 0x00000001)
{
    Scope (\_SB)
    {
        Device (COM0)
        {
            Name (_HID, "ARMH0011")
            Name (_UID, Zero)
            Name (_CRS, ResourceTemplate ()
            {
                Memory32Fixed (ReadWrite, 0x09000000, 0x00001000)
                Interrupt (ResourceConsumer, Level, ActiveHigh, Exclusive) { 32 }
            })
        }

        Device (FWCF)
        {
            Name (_HID, "QEMU0002")
            Name (_STA, 0x0B)
            Name (_CCA, One)
            Name (_CRS, ResourceTemplate ()
            {
                Memory32Fixed (ReadWrite, 0x09020000, 0x00000018)
            })
        }

        /* The power button: bhyve raises SPI 39 through the generic event device. */
        Device (PWRB)
        {
            Name (_HID, "PNP0C0C")
            Name (_UID, Zero)
        }

        Device (GED0)
        {
            Name (_HID, "ACPI0013")
            Name (_UID, Zero)
            Name (_CRS, ResourceTemplate ()
            {
                Interrupt (ResourceConsumer, Edge, ActiveHigh, Exclusive) { 39 }
            })
            Method (_EVT, 1, Serialized)
            {
                If ((Arg0 == 39))
                {
                    Notify (\_SB.PWRB, 0x80)
                }
            }
        }

        Device (PCI0)
        {
            Name (_HID, "PNP0A08")
            Name (_CID, "PNP0A03")
            Name (_SEG, Zero)
            Name (_BBN, Zero)
            Name (_UID, Zero)
            Name (_CCA, One)
            Name (_STR, Unicode ("PCIe 0 Device"))

            Name (_CRS, ResourceTemplate ()
            {
                WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode,
                    0x0000, 0x0000, 0x00FF, 0x0000, 0x0100)
                DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                    0x00000000, 0xA0000000, 0xDEFFFFFF, 0x00000000, 0x3F000000)
                QWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                    0x0000000000000000, 0x0000004000000000, 0x00000047FFFFFFFF, 0x0000000000000000,
                    0x0000000800000000)
                DWordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                    0x00000000, 0x00000000, 0x0000FFFF, 0xDF000000, 0x00010000,
                    ,, , TypeStatic, DenseTranslation)
            })

            /* The memory behind the configuration space is not for the operating system to hand out. */
            Device (RES0)
            {
                Name (_HID, "PNP0C02")
                Name (_CRS, ResourceTemplate ()
                {
                    QWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                        0x0000000000000000, 0x00000000E0000000, 0x00000000EFFFFFFF,
                        0x0000000000000000, 0x0000000010000000)
                })
            }

            /* INTx: pin P of slot S is SPI 34 + ((S + P) % 4). */
            Method (_PRT, 0, NotSerialized)
            {
                Local0 = Package (0x80) {}
                Local1 = Zero
                While ((Local1 < 0x20))
                {
                    Local2 = Zero
                    While ((Local2 < 0x04))
                    {
                        Local3 = Package (0x04) {}
                        Local3 [Zero] = ((Local1 << 0x10) | 0xFFFF)
                        Local3 [One] = Local2
                        Local3 [0x02] = Zero
                        Local3 [0x03] = (34 + ((Local1 + Local2) % 0x04))
                        Local0 [((Local1 * 0x04) + Local2)] = Local3
                        Local2++
                    }
                    Local1++
                }
                Return (Local0)
            }

            /*
             * The operating system may run PCI Express native features (hot plug, power
             * management events, error reporting, capability structure, LTR) itself:
             * there is no firmware that would.
             */
            Name (SUPP, Zero)
            Name (CTRL, Zero)
            Method (_OSC, 4, NotSerialized)
            {
                CreateDWordField (Arg3, Zero, CDW1)
                If ((Arg0 == ToUUID ("33db4d5b-1ff7-401c-9657-7441c03dd766")))
                {
                    CreateDWordField (Arg3, 0x04, CDW2)
                    CreateDWordField (Arg3, 0x08, CDW3)
                    SUPP = CDW2
                    CTRL = CDW3
                    CTRL &= 0x1F
                    If ((Arg1 != One))
                    {
                        CDW1 |= 0x08
                    }
                    If ((CDW3 != CTRL))
                    {
                        CDW1 |= 0x10
                    }
                    CDW3 = CTRL
                    Return (Arg3)
                }
                Else
                {
                    CDW1 |= 0x04
                    Return (Arg3)
                }
            }
        }
    }
}
