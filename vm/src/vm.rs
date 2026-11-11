//! The VMs (Virtualization.framework), no window on the host.
//! macOS guests: VZ is the only way to run them on Apple Silicon; they get Apple's paravirtual GPU
//! (Metal works in the guest). Linux guests: EFI + virtio, VZ's 2D virtio-gpu.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use anyhow::{Context, anyhow};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSScreen, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSPoint, NSRect, NSSize};
use objc2_foundation::{NSArray, NSData, NSError, NSString, NSURL};
use objc2_virtualization::*;

use crate::{VmConf, load, num, opt, vm_dir};

/// Objects that are only touched on the main queue, carried through Send closures.
struct OnMain<T>(T);
unsafe impl<T> Send for OnMain<T> {}
unsafe impl<T> Sync for OnMain<T> {}

static VNC_PORT: AtomicU16 = AtomicU16::new(0);
static WAS_RUNNING: AtomicBool = AtomicBool::new(false);

fn url(p: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()))
}

fn err(e: Retained<NSError>) -> anyhow::Error {
    anyhow!("{}", e.localizedDescription())
}

fn die(e: anyhow::Error) -> ! {
    eprintln!("error: {e:#}");
    std::process::exit(1)
}

/// NAT, or the socket of a vmnet bridge from the root helper (vmnet.rs).
fn net_attachment(c: &VmConf) -> anyhow::Result<Retained<VZNetworkDeviceAttachment>> {
    let net = c.net.as_deref().unwrap_or("nat");
    if net == "nat" {
        return Ok(Retained::into_super(unsafe { VZNATNetworkDeviceAttachment::new() }));
    }
    let host_if = match net.strip_prefix("bridged:") {
        Some(i) => i.to_string(),
        None => crate::vmnet::default_interface().context("no default route: name the interface (bridged:en0)")?,
    };
    use std::os::fd::IntoRawFd;
    // macOS 26+ lets a process with the virtualization entitlement start the bridge itself;
    // older systems want root for it: then ask the helper (hypermac netd)
    let fd = match crate::vmnet::bridge(&host_if) {
        Ok(fd) => fd,
        Err(e) => crate::vmnet::request(&host_if).with_context(|| format!("in-process bridge failed ({e:#})"))?,
    }
    .into_raw_fd();
    eprintln!("network: bridged on {host_if}");
    unsafe {
        let fh = objc2_foundation::NSFileHandle::initWithFileDescriptor_closeOnDealloc(objc2_foundation::NSFileHandle::alloc(), fd, true);
        let a = VZFileHandleNetworkDeviceAttachment::initWithFileHandle(VZFileHandleNetworkDeviceAttachment::alloc(), &fh);
        Ok(Retained::into_super(a))
    }
}

/// The VM's configuration. `new_hw`: the hardware model of a VM being created (writes
/// hardware-model, machine-id and a fresh aux.img); None reads what create left.
fn config_macos(dir: &Path, c: &VmConf, new_hw: Option<&VZMacHardwareModel>) -> anyhow::Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
        let cfg = VZVirtualMachineConfiguration::new();
        cfg.setCPUCount(c.cpus);
        cfg.setMemorySize(c.mem_gb << 30);
        cfg.setBootLoader(Some(&VZMacOSBootLoader::new()));

        let p = VZMacPlatformConfiguration::new();
        let (hwf, idf, auxf) = (dir.join("hardware-model"), dir.join("machine-id"), dir.join("aux.img"));
        match new_hw {
            Some(hw) => {
                std::fs::write(&hwf, hw.dataRepresentation().to_vec())?;
                let id = VZMacMachineIdentifier::new();
                std::fs::write(&idf, id.dataRepresentation().to_vec())?;
                let aux = VZMacAuxiliaryStorage::initCreatingStorageAtURL_hardwareModel_options_error(
                    VZMacAuxiliaryStorage::alloc(),
                    &url(&auxf),
                    hw,
                    VZMacAuxiliaryStorageInitializationOptions::AllowOverwrite,
                )
                .map_err(err)?;
                p.setHardwareModel(hw);
                p.setMachineIdentifier(&id);
                p.setAuxiliaryStorage(Some(&aux));
            }
            None => {
                let hw = VZMacHardwareModel::initWithDataRepresentation(
                    VZMacHardwareModel::alloc(),
                    &NSData::with_bytes(&std::fs::read(&hwf).context("hardware-model")?),
                )
                .context("bad hardware-model")?;
                anyhow::ensure!(hw.isSupported(), "this Mac cannot run the VM's hardware model");
                let id = VZMacMachineIdentifier::initWithDataRepresentation(
                    VZMacMachineIdentifier::alloc(),
                    &NSData::with_bytes(&std::fs::read(&idf).context("machine-id")?),
                )
                .context("bad machine-id")?;
                p.setHardwareModel(&hw);
                p.setMachineIdentifier(&id);
                p.setAuxiliaryStorage(Some(&VZMacAuxiliaryStorage::initWithURL(VZMacAuxiliaryStorage::alloc(), &url(&auxf))));
            }
        }
        cfg.setPlatform(&p);

        // Apple's paravirtual GPU: Metal in the guest. 80 ppi = a 1x display.
        let gpu = VZMacGraphicsDeviceConfiguration::new();
        let disp = VZMacGraphicsDisplayConfiguration::initWithWidthInPixels_heightInPixels_pixelsPerInch(
            VZMacGraphicsDisplayConfiguration::alloc(),
            c.w,
            c.h,
            80,
        );
        gpu.setDisplays(&NSArray::from_retained_slice(&[disp]));
        cfg.setGraphicsDevices(&NSArray::from_retained_slice(&[Retained::into_super(gpu)]));

        let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &url(&dir.join("disk.img")),
            false,
        )
        .map_err(err)?;
        let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), &att);
        cfg.setStorageDevices(&NSArray::from_retained_slice(&[Retained::into_super(blk)]));

        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setAttachment(Some(&*net_attachment(c)?));
        let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&c.mac)).context("bad mac in vm.json")?;
        net.setMACAddress(&mac);
        cfg.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        cfg.setKeyboards(&NSArray::from_retained_slice(&[
            Retained::into_super(VZUSBKeyboardConfiguration::new()),
            Retained::into_super(VZMacKeyboardConfiguration::new()),
        ]));
        cfg.setPointingDevices(&NSArray::from_retained_slice(&[
            Retained::into_super(VZUSBScreenCoordinatePointingDeviceConfiguration::new()),
            Retained::into_super(VZMacTrackpadConfiguration::new()),
        ]));

        // A sound card whose output goes nowhere on the host: the guest has a default output
        // device (macRDP in the guest taps it), and the host stays silent.
        let snd = VZVirtioSoundDeviceConfiguration::new();
        let out = VZVirtioSoundDeviceOutputStreamConfiguration::new();
        snd.setStreams(&NSArray::from_retained_slice(&[Retained::into_super(out)]));
        cfg.setAudioDevices(&NSArray::from_retained_slice(&[Retained::into_super(snd)]));

        cfg.validateWithError().map_err(err)?;
        Ok(cfg)
    }
}

