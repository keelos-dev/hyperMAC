#!/usr/bin/env python3
"""Prints THIRD_PARTY-vm.txt (tools/package.sh puts it in the app and the dmg): the notices of
what the virtual machine programs are made of.

  - bhyve: FreeBSD's sources in vm/bhyve/freebsd. The copyright and license comment at the top
    of every imported file, identical texts listed once with their files.
  - the UEFI firmware (EDK2): vm/bhyve/firmware/License-edk2.txt.
  - the VM program's Rust crates: THIRD_PARTY-vm-crates.txt (tools/third-party.py).
"""
import os, re, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
FB = os.path.join(ROOT, "vm", "bhyve", "freebsd")


def header(path):
    """The first comment of the file that states a copyright or a license, cleaned of its frame."""
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read(20000)
    for m in re.finditer(r"/\*.*?\*/", text, re.S):
        c = m.group(0)
        if not re.search(r"copyright|SPDX-License-Identifier", c, re.I):
            continue
        lines = [re.sub(r"^\s*(/\*+|\*+/|\*)?\s?", "", l).rstrip() for l in c.splitlines()]
        lines = [re.sub(r"\*+/$", "", l).rstrip() for l in lines]
        while lines and not lines[0]:
            lines.pop(0)
        while lines and not lines[-1]:
            lines.pop()
        return "\n".join(lines)
    return None


def main():
    out = sys.stdout
    rev = open(os.path.join(ROOT, "vm", "bhyve", "UPSTREAM")).read().split()[0]
    out.write("Third-party software in hyperMAC's virtual machine programs\n" + "=" * 60 + "\n\n")
    out.write("hyperMAC.app/Contents/MacOS/bhyve is FreeBSD's bhyve (https://www.freebsd.org, commit\n"
              f"{rev}) with changes, built for macOS. The notices of its source files follow.\n"
              "hyperMAC.app/Contents/Resources/QEMU_EFI.fd is EDK2's ArmVirtQemuKernel firmware (https://www.tianocore.org,\n"
              "tag edk2-stable202608) with changes; its license follows them. EDK2 builds in third-party\n"
              "components of its own (among them libfdt, the LZMA SDK and OpenSSL), each under the license\n"
              "stated in EDK2's sources.\n"
              "hyperMAC.app/Contents/MacOS/hypermac-vm is hyperMAC's own; the Rust crates it is built from are at the end.\n\n")
    groups, none = {}, []
    for d, _, files in sorted(os.walk(FB)):
        for n in sorted(files):
            if not n.endswith((".c", ".h", ".S", ".s", ".asl")):
                continue
            p = os.path.join(d, n)
            h = header(p)
            rel = os.path.relpath(p, FB)
            if h is None:
                none.append(rel)
            else:
                groups.setdefault(h, []).append(rel)
    out.write(f"FreeBSD ({sum(len(v) for v in groups.values())} files, {len(groups)} distinct notices)\n" + "-" * 60 + "\n\n")
    for h, files in sorted(groups.items(), key=lambda kv: kv[1][0]):
        out.write("Files: " + ", ".join(files) + "\n\n" + "".join("  " + l + "\n" for l in h.splitlines()) + "\n" + "-" * 40 + "\n\n")
    if none:
        out.write("Files without a notice of their own (the FreeBSD project's license applies): " + ", ".join(none) + "\n\n")
    out.write("EDK2 firmware\n" + "-" * 60 + "\n\n")
    out.write(open(os.path.join(ROOT, "vm", "bhyve", "firmware", "License-edk2.txt")).read().rstrip() + "\n\n")
    tp = os.path.join(ROOT, "vm", "bhyve", "third-party")
    out.write("libtpms (in bhyve: the TPM 2.0 of a Windows machine; https://github.com/stefanberger/libtpms)\n" + "-" * 60 + "\n\n")
    out.write(open(os.path.join(tp, "LICENSE-libtpms")).read().rstrip() + "\n\n")
    out.write("OpenSSL (libcrypto, in bhyve: what libtpms computes with; https://www.openssl.org)\n" + "-" * 60 + "\n\n")
    out.write(open(os.path.join(tp, "LICENSE-openssl")).read().rstrip() + "\n\n")
    out.write("Rust crates of hypermac-vm\n" + "-" * 60 + "\n\n")
    out.write(open(os.path.join(ROOT, "THIRD_PARTY-vm-crates.txt")).read())


main()
