# Automatic pairing of a macOS guest with its host: done on the hyperMAC side (2026-11-11)

**Status, from the hyperMAC session.** Built as proposed below; in hyperMAC-dev from the commit that
changes this file, and in the release after `7a4c558`. What exists:

- **The channel**: every macOS guest gets a shared directory of its own (`<VM folder>/share` on the host,
  `/Volumes/My Shared Files` in the guest; `vm/src/vm.rs`). The host writes `hypermac-host` into it.
- **The guest** (`src/guest.rs`): a hyperMAC that runs in a virtual machine and finds that mark writes
  `hypermac-guest.json` there (`token`, `cert_sha256`, `rdp_port`, `build`). The token is its own, made
  for the host, written again unchanged at each start; a new one when it was revoked in the guest or when
  the machine is a copy (`kern.uuid` differs). Checked every 15 s.
- **The host** (`src/vms.rs` `direct`): reads that file (else the manual pairing), and offers `rdp_direct`
  only while the guest's RDP port answers. The wire is unchanged: `host`, `port`, `token`, `cert_sha256`
  (never empty with automatic pairing).
- **`rdp_direct_why`** (a string, in the guest's `/v1/vms` entry when it runs and has no `rdp_direct`), one of:
  - `the virtual machine has no address yet`
  - `restart the virtual machine once: it was started by a hyperMAC from before automatic pairing`
  - `hyperMAC is not installed in the virtual machine, or nobody is logged in there`
  - `the hyperMAC in the virtual machine does not answer (is somebody logged in there?)`
- **`/v1/version`** now has `"build"`: the commit of the release (`"dev"` for a build from source).
- The manual "Pair for Remote…" stays for a guest without the file; the window hides it once the guest
  paired by itself.

**Verified**: the guest-side logic by a unit test (writes once, the same again is not rewritten, a revoked
token is replaced and the old one stops working, the certificate's SHA-256); Virtualization.framework
accepts the shared directory device (a macOS guest's install started with it).
**Not verified** (nobody has run it end to end): that the guest mounts the share by itself and the service
sees it, the whole path to a double-click in kRDPclient, an existing guest (`mac1`) after the update.
For `mac1`: update the Air, **stop and start `mac1`** (the device is added when the VM starts), update the
hyperMAC inside it, log in.

Not done: a guest that is not logged in still serves nothing (the last point under "Also asked").

---

## The request (kRDPclient session)

Written 2026-11-11 by the kRDPclient session, for the hyperMAC session (user: "不能自动配对么" …
"你写一下要求，我让 hyperMAC 那边改。然后你配合"). The client side of `rdp_direct` is built
(kRDPclient working tree, TODO-rdp-direct.md there; not committed until it has opened a guest once).
Nothing below is tried: the mechanism in "A way to do it" is a proposal, the requirement is the
part the user asked for.

## What the user saw

Air (192.168.100.41) and the guest `mac1` (192.168.100.42) both updated to the newest hyperMAC 1.1.
Double-click on `mac1` under the Air in kRDPclient: "no RDP", three times in a row. Each time the
list had no `rdp_direct`, for a different reason the user could not see:

1. the Air still ran a build from before `4a99910`;
2. the guest had not been logged in, so the hyperMAC inside it was not running;
3. nobody had paired it in the Air's window: the user expected the update to be enough.

The user does not want a pairing step at all.

## What is asked

1. **No pairing step.** When a macOS guest of this host runs hyperMAC, the host's `/v1/vms` carries
   `rdp_direct` for it without the user typing the guest's account and password anywhere. Installing
   hyperMAC in the guest (and the guest being logged in, as long as hyperMAC needs that) is all the
   user does.
2. **It keeps itself right.** A guest whose hyperMAC was reinstalled, whose token was revoked or
   whose certificate changed is paired again by itself; a cloned VM gets its own pairing, not the
   original's.
3. **The wire stays as it is.** `rdp_direct` = `host`, `port`, `token`, `cert_sha256` exactly as in
   `4a99910` (`src/vms.rs`), so kRDPclient needs no change for this. `cert_sha256` should always be
   there once pairing is automatic: kRDPclient takes any certificate when it is empty.
4. **The manual pairing may stay** as the fallback (a guest made by an older hyperMAC, a guest
   without the channel below), but the window should not be where the user has to go.

## A way to do it (proposal)

The host has to prove to the guest's hyperMAC that it is this VM's host, by something only the two
share. The account and password do that today. A channel the host gives the VM does it without them:

- The host adds a small shared directory to every macOS guest (`VZVirtioFileSystemDeviceConfiguration`
  with `VZVirtioFileSystemDeviceConfiguration.macOSGuestAutomountTag`; macOS 13+ guests mount it by
  themselves under `/Volumes/My Shared Files`). One directory per VM, in the VM's folder, nothing
  else in it.
- The hyperMAC in the guest already knows it is a guest (`kern.hv_vmm_present`, `src/ui.rs`
  `can_virtualize`). There, when the directory is mounted, it makes a token for the host
  (`web::new_token`, named like the manual one: "host …") and writes token, certificate SHA-256 and
  RDP port into the directory: the same three values `guest-rdp.json` holds now.
- The host reads that file where it reads `guest-rdp.json` today (`vms::direct`).
- Written again when the guest's hyperMAC starts and whenever its certificate or token list
  changes, so point 2 above holds. A token it finds revoked in its own `tokens.json` is replaced.

Why this and not "trust requests from the host's address": on a bridged network an address proves
nothing; a directory only the VM's own host process can read does. The file holds a token for the
guest's desktop, so 0600 on the host side as `guest-rdp.json` is now.

Not known and to be found out there: whether an existing VM takes the new device without being made
again (it is configuration, not the saved hardware model, so it should), whether the automount needs
the user logged in (hyperMAC in the guest needs that anyway today), and whether a LaunchAgent sees
`/Volumes/My Shared Files` early enough or has to wait for it.

## Also asked, small

- **Say why there is no `rdp_direct`.** For a macOS guest without it, an optional string in the
  guest's entry, e.g. `"rdp_direct_why": "hyperMAC in the guest does not answer"` /
  `"the guest has no address yet"` / `"hyperMAC is not installed in the guest"`. kRDPclient will show
  it in place of its own general sentence; tell the kRDPclient session the field's name when it is
  in, an absent field stays "no RDP" as now.
- **`/v1/version` with the build.** It says `{"product":"hyperMAC","version":"1.1.0"}` for every
  1.1 build, so nobody could tell from outside whether the Air had `4a99910`. Add the commit (the
  one in the release's name), e.g. `"build": "7a4c558"`.
- **A guest that is not logged in** serves nothing, so after every guest restart somebody has to log
  in on the host first. Not asked for now; written down because it is the next thing the user will
  meet (auto-login in the guest, or hyperMAC's server as a LaunchDaemon).

## To check when it is built

- A fresh macOS guest: install hyperMAC inside, log in, nothing else: within one list refresh the
  host's `/v1/vms` has `rdp_direct` for it, and kRDPclient opens it by double-click.
- `mac1` on the Air, which exists already and was never paired: the same after both sides update.
- Revoke the host's token in the guest's hyperMAC: a new one is there by the next list, and a
  kRDPclient tab that dropped reconnects (it takes the token from the newest list).
- Stop the guest's hyperMAC: `rdp_direct` goes away or `rdp_direct_why` says so; it must not keep
  offering a token for a server that is not there. (Today a paired guest keeps `rdp_direct` while
  the VM runs, whether or not its hyperMAC answers.)
- The shared directory shows nothing of the host's files.