/// hypermac create NAME --ipsw PATH|latest [--cpus N] [--mem-gb G] [--disk-gb G] [--w W --h H]
/// `--ipsw latest`: the newest macOS this Mac can run as a guest, downloaded from Apple into the
/// storage folder's iso/ first (the address comes from Virtualization.framework).
pub fn create(name: &str, args: &[String]) -> anyhow::Result<()> {
    // Windows from Microsoft's ISO, installed unattended (winsetup.rs); a made-up stick (.img)
    // still goes the old way
    if opt(args, "--os") == Some("windows") && !opt(args, "--iso").is_some_and(|i| i.ends_with(".img")) {
        // Windows runs on bhyve (2026-11-11); Virtualization.framework + efiprobe only when
        // asked for, or where the bhyve engine is not installed
        let engine = opt(args, "--engine").unwrap_or(if crate::bhyve::program().is_some() { "bhyve" } else { "vz" });
        if engine == "bhyve" {
            return crate::bhyve::create_windows(name, args);
        }
        return crate::winsetup::create(name, args);
    }
    if matches!(opt(args, "--os"), Some("linux" | "windows" | "freebsd")) || (opt(args, "--iso").is_some() && opt(args, "--ipsw").is_none()) {
        return create_linux(name, args);
    }
    let ipsw = std::path::PathBuf::from(opt(args, "--ipsw").context("--ipsw PATH (a macOS restore image), or --ipsw latest")?);
    let download = ipsw.as_os_str() == "latest";
    anyhow::ensure!(download || ipsw.exists(), "{}: no such file", ipsw.display());
    // the file itself, not a link to it or a path through one: Virtualization.framework reports
    // "the restore image does not exist" for those (2026-11-11, a state folder that is a link)
    let ipsw = if download { ipsw } else { std::fs::canonicalize(&ipsw).unwrap_or(ipsw) };
    let dir = vm_dir(name);
    anyhow::ensure!(!dir.join("vm.json").exists(), "VM {name:?} exists ({})", dir.display());
    std::fs::create_dir_all(&dir)?;
    let mut pw = [0u8; 4];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut pw))?;
    let conf = VmConf {
        os: "macos".into(),
        cpus: num(args, "--cpus", 4) as usize,
        mem_gb: num(args, "--mem-gb", 6),
        w: num(args, "--w", 1920) as isize,
        h: num(args, "--h", 1080) as isize,
        mac: unsafe { VZMACAddress::randomLocallyAdministeredAddress().string() }.to_string(),
        vnc_password: pw.iter().map(|b| format!("{b:02x}")).collect(),
        iso: None,
        drivers: None,
        own_devices: false,
        net: opt(args, "--net").filter(|n| *n != "nat").map(str::to_string),
        os_version: opt(args, "--os-version").map(str::to_string),
        engine: None,
    };
    let disk_gb = num(args, "--disk-gb", 64);
    let name = name.to_string();

    if download {
        // listed in the window from the start, with the download's progress
        std::fs::write(dir.join("installing"), "dl:0")?;
        std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&conf)?)?;
        eprintln!("asking Apple for the newest macOS this Mac can run as a guest");
        let fetched = RcBlock::new(move |img: *mut VZMacOSRestoreImage, e: *mut NSError| {
            let Some(img) = (unsafe { Retained::retain(img) }) else {
                let _ = std::fs::remove_dir_all(&dir);
                die(anyhow!("Apple's list of restore images: {}", unsafe { e.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default()));
            };
            let from = unsafe { img.URL() }.absoluteString().map(|s| s.to_string()).unwrap_or_default();
            let (conf, dir, name) = (conf.clone(), dir.clone(), name.clone());
            std::thread::spawn(move || match fetch_ipsw(&from, &dir) {
                Ok(ipsw) => load_and_install(ipsw, conf, dir, disk_gb, name),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    die(e)
                }
            });
        });
        unsafe { VZMacOSRestoreImage::fetchLatestSupportedWithCompletionHandler(&fetched) };
    } else {
        load_and_install(ipsw, conf, dir, disk_gb, name);
    }
    dispatch2::dispatch_main()
}

/// Download `from` (Apple's .ipsw) into the storage folder's iso/, the progress into the VM's
/// `installing` file ("dl:PERCENT"). A file that is there already is used; a part of one from
/// an earlier try is continued. Ends when the VM's folder is gone (deleted in the window).
fn fetch_ipsw(from: &str, vm: &Path) -> anyhow::Result<std::path::PathBuf> {
    let file = from.rsplit('/').next().filter(|n| n.ends_with(".ipsw")).with_context(|| format!("{from}: not a restore image's address"))?;
    let iso = crate::storage().join("iso");
    std::fs::create_dir_all(&iso)?;
    let (done, part) = (iso.join(file), iso.join(format!("{file}.part")));
    if done.exists() {
        eprintln!("{} is here already", done.display());
        return Ok(std::fs::canonicalize(&done).unwrap_or(done));
    }
    let head = std::process::Command::new("/usr/bin/curl").args(["-sIL", "--max-time", "30", from]).output()?;
    let total: u64 = String::from_utf8_lossy(&head.stdout)
        .lines()
        .filter_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse().ok()))
        .last()
        .unwrap_or(0);
    eprintln!("downloading {from} ({:.1} GB) to {}", total as f64 / 1e9, iso.display());
    let mut curl = std::process::Command::new("/usr/bin/curl")
        .args(["-fsSL", "--retry", "5", "--retry-delay", "5", "-C", "-", "-o"])
        .arg(&part)
        .arg(from)
        .spawn()
        .context("curl")?;
    let st = loop {
        if let Some(st) = curl.try_wait()? {
            break st;
        }
        if !vm.join("vm.json").exists() {
            let _ = curl.kill();
            anyhow::bail!("the VM was deleted: download stopped ({} stays for the next try)", part.display());
        }
        let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if total > 0 {
            let _ = std::fs::write(vm.join("installing"), format!("dl:{}", (got * 100 / total).min(99)));
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    };
    anyhow::ensure!(st.success(), "the download from Apple failed (curl: {st}); {} stays, the next try goes on from it", part.display());
    std::fs::rename(&part, &done)?;
    Ok(std::fs::canonicalize(&done).unwrap_or(done))
}

/// Read the restore image and run the installer (any thread; the install itself on the main queue).
fn load_and_install(ipsw: std::path::PathBuf, conf: VmConf, dir: std::path::PathBuf, disk_gb: u64, name: String) {
    eprintln!("reading {}", ipsw.display());
    let ipsw2 = ipsw.clone();
    let loaded = RcBlock::new(move |img: *mut VZMacOSRestoreImage, e: *mut NSError| {
        let Some(img) = (unsafe { Retained::retain(img) }) else {
            // a VM that was listed while its image came down, and has no disk yet: gone again
            if !dir.join("disk.img").exists() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            die(anyhow!("restore image: {}", unsafe { e.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default()));
        };
        let (img, conf, dir, ipsw, name) = (OnMain(img), conf.clone(), dir.clone(), ipsw2.clone(), name.clone());
        DispatchQueue::main().exec_async(move || {
            let img = img; // the whole wrapper, not its field (closures capture fields one by one)
            if let Err(e) = install(&img.0, conf, &dir, &ipsw, disk_gb, &name) {
                let _ = std::fs::remove_dir_all(&dir);
                die(e);
            }
        });
    });
    unsafe { VZMacOSRestoreImage::loadFileURL_completionHandler(&url(&ipsw), &loaded) };
}

/// On the main queue: make the VM's files and run the installer.
fn install(img: &VZMacOSRestoreImage, mut conf: VmConf, dir: &Path, ipsw: &Path, disk_gb: u64, name: &str) -> anyhow::Result<()> {
    let v = unsafe { img.operatingSystemVersion() };
    let req = unsafe { img.mostFeaturefulSupportedConfiguration() }
        .with_context(|| format!("this Mac cannot run macOS {}.{}.{} as a guest", v.majorVersion, v.minorVersion, v.patchVersion))?;
    let hw = unsafe { req.hardwareModel() };
    conf.os_version = Some(match v.patchVersion {
        0 => format!("{}.{}", v.majorVersion, v.minorVersion),
        p => format!("{}.{}.{p}", v.majorVersion, v.minorVersion),
    });
    conf.cpus = conf.cpus.max(unsafe { req.minimumSupportedCPUCount() });
    conf.mem_gb = conf.mem_gb.max(unsafe { req.minimumSupportedMemorySize() }.div_ceil(1 << 30));
    eprintln!(
        "macOS {}.{}.{} ({}): {} cpu, {} GB memory, {} GB disk",
        v.majorVersion,
        v.minorVersion,
        v.patchVersion,
        unsafe { img.buildVersion() },
        conf.cpus,
        conf.mem_gb,
        disk_gb
    );
    std::fs::File::create(dir.join("disk.img"))?.set_len(disk_gb << 30)?;
    let cfg = config_macos(dir, &conf, Some(&hw))?;
    // "installing": the window shows the progress from it; gone = ready to run
    let marker = dir.join("installing");
    std::fs::write(&marker, "0")?;
    std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&conf)?)?;

    let vm = unsafe { VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &cfg) };
    let inst = unsafe { VZMacOSInstaller::initWithVirtualMachine_restoreImageURL(VZMacOSInstaller::alloc(), &vm, &url(ipsw)) };
    let name = name.to_string();
    let (marker2, marker3, dir2) = (marker.clone(), marker.clone(), dir.to_path_buf());
    let done = RcBlock::new(move |e: *mut NSError| match unsafe { e.as_ref() } {
        Some(e) => {
            // a half-installed VM is of no use: remove it (the log keeps the reason)
            let _ = std::fs::remove_dir_all(&dir2);
            die(anyhow!("install failed: {}", e.localizedDescription()))
        }
        None => {
            let _ = std::fs::remove_file(&marker2);
            eprintln!("installed. Next: hypermac run {name}");
            std::process::exit(0);
        }
    });
    unsafe { inst.installWithCompletionHandler(&done) };
    let progress = OnMain(unsafe { inst.progress() });
    std::thread::spawn(move || {
        let p = progress;
        let mut last = -1i64;
        loop {
            let f: f64 = unsafe { msg_send![&*p.0, fractionCompleted] };
            let pct = (f * 100.0) as i64;
            if pct != last {
                last = pct;
                eprintln!("install {pct}%");
                if marker3.exists() {
                    let _ = std::fs::write(&marker3, pct.to_string());
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
    // the installer and the VM live as long as the process
    std::mem::forget(inst);
    std::mem::forget(vm);
    Ok(())
}

/// The console of a VM on our own virtio-gpu: our VNC server (rfb.rs) in the Mac's Screen Sharing.
fn open_vnc_console(password: &str) {
    let port = VNC_PORT.load(Ordering::Relaxed);
    if port == 0 {
        eprintln!("console: VNC server not up yet");
        return;
    }
    let url = format!("vnc://:{password}@127.0.0.1:{port}");
    if let Err(e) = std::process::Command::new("/usr/bin/open").arg(&url).status() {
        eprintln!("console: open {url}: {e}");
    }
}

/// hypermac run NAME [--vnc-port P]: runs until the guest shuts down or `ctl NAME kill`.
pub fn run(name: &str, args: &[String]) -> anyhow::Result<()> {
    let conf = load(name)?;
    let dir = vm_dir(name);
    let sock = dir.join("ctl.sock");
    anyhow::ensure!(std::os::unix::net::UnixStream::connect(&sock).is_err(), "VM {name:?} is already running");
    let _ = std::fs::remove_file(&sock);
    if crate::bhyve::is(&conf) {
        return crate::bhyve::run(name, args, conf);
    }

    let cfg = match conf.os.as_str() {
        "macos" => config_macos(&dir, &conf, None)?,
        "linux" | "windows" => config_linux(&dir, &conf, opt(args, "--iso").or(conf.iso.as_deref()))?,
        os => anyhow::bail!("vm.json: os {os:?}?"),
    };
    let vm = unsafe { VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &cfg) };
    let started = RcBlock::new(|e: *mut NSError| match unsafe { e.as_ref() } {
        Some(e) => die(anyhow!("start failed: {}", e.localizedDescription())),
        None => {
            WAS_RUNNING.store(true, Ordering::Relaxed);
            eprintln!("vm started");
        }
    });
    unsafe { vm.startWithCompletionHandler(&started) };

    // our own virtio-gpu/input (HYPERMAC_VIRTIO=1): our own VNC server (rfb.rs); VZ's only
    // serves VZ's graphics and USB HID
    // the picture for `hypermac serve`'s RDP (shm.rs, rdp.rs)
    if conf.own_devices() {
        if let Err(e) = crate::shm::export(&dir) {
            eprintln!("screen.shm: {e:#}");
        }
    }
    let vnc = if conf.own_devices() {
        let p = crate::rfb::start(num(args, "--vnc-port", 0) as u16, conf.vnc_password.clone())?;
        VNC_PORT.store(p, Ordering::Relaxed);
        eprintln!("console: vnc://127.0.0.1:{p} (own server)");
        None
    } else {
        Some(crate::vnc::Vnc::start(&vm, num(args, "--vnc-port", 0) as u16, &conf.vnc_password)?)
    };
    let vm = std::sync::Arc::new(OnMain(vm));
    let vnc = std::sync::Arc::new(OnMain(vnc));

    // every second on the main queue: the VNC port once it is known; exit when the guest has shut down
    {
        let (vm, vnc, sock) = (vm.clone(), vnc.clone(), sock.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let (vm, vnc, sock) = (vm.clone(), vnc.clone(), sock.clone());
                DispatchQueue::main().exec_async(move || {
                    if let Some(v) = vnc.0.as_ref() {
                        let p = v.port();
                        if p != 0 && VNC_PORT.swap(p, Ordering::Relaxed) == 0 {
                            eprintln!("console: vnc://127.0.0.1:{p}");
                        }
                    }
                    let st = unsafe { vm.0.state() };
                    if WAS_RUNNING.load(Ordering::Relaxed) && (st == VZVirtualMachineState::Stopped || st == VZVirtualMachineState::Error) {
                        eprintln!("vm stopped (state {})", st.0);
                        let _ = std::fs::remove_file(&sock);
                        std::process::exit(0);
                    }
                });
            }
        });
    }

    let l = std::os::unix::net::UnixListener::bind(&sock)?;
    let title = format!("{name} — hyperMAC");
    let conf_wh = (conf.w as f64, conf.h as f64);
    let open_now = args.iter().any(|a| a == "--console");
    let (fb_path, log_path, mem) = (dir.join("fb.bmp"), dir.join("hmlog.txt"), conf.mem_gb << 30);
    let vshot_path = dir.join("vscreen.bmp");
    let own_virtio = conf.own_devices();
    let vnc_password = conf.vnc_password.clone();
    if open_now {
        if own_virtio {
            crate::console::open(&title);
        } else {
            open_console(&vm.0, &title, conf_wh);
        }
    }
    // one thread per connection: hypermac serve's RDP keeps one open for a whole session
    // (vkey/vabs/vbtn), and one at a time left the next session and the app's polling waiting
    // forever (user 2026-11-10: the mouse worked in the first session only)
    let handle = std::sync::Arc::new(move |c: std::os::unix::net::UnixStream| {
        {
            let Ok(mut w) = c.try_clone() else { return };
            for line in BufReader::new(c).lines().map_while(Result::ok) {
                let r = match line.trim() {
                    // own virtio-gpu / virtio-input (HYPERMAC_VIRTIO=1)
                    "vshot" => match crate::vgpu::shot(&vshot_path) {
                        Ok(v) => v,
                        Err(e) => serde_json::json!({"error": e.to_string()}),
                    },
                    // where the guest's hardware cursor is (our virtio-gpu's cursor queue), and the screen
                    "cursor" => match (crate::vgpu::cursor_position(), crate::vgpu::size()) {
                        (Some((x, y)), Some((w, h))) => serde_json::json!({"x": x, "y": y, "w": w, "h": h}),
                        _ => serde_json::json!({"error": "no cursor reported yet"}),
                    },
                    // "vkey LINUX_KEYCODE 1|0", "vabs X Y" (0..1), "vbtn 0|1|2 1|0"
                    l if l.starts_with("vraw ") => {
                        let a: Vec<i64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                        match a.as_slice() {
                            [t, c, v] => serde_json::json!({"ok": true, "sent": crate::vinput::raw(*t as u16, *c as u16, *v as i32)}),
                            _ => serde_json::json!({"error": "vraw TYPE CODE VALUE"}),
                        }
                    }
                    l if l.starts_with("vkey ") || l.starts_with("vabs ") || l.starts_with("vbtn ") => {
                        let a: Vec<&str> = l.split_whitespace().collect();
                        let f = |i: usize| a.get(i).and_then(|x| x.parse::<f64>().ok());
                        let sent = match (a[0], f(1), f(2)) {
                            ("vkey", Some(k), Some(d)) => Some(crate::vinput::key(k as u16, d != 0.0)),
                            ("vabs", Some(x), Some(y)) => Some(crate::vinput::abs(x, y)),
                            ("vbtn", Some(b), Some(d)) => Some(crate::vinput::button(b as u16, d != 0.0)),
                            _ => None,
                        };
                        match sent {
                            Some(n) => serde_json::json!({"ok": true, "sent": n}),
                            None => serde_json::json!({"error": "vkey CODE 1|0 | vabs X Y | vbtn B 1|0"}),
                        }
                    }
                    // guest RAM as hex: "peek ADDR LEN" (hex or decimal)
                    l if l.starts_with("peek ") => {
                        let n = |x: &str| match x.strip_prefix("0x") {
                            Some(h) => u64::from_str_radix(h, 16).ok(),
                            None => x.parse().ok(),
                        };
                        let a: Vec<Option<u64>> = l.split_whitespace().skip(1).map(n).collect();
                        match (a.first().copied().flatten(), a.get(1).copied().flatten()) {
                            (Some(addr), Some(len)) => match crate::fbdev::peek(mem, addr, len as usize) {
                                Ok(v) => v,
                                Err(e) => serde_json::json!({"error": e.to_string()}),
                            },
                            _ => serde_json::json!({"error": "peek ADDR LEN"}),
                        }
                    }
                    "kill" => {
                        let _ = std::fs::remove_file(&sock);
                        std::process::exit(0);
                    }
                    "console" => {
                        if own_virtio {
                            // VZ's view only shows VZ's own graphics: our own window (console.rs)
                            let title = title.clone();
                            DispatchQueue::main().exec_async(move || crate::console::open(&title));
                        } else {
                            let (vm, title) = (vm.clone(), title.clone());
                            DispatchQueue::main().exec_async(move || open_console(&vm.0, &title, conf_wh));
                        }
                        serde_json::json!({"ok": true})
                    }
                    // Windows: save the EFI shim's framebuffer from guest RAM as DIR/fb.bmp
                    "fbshot" => match crate::fbdev::shot(mem, &fb_path) {
                        Ok(v) => v,
                        Err(e) => serde_json::json!({"error": e.to_string()}),
                    },
                    // Windows: the EFI shim's log in guest RAM -> DIR/hmlog.txt
                    "log" => match crate::fbdev::log(mem, &log_path) {
                        Ok(v) => v,
                        Err(e) => serde_json::json!({"error": e.to_string()}),
                    },
                    // freeze / thaw the guest (e.g. at a bugcheck, to read its memory at leisure)
                    "pause" | "resume" => {
                        let (vm, pause) = (vm.clone(), line.trim() == "pause");
                        DispatchQueue::main().exec_async(move || {
                            let done = RcBlock::new(|e: *mut NSError| {
                                if let Some(e) = unsafe { e.as_ref() } {
                                    eprintln!("pause/resume: {}", e.localizedDescription());
                                }
                            });
                            unsafe {
                                if pause {
                                    vm.0.pauseWithCompletionHandler(&done)
                                } else {
                                    vm.0.resumeWithCompletionHandler(&done)
                                }
                            }
                        });
                        serde_json::json!({"ok": true})
                    }
                    // our VNC server in the Mac's Screen Sharing (own_devices)
                    "vnc" => {
                        open_vnc_console(&vnc_password);
                        serde_json::json!({"ok": true})
                    }
                    "stop" => {
                        // the guest's shutdown dialog / ACPI-style request; the process exits when it is down
                        let vm = vm.clone();
                        DispatchQueue::main().exec_async(move || {
                            if let Err(e) = unsafe { vm.0.requestStopWithError() } {
                                eprintln!("stop: {}", e.localizedDescription());
                            }
                        });
                        serde_json::json!({"ok": true})
                    }
                    _ => serde_json::json!({
                        "pid": std::process::id(),
                        "os": conf.os,
                        "vnc_port": VNC_PORT.load(Ordering::Relaxed),
                        "vnc_password": conf.vnc_password,
                        "mac": conf.mac,
                        "net": conf.net.clone().unwrap_or("nat".into()),
                        "ip": crate::guest_ip(&conf),
                        // bridged only: frames relayed each way, and what was lost (vmnet.rs)
                        "net_stats": {
                            "to_guest": crate::vmnet::TO_GUEST.load(Ordering::Relaxed),
                            "to_guest_dropped": crate::vmnet::TO_GUEST_DROPPED.load(Ordering::Relaxed),
                            "to_guest_errno": crate::vmnet::TO_GUEST_ERRNO.load(Ordering::Relaxed),
                            "to_guest_largest": crate::vmnet::TO_GUEST_MAX.load(Ordering::Relaxed),
                            "from_guest": crate::vmnet::FROM_GUEST.load(Ordering::Relaxed),
                            "write_failed": crate::vmnet::WRITE_FAILED.load(Ordering::Relaxed),
                        },
                    }),
                };
                if writeln!(w, "{r}").is_err() {
                    break;
                }
            }
        }
    });
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let h = handle.clone();
            std::thread::spawn(move || h(c));
        }
    });
    // an app without a Dock icon: it has a window only while the console is open
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.run();
    Ok(())
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACConsoleDelegate"]
    struct ConsoleDelegate;

    unsafe impl NSObjectProtocol for ConsoleDelegate {}

    unsafe impl NSWindowDelegate for ConsoleDelegate {
        #[unsafe(method(windowWillClose:))]
        fn will_close(&self, _n: &NSNotification) {
            // the console is gone, the VM is not: back to no Dock icon
            NSApplication::sharedApplication(self.mtm()).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        }
    }
);

thread_local! {
    static CONSOLE: std::cell::RefCell<Option<(Retained<NSWindow>, Retained<ConsoleDelegate>)>> = const { std::cell::RefCell::new(None) };
}

/// The console: a window in the VM's own process with Virtualization.framework's view of the
/// VM's display (keyboard, pointer and the picture go straight to the VM, nothing in between).
/// Opened on request (ctl "console", `run --console`); closing it leaves the VM running.
/// The guest's resolution stays what vm.json says: the view scales it to the window.
fn open_console(vm: &VZVirtualMachine, title: &str, (w, h): (f64, f64)) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    crate::console::set_app_icon(&app);
    let window = CONSOLE.with(|c| c.borrow().as_ref().map(|(w, _)| w.clone()));
    let window = match window {
        Some(w) => w,
        None => {
            // as large as the guest's display, or what fits on this screen at the same shape
            let (sw, sh) = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame().size).map(|s| (s.width, s.height)).unwrap_or((1440.0, 900.0));
            let k = (sw * 0.9 / w).min((sh * 0.9 - 30.0) / h).min(1.0);
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new((w * k).round(), (h * k).round())),
                    NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setContentAspectRatio(NSSize::new(w, h));
            let view = unsafe { VZVirtualMachineView::new(mtm) };
            unsafe {
                view.setVirtualMachine(Some(vm));
                // not capturing system keys: that needs the Accessibility grant for this binary, and
                // without it the modifiers went missing (user 2026-11-10: Shift+D gave no capital D
                // in a Debian guest). The price: Cmd-Tab and the like stay with the host.
                view.setCapturesSystemKeys(false);
            }
            window.setContentView(Some(&view));
            window.makeFirstResponder(Some(&view));
            let delegate: Retained<ConsoleDelegate> = unsafe { msg_send![super(ConsoleDelegate::alloc(mtm).set_ivars(())), init] };
            window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            window.center();
            CONSOLE.with(|c| *c.borrow_mut() = Some((window.clone(), delegate)));
            window
        }
    };
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

/// hypermac create NAME --os linux [--iso PATH] [--cpus N] [--mem-gb G] [--disk-gb G] [--w W --h H]
/// Only makes the VM's files; `run` boots it (from the ISO while vm.json names one and the disk is empty).
pub(crate) fn create_linux(name: &str, args: &[String]) -> anyhow::Result<()> {
    let dir = vm_dir(name);
    anyhow::ensure!(!dir.join("vm.json").exists(), "VM {name:?} exists ({})", dir.display());
    let abs = |o: Option<&str>| -> anyhow::Result<Option<String>> {
        match o {
            Some(p) => {
                let p = std::fs::canonicalize(p).with_context(|| format!("{p}: no such file"))?;
                Ok(Some(p.to_string_lossy().into_owned()))
            }
            None => Ok(None),
        }
    };
    let iso = abs(opt(args, "--iso"))?;
    let drivers = abs(opt(args, "--drivers"))?;
    let windows = opt(args, "--os") == Some("windows");
    // FreeBSD runs on the bhyve engine only (Virtualization.framework's EFI + virtio set was
    // never tried with it; bhyve is its own hypervisor anyway)
    let freebsd = opt(args, "--os") == Some("freebsd");
    if freebsd {
        anyhow::ensure!(crate::bhyve::program().is_some(), "FreeBSD VMs need the bhyve engine: run build.sh (it builds vm/bhyve)");
    }
    std::fs::create_dir_all(&dir)?;
    let mut pw = [0u8; 4];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut pw))?;
    let conf = VmConf {
        os: if windows { "windows" } else if freebsd { "freebsd" } else { "linux" }.into(),
        cpus: num(args, "--cpus", 4) as usize,
        mem_gb: num(args, "--mem-gb", if windows { 8 } else { 4 }),
        w: num(args, "--w", 1920) as isize,
        h: num(args, "--h", 1080) as isize,
        mac: unsafe { VZMACAddress::randomLocallyAdministeredAddress().string() }.to_string(),
        vnc_password: pw.iter().map(|b| format!("{b:02x}")).collect(),
        iso,
        drivers,
        // our own virtio-gpu/input (2026-11-10): the console is our window then, which takes the
        // events hyperMAC's RDP posts on the host (VZ's own view ignored them: no mouse when
        // reached over kRDPclient), and kRDPclient can open the VM itself (screen.shm);
        // --vz-devices for VZ's graphics and USB HID
        own_devices: !args.iter().any(|a| a == "--vz-devices"),
        net: opt(args, "--net").filter(|n| *n != "nat").map(str::to_string),
        os_version: opt(args, "--os-version").map(str::to_string),
        engine: freebsd.then(|| "bhyve".to_string()),
    };
    std::fs::File::create(dir.join("disk.img"))?.set_len(num(args, "--disk-gb", if windows { 64 } else { 32 }) << 30)?;
    unsafe {
        let id = VZGenericMachineIdentifier::new();
        std::fs::write(dir.join("machine-id"), id.dataRepresentation().to_vec())?;
        VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
            VZEFIVariableStore::alloc(),
            &url(&dir.join("nvram")),
            VZEFIVariableStoreInitializationOptions::empty(),
        )
        .map_err(err)?;
    }
    std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&conf)?)?;
    eprintln!("created. Next: hypermac run {name}");
    Ok(())
}

/// Linux: EFI, virtio disk, the ISO as a USB stick, VZ's virtio-gpu (2D: the guest renders on the
/// CPU), USB keyboard and absolute pointer (the VNC console feeds them), serial console -> console.log.
fn config_linux(dir: &Path, c: &VmConf, iso: Option<&str>) -> anyhow::Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
        let cfg = VZVirtualMachineConfiguration::new();
        cfg.setCPUCount(c.cpus);
        cfg.setMemorySize(c.mem_gb << 30);

        let p = VZGenericPlatformConfiguration::new();
        let id = VZGenericMachineIdentifier::initWithDataRepresentation(
            VZGenericMachineIdentifier::alloc(),
            &NSData::with_bytes(&std::fs::read(dir.join("machine-id")).context("machine-id")?),
        )
        .context("bad machine-id")?;
        p.setMachineIdentifier(&id);
        cfg.setPlatform(&p);

        let boot = VZEFIBootLoader::new();
        boot.setVariableStore(Some(&VZEFIVariableStore::initWithURL(VZEFIVariableStore::alloc(), &url(&dir.join("nvram")))));
        cfg.setBootLoader(Some(&boot));

        let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &url(&dir.join("disk.img")),
            false,
        )
        .map_err(err)?;
        // Windows has no virtio-blk driver out of the box but does have NVMe
        let mut storage: Vec<Retained<VZStorageDeviceConfiguration>> = if c.os == "windows" {
            vec![Retained::into_super(VZNVMExpressControllerDeviceConfiguration::initWithAttachment(
                VZNVMExpressControllerDeviceConfiguration::alloc(),
                &att,
            ))]
        } else {
            vec![Retained::into_super(VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), &att))]
        };
        for iso in iso.into_iter().chain(c.drivers.as_deref()) {
            let a = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url(Path::new(iso)),
                // a .iso is read-only; a made-up boot stick (.img, Windows) is a real stick
                !iso.ends_with(".img"),
            )
            .map_err(err)?;
            let usb = VZUSBMassStorageDeviceConfiguration::initWithAttachment(VZUSBMassStorageDeviceConfiguration::alloc(), &a);
            storage.push(Retained::into_super(usb));
        }
        cfg.setStorageDevices(&NSArray::from_retained_slice(&storage));

        let gpu = VZVirtioGraphicsDeviceConfiguration::new();
        let so = VZVirtioGraphicsScanoutConfiguration::initWithWidthInPixels_heightInPixels(
            VZVirtioGraphicsScanoutConfiguration::alloc(),
            c.w,
            c.h,
        );
        gpu.setScanouts(&NSArray::from_retained_slice(&[so]));
        let mut gpus: Vec<Retained<VZGraphicsDeviceConfiguration>> = Vec::new();
        // Experiment (Windows): VZ's EFI draws nothing on virtio-gpu; try the private linear
        // framebuffer device (a plain framebuffer, like QEMU's ramfb) in front of it.
        // On macOS 27.2 with a generic platform the VM then fails to start ("Internal Virtualization error").
        if c.os == "windows" && std::env::var_os("HYPERMAC_LFB").is_some() {
            if let Some(cls) = objc2::runtime::AnyClass::get(c"_VZLinearFramebufferGraphicsDeviceConfiguration") {
                let a: objc2::rc::Allocated<VZGraphicsDeviceConfiguration> = msg_send![cls, alloc];
                let size: u64 = ((c.w * c.h * 4) as u64).next_multiple_of(1 << 20);
                let l: Retained<VZGraphicsDeviceConfiguration> = msg_send![a, initWithBackingStoreSize: size];
                eprintln!("linear framebuffer: {size} bytes");
                gpus.push(l);
            }
        }
        // HYPERMAC_VIRTIO=1: our own virtio-gpu and virtio-input (vgpu.rs, vinput.rs) instead of
        // VZ's graphics; the picture and the input go through this process (ctl vshot / vkey...)
        let own_virtio = c.own_devices();
        if !own_virtio && (c.os != "windows" || std::env::var_os("HYPERMAC_NO_VIRTIO_GPU").is_none()) {
            gpus.push(Retained::into_super(gpu));
        }
        cfg.setGraphicsDevices(&NSArray::from_retained_slice(&gpus));

        let mut custom: Vec<Retained<objc2::runtime::AnyObject>> = Vec::new();
        if own_virtio {
            custom.push(crate::vgpu::configuration(c.w as u32, c.h as u32)?);
            custom.extend(crate::vinput::configurations(c.os == "windows")?);
            eprintln!("own virtio-gpu {}x{} + virtio-input (keyboard, pointer)", c.w, c.h);
        }
        // Windows: a do-nothing virtio device whose only use is mapping guest RAM, so the host
        // can read the EFI shim's linear framebuffer (fbdev.rs)
        if c.os == "windows" {
            if let Some(d) = crate::fbdev::configuration() {
                custom.push(d);
            }
        }
        if !custom.is_empty() {
            let _: () = msg_send![&*cfg, setCustomVirtioDevices: &*NSArray::from_retained_slice(&custom)];
        }

        // Debugging the guest (private API): HYPERMAC_GDB=PORT opens VZ's GDB stub on
        // 127.0.0.1:PORT; `lldb` -> `gdb-remote PORT` shows where the vCPUs are.
        if let Some(port) = std::env::var("HYPERMAC_GDB").ok().and_then(|p| p.parse::<u16>().ok()) {
            if let Some(cls) = objc2::runtime::AnyClass::get(c"_VZGDBDebugStubConfiguration") {
                let a: objc2::rc::Allocated<NSObject> = msg_send![cls, alloc];
                let stub: Retained<NSObject> = msg_send![a, initWithPort: port as u64];
                let _: () = msg_send![&*cfg, _setDebugStub: &*stub];
                eprintln!("gdb stub on 127.0.0.1:{port}");
            }
        }

        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setAttachment(Some(&*net_attachment(c)?));
        let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&c.mac)).context("bad mac in vm.json")?;
        net.setMACAddress(&mac);
        cfg.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        cfg.setKeyboards(&NSArray::from_retained_slice(&[Retained::into_super(VZUSBKeyboardConfiguration::new())]));
        cfg.setPointingDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZUSBScreenCoordinatePointingDeviceConfiguration::new(),
        )]));
        cfg.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(VZVirtioEntropyDeviceConfiguration::new())]));

        let snd = VZVirtioSoundDeviceConfiguration::new();
        let out = VZVirtioSoundDeviceOutputStreamConfiguration::new();
        snd.setStreams(&NSArray::from_retained_slice(&[Retained::into_super(out)]));
        cfg.setAudioDevices(&NSArray::from_retained_slice(&[Retained::into_super(snd)]));

        let ser = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let att = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &url(&dir.join("console.log")),
            true,
        )
        .map_err(err)?;
        ser.setAttachment(Some(&att));
        // Windows: no virtio-console (virtio-win's vioser.sys binds to VZ's and bugchecks 0x7E,
        // a NULL read at +0x2be8; Windows has no use for it anyway)
        let mut ports: Vec<Retained<VZSerialPortConfiguration>> =
            if c.os == "windows" { vec![] } else { vec![Retained::into_super(ser)] };
        // A real UART (private VZ classes): HYPERMAC_UART=pl011|16550 -> DIR/uart.log.
        // The firmware and Windows (boot debugging, EMS, KD) know these; nobody knows virtio-console.
        if let Ok(kind) = std::env::var("HYPERMAC_UART") {
            let cls = match kind.as_str() {
                "16550" => c"_VZ16550SerialPortConfiguration",
                _ => c"_VZPL011SerialPortConfiguration",
            };
            if let Some(cls) = objc2::runtime::AnyClass::get(cls) {
                let u: Retained<VZSerialPortConfiguration> = msg_send![cls, new];
                let a = VZFileSerialPortAttachment::initWithURL_append_error(
                    VZFileSerialPortAttachment::alloc(),
                    &url(&dir.join("uart.log")),
                    true,
                )
                .map_err(err)?;
                u.setAttachment(Some(&a));
                ports.push(u);
                eprintln!("uart: {kind} -> uart.log");
            }
        }
        cfg.setSerialPorts(&NSArray::from_retained_slice(&ports));

        cfg.validateWithError().map_err(err)?;
        Ok(cfg)
    }
}
