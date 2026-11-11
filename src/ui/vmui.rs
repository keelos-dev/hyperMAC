//! The main window's Virtual Machines tab (user 2026-11-10: "2个tab：一个是host设置，一个是vm列表，
//! 安装等等"): hyperMAC's VMs with their state, New / Start / Stop / Console / Bridged / Eject
//! Installer / Delete. Moved here from the VM program's own window (vm/src/ui.rs).
//!
//! The VMs are the VM program's processes (`hypermac-vm run NAME`, one per VM, started detached:
//! they outlive this window and the service); this tab starts them, creates new ones
//! (`hypermac-vm create`), and talks to running ones over their ctl.sock.

use std::path::PathBuf;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSBackingStoreType, NSStackViewDistribution, NSWindow, NSWindowStyleMask, NSAlert, NSAlertStyle, NSColor, NSFontWeightMedium, NSFontWeightRegular, NSOpenPanel, NSSecureTextField, NSStackView, NSTextField, NSView};
use objc2_foundation::{NSEdgeInsets, NSPoint, NSRect, NSSize};

use super::{as_view, button, copy, fixed_size, hstack, insets, label, mtm, render, row, same_width, section, spacer, switch, with, wrapping};
use crate::gui::{ns, t, zh};
use crate::vms::{guests_dir, root, storage, vm_dir};

/// One VM as the tab shows it.
#[derive(Clone, Debug)]
pub struct Vm {
    name: String,
    /// vm.json
    conf: serde_json::Value,
    /// ctl.sock's answer when the VM runs
    info: Option<serde_json::Value>,
    /// while `create` is still at work: macOS's install progress ("45"), or where a Windows
    /// install is (`win:stick`, `win:phase1`, `win:phase2`, `win:failed:WHY`; vm/src/winsetup.rs)
    installing: Option<String>,
}

fn vms() -> Vec<Vm> {
    let mut v: Vec<Vm> = std::fs::read_dir(guests_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let conf: serde_json::Value = serde_json::from_slice(&std::fs::read(e.path().join("vm.json")).ok()?).ok()?;
            let installing = std::fs::read_to_string(e.path().join("installing")).ok().map(|s| s.trim().to_string());
            // a Windows install runs the VM meanwhile (its Console works); macOS's has no ctl.sock
            let info = ctl_request(&name, "info").ok();
            Some(Vm { name, conf, info, installing })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// What the tab shows that changes behind its back (for the window's refresh timer).
pub fn snapshot() -> String {
    let starting = with(|u| {
        // give up on a start after 20 s (the row goes back to "stopped"; run.log says why)
        u.starting.retain(|(_, at)| at.elapsed().as_secs() < 20);
        u.starting.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
    });
    // ip and state only: the rest of info (frame counters) changes all the time
    let list: Vec<String> = vms()
        .iter()
        .map(|v| {
            let ip = v.info.as_ref().and_then(|i| i.get("ip")).cloned();
            format!("{}{}{:?}{:?}{}", v.name, v.conf, v.installing, ip, v.info.is_some())
        })
        .collect();
    format!("{list:?}{starting:?}")
}

/// One request to a running VM's ctl.sock, its JSON answer (1 s at most).
fn ctl_request(name: &str, cmd: &str) -> anyhow::Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect(vm_dir(name).join("ctl.sock"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
    writeln!(s, "{cmd}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn str_of<'a>(c: &'a serde_json::Value, k: &str) -> Option<&'a str> {
    c.get(k).and_then(|v| v.as_str())
}

fn num_of(c: &serde_json::Value, k: &str) -> u64 {
    c.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Change vm.json (the VM is stopped: it reads it at its next start).
fn edit(name: &str, f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) {
    let p = vm_dir(name).join("vm.json");
    if let Some(mut c) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
        if let Some(m) = c.as_object_mut() {
            f(m);
            if let Ok(j) = serde_json::to_vec_pretty(&c) {
                let _ = std::fs::write(&p, j);
            }
        }
    }
}

// ---- the tab --------------------------------------------------------------------------------

pub fn page(root_stack: &NSStackView, width: f64) {
    let list = vms();
    with(|u| u.starting.retain(|(n, _)| list.iter().any(|v| &v.name == n && v.info.is_none())));
    let running = list.iter().filter(|v| v.info.is_some()).count();

    let status = if zh() {
        format!("{} 台虚拟机，{} 台在运行", list.len(), running)
    } else {
        format!("{} virtual machines, {} running", list.len(), running)
    };
    let dot_color = if running > 0 { NSColor::systemGreenColor() } else { NSColor::tertiaryLabelColor() };
    let dot = label("●", 11.0, unsafe { NSFontWeightRegular }, &dot_color);
    let status_l = label(&status, 12.0, unsafe { NSFontWeightMedium }, &NSColor::secondaryLabelColor());
    let new = button(t("新建虚拟机…", "New Virtual Machine…"), new_vm);
    let head = hstack(&[&dot, &status_l, &spacer(), &new], 6.0);
    root_stack.addArrangedSubview(&head);
    same_width(&head, root_stack, insets(root_stack));

    if runner().is_none() {
        let p = wrapping(
            t(
                "找不到虚拟机程序（hypermac-vm）：重新安装 hyperMAC（源码安装则重新运行 build.sh）。",
                "The VM program (hypermac-vm) is missing: install hyperMAC again (from source: run build.sh again).",
            ),
            13.0,
            &NSColor::systemOrangeColor(),
            width - 40.0,
        );
        root_stack.addArrangedSubview(&p);
    }
    if list.is_empty() {
        let p = wrapping(
            t(
                "还没有虚拟机。点「新建虚拟机…」，选一个 macOS 的 .ipsw 恢复映像或 Linux 的 .iso 安装盘。",
                "No virtual machines yet. Click New Virtual Machine… and choose a macOS restore image (.ipsw) or a Linux installer (.iso).",
            ),
            13.0,
            &NSColor::secondaryLabelColor(),
            width - 40.0,
        );
        root_stack.addArrangedSubview(&p);
    } else {
        let rows: Vec<Retained<NSView>> = list.iter().map(vm_row).collect();
        section(
            root_stack,
            t("虚拟机", "VIRTUAL MACHINES"),
            &rows,
            Some(t(
                "虚拟机在宿主上不开窗口，关掉这个窗口、重启服务它们照样运行。kRDPclient 连这台 Mac 时，每台虚拟机多显示一项；「控制台」用于安装和登录窗。",
                "The VMs show no window on this Mac and keep running when this window closes or the service restarts. kRDPclient lists each VM beside this Mac; Console is for installing and the login window.",
            )),
        );
    }
    // where they are kept (vms.rs `storage`)
    let at = storage();
    let change = button(t("更改…", "Change…"), move || change_storage(running));
    let sub = match free_gb(&at) {
        Some(g) if zh() => format!("可用 {g} GB"),
        Some(g) => format!("{g} GB free"),
        None => t("这个位置现在不在（磁盘没接上？）", "This folder is not there now (is the disk attached?)").into(),
    };
    section(
        root_stack,
        t("存储位置", "STORAGE"),
        &[row(Some(("externaldrive", NSColor::secondaryLabelColor())), &at.display().to_string(), Some(&sub), Some(&change))],
        Some(t(
            "里面是 guests（每台虚拟机一个文件夹）和 iso（安装盘，新建向导会列出里面的文件）。更改位置不会搬动已有的虚拟机：要带过去的话，先停机，把它在 guests 里的文件夹移到新位置的 guests 里。",
            "It holds guests (one folder per virtual machine) and iso (installers; the New Virtual Machine window lists the files in it). Changing it does not move the machines there are: to take one along, stop it and move its folder from guests into the new place's guests.",
        )),
    );
}

/// Free space of the volume `p` is on, in GB; None when the folder is not there.
fn free_gb(p: &std::path::Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_bsize as u64 / 1_000_000_000)
}

/// "Change…": another folder for the VMs and the installers. Not while a VM runs (it would
/// vanish from the list and from kRDPclient, still running).
fn change_storage(running: usize) {
    if running > 0 {
        alert(
            t("先停止运行中的虚拟机", "Stop the running virtual machines first"),
            t("更改存储位置后，列表只显示新位置里的虚拟机。", "After the change the list shows only the machines of the new place."),
            &[t("好", "OK")],
            None,
            false,
        );
        return;
    }
    let panel = NSOpenPanel::openPanel(mtm());
    panel.setMessage(Some(&ns(t("选一个文件夹放虚拟机和安装盘", "Choose a folder for the virtual machines and the installers"))));
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setCanCreateDirectories(true);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != 1 {
        return;
    }
    let Some(dir) = panel.URL().and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string())) else { return };
    // a running VM's sockets are DIR/guests/NAME/ctl.sock and the like: a Unix socket's path
    // holds 103 bytes, leave room for the VM's name and the file's
    if dir.as_os_str().len() > 50 {
        alert(
            t("这个路径太长", "This path is too long"),
            t("虚拟机运行时的通信文件放在它的文件夹里，路径有长度上限。请选一个路径更短的文件夹（50 个字符以内）。", "A running machine keeps its sockets in its folder, and their path has a limit. Choose a folder with a shorter path (50 characters at most)."),
            &[t("好", "OK")],
            None,
            true,
        );
        return;
    }
    let made = std::fs::create_dir_all(dir.join("guests")).and_then(|_| std::fs::create_dir_all(dir.join("iso")));
    let saved = made.map_err(anyhow::Error::from).and_then(|_| crate::settings::set_str(&super::state(), "vm_dir", &dir.to_string_lossy()));
    if let Err(e) = saved {
        alert(t("没能更改存储位置", "The storage folder was not changed"), &format!("{e:#}"), &[t("好", "OK")], None, true);
    }
    render();
}

fn vm_row(vm: &Vm) -> Retained<NSView> {
    let c = &vm.conf;
    let bridged = str_of(c, "net").is_some();
    let os_key = str_of(c, "os").unwrap_or("linux");
    let (icon, color) = match os_key {
        "macos" => ("apple.logo", NSColor::systemGrayColor()),
        "windows" => ("pc", NSColor::systemBlueColor()),
        "freebsd" => ("server.rack", NSColor::systemRedColor()),
        _ => ("terminal.fill", NSColor::systemOrangeColor()),
    };
    let os = match os_key {
        "macos" => "macOS",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        _ => "Linux",
    };
    let (cpus, mem) = (num_of(c, "cpus"), num_of(c, "mem_gb"));
    let spec = if zh() { format!("{os} · {cpus} 核 · {mem} GB") } else { format!("{os} · {cpus} CPU · {mem} GB") };
    let name = vm.name.clone();
    let starting = with(|u| u.starting.iter().any(|(n, _)| n == &vm.name));

    let (state, controls): (String, Vec<Retained<NSView>>) = if let Some(what) = &vm.installing {
        let mut ctl: Vec<Retained<NSView>> = Vec::new();
        if vm.info.is_some() {
            let n = name.clone();
            ctl.push(as_view(&button(t("控制台", "Console"), move || {
                let _ = ctl_request(&n, "console");
            })));
        }
        let failed = what.starts_with("win:failed:");
        if what.starts_with("win:") {
            let n = name.clone();
            let title = if failed { t("删除…", "Delete…") } else { t("取消安装…", "Cancel Install…") };
            ctl.push(as_view(&button(title, move || delete(&n))));
        }
        (install_state(what), ctl)
    } else if let Some(info) = &vm.info {
        let ip = info.get("ip").and_then(|v| v.as_str()).map(str::to_string);
        let mut ctl: Vec<Retained<NSView>> = Vec::new();
        if let Some(ip) = ip.clone() {
            ctl.push(as_view(&button(t("拷贝地址", "Copy Address"), move || copy(ip.clone()))));
        }
        // a macOS guest serves its own desktop (hyperMAC installed in it): pair with it once, and
        // kRDPclient opens it from this Mac's list (vms.rs "a guest that serves its own desktop")
        if os_key == "macos" && ip.is_some() && !crate::vms::auto_paired(&name) {
            let n = name.clone();
            if crate::vms::paired(&name) {
                ctl.push(as_view(&button(t("取消远程配对", "Unpair Remote"), move || {
                    crate::vms::unpair(&n);
                    render();
                })));
            } else {
                ctl.push(as_view(&button(t("远程配对…", "Pair for Remote…"), move || {
                    if let Some((user, password)) = crate::gui::pair_dialog(mtm(), &n) {
                        if let Err(e) = crate::vms::pair(&n, &user, &password) {
                            alert(t("没能配对", "Pairing failed"), &format!("{e:#}"), &[t("好", "OK")], None, true);
                        }
                    }
                    render();
                })));
            }
        }
        let n = name.clone();
        ctl.push(as_view(&button(t("控制台", "Console"), move || {
            // the VM's own process opens the window
            let _ = ctl_request(&n, "console");
        })));
        // pause: the machine stands still in memory. suspend (where the engine can): its memory
        // goes to a file, the process ends, the next start goes on from there
        let paused = info.get("paused").and_then(|v| v.as_bool()).unwrap_or(false);
        let n = name.clone();
        ctl.push(as_view(&button(if paused { t("继续", "Resume") } else { t("暂停", "Pause") }, move || {
            let _ = ctl_request(&n, if paused { "resume" } else { "pause" });
            render();
        })));
        if info.get("can_suspend").and_then(|v| v.as_bool()).unwrap_or(false) {
            let n = name.clone();
            ctl.push(as_view(&button(t("挂起", "Suspend"), move || {
                let _ = ctl_request(&n, "suspend");
                render();
            })));
        }
        let n = name.clone();
        ctl.push(as_view(&button(t("停止…", "Stop…"), move || stop(&n))));
        let net = if bridged { t("桥接", "bridged") } else { "NAT" };
        let running = if paused { t("已暂停", "Paused") } else { t("运行中", "Running") };
        let state = match ip {
            Some(ip) => format!("{running} · {ip}（{net}）"),
            None => format!("{running} · {}（{net}）", t("还没有地址", "no address yet")),
        };
        (state, ctl)
    } else if starting {
        (t("正在启动…", "Starting…").to_string(), vec![])
    } else {
        let n1 = name.clone();
        let sw = switch(bridged, move |on| {
            edit(&n1, |m| {
                if on {
                    m.insert("net".into(), "bridged".into());
                } else {
                    m.remove("net");
                }
            });
            render();
        });
        let sw_l = label(t("桥接", "Bridged"), 11.0, unsafe { NSFontWeightRegular }, &NSColor::secondaryLabelColor());
        let (n2, n3, n4) = (name.clone(), name.clone(), name.clone());
        let mut ctl = vec![as_view(&hstack(&[&sw_l, &sw], 4.0))];
        let has_iso = str_of(c, "iso").is_some();
        if has_iso {
            // the installer is plugged in at every start until it is ejected
            ctl.push(as_view(&button(t("弹出安装盘", "Eject Installer"), move || {
                edit(&n4, |m| {
                    m.remove("iso");
                });
                render();
            })));
        }
        let n5 = name.clone();
        ctl.push(as_view(&button(t("编辑…", "Edit…"), move || edit_vm(&n5))));
        ctl.push(as_view(&button(t("删除…", "Delete…"), move || delete(&n2))));
        // suspended: its memory is in a file; Start goes on from there (vm/src/vm.rs SAVED)
        let suspended = vm_dir(&name).join("suspend.vzvmsave").exists();
        ctl.push(as_view(&button(if suspended { t("继续", "Resume") } else { t("启动", "Start") }, move || start(&n3))));
        let state = if suspended {
            t("已挂起", "Suspended")
        } else if has_iso {
            t("已停止 · 插着安装盘", "Stopped · installer attached")
        } else {
            t("已停止", "Stopped")
        };
        (state.to_string(), ctl)
    };
    let refs: Vec<&NSView> = controls.iter().map(|v| &**v).collect();
    let acc = hstack(&refs, 8.0);
    row(Some((icon, color)), &vm.name, Some(&format!("{spec} · {state}")), Some(&acc))
}

// ---- actions --------------------------------------------------------------------------------

/// The VM program: <state>/bin/hypermac-vm (build.sh installs it, signed with the
/// virtualization entitlement), else beside this executable.
fn runner() -> Option<PathBuf> {
    let installed = super::state().join("bin/hypermac-vm");
    if installed.exists() {
        return Some(installed);
    }
    let beside = std::env::current_exe().ok()?.parent()?.join("hypermac-vm");
    beside.exists().then_some(beside)
}

/// `hypermac-vm ARGS` as its own session, output to `log`: it outlives this window.
fn spawn_detached(args: &[&str], log: &std::path::Path, env: &[(&str, &str)]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = runner().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "hypermac-vm is not installed"))?;
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).envs(env.iter().copied()).stdin(std::process::Stdio::null()).stdout(out.try_clone()?).stderr(out);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

/// A modal alert; returns the index of the button pressed (0 = the first = the default one).
fn alert(title: &str, text: &str, buttons: &[&str], accessory: Option<&NSView>, warning: bool) -> usize {
    let a = NSAlert::new(mtm());
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(text));
    if warning {
        a.setAlertStyle(NSAlertStyle::Warning);
    }
    for b in buttons {
        a.addButtonWithTitle(&ns(b));
    }
    if let Some(v) = accessory {
        a.setAccessoryView(Some(v));
        a.window().setInitialFirstResponder(Some(v));
    }
    // NSAlertFirstButtonReturn = 1000, the second 1001, ...
    (a.runModal() - 1000).max(0) as usize
}

fn start(name: &str) {
    match spawn_detached(&["run", name], &vm_dir(name).join("run.log"), &[]) {
        Ok(()) => with(|u| u.starting.push((name.to_string(), std::time::Instant::now()))),
        Err(e) => {
            alert(t("启动失败", "Could not start"), &e.to_string(), &["OK"], None, true);
        }
    }
    render();
}

fn stop(name: &str) {
    let text = t(
        "「关机」请求虚拟机里的系统关机（macOS 会在虚拟机里弹确认框）。「强制关闭」等于拔电源，没保存的东西会丢。",
        "Shut Down asks the guest system to shut down (macOS shows its confirmation in the guest). Force Off is pulling the plug: unsaved work is lost.",
    );
    let title = if zh() { format!("停止「{name}」？") } else { format!("Stop “{name}”?") };
    match alert(&title, text, &[t("关机", "Shut Down"), t("强制关闭", "Force Off"), t("取消", "Cancel")], None, false) {
        0 => {
            let _ = ctl_request(name, "stop");
        }
        1 => {
            let _ = ctl_request(name, "kill");
        }
        _ => {}
    }
    render();
}

fn delete(name: &str) {
    let title = if zh() { format!("删除「{name}」？") } else { format!("Delete “{name}”?") };
    let text = t("它的磁盘和所有文件都会删掉，不能恢复。", "Its disk and all its files are removed. This cannot be undone.");
    if alert(&title, text, &[t("取消", "Cancel"), t("删除", "Delete")], None, true) == 1 {
        // still running (a Windows install in progress): off first; its `create` sees the
        // directory gone and stops
        if ctl_request(name, "kill").is_ok() {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if let Err(e) = std::fs::remove_dir_all(vm_dir(name)) {
            alert(t("删除失败", "Could not delete"), &e.to_string(), &["OK"], None, true);
        }
    }
    render();
}


// ---- New Virtual Machine: the wizard --------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Os {
    Mac,
    Windows,
    Linux,
    FreeBsd,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    Os,
    Image,
    Settings,
    /// not a step of New Virtual Machine: the same window as a stopped VM's settings (`edit_vm`)
    Edit,
}

/// The wizard's state while it is open (Ui.wizard). It has a window of its own (user
/// 2026-11-11), so the list behind it keeps refreshing. Text fields are read into the strings
/// before any re-render of it (`save_fields`).
pub struct Wizard {
    window: Retained<NSWindow>,
    step: Step,
    os: Os,
    image: Option<PathBuf>,
    virtio: Option<PathBuf>,
    name: String,
    cpus: String,
    mem: String,
    disk: String,
    bridged: bool,
    /// Windows: install by itself with the account below; off = the user installs in the Console
    unattended: bool,
    user: String,
    password: String,
    note: String,
    fields: Vec<(&'static str, Retained<NSTextField>)>,
    /// Step::Edit: the VM, its display size, its disk's size now, and the installer to plug in
    /// (Some(None) = eject)
    edit: String,
    w: String,
    h: String,
    disk_now: u64,
    iso: Option<Option<PathBuf>>,
}

fn wiz<R>(f: impl FnOnce(&mut Wizard) -> R) -> Option<R> {
    with(|u| u.wizard.as_mut().map(f))
}

/// New Virtual Machine…: the wizard, in a window of its own.
fn new_vm() {
    // open already: bring it forward
    if let Some(w) = wiz(|w| w.window.clone()) {
        w.makeKeyAndOrderFront(None);
        return;
    }
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm()),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(super::MAIN_WIDTH, 400.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns(t("新建虚拟机", "New Virtual Machine")));
    with(|u| window.setDelegate(Some(ProtocolObject::from_ref(&*u.target))));
    let user: String = crate::account::mac_user().chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).take(20).collect();
    with(|u| {
        u.wizard = Some(Wizard {
            window: window.clone(),
            step: Step::Os,
            os: Os::Mac,
            image: None,
            virtio: None,
            name: String::new(),
            cpus: String::new(),
            mem: String::new(),
            disk: String::new(),
            bridged: true,
            unattended: false,
            user: if user.is_empty() { "win".into() } else { user },
            password: String::new(),
            note: String::new(),
            fields: Vec::new(),
            edit: String::new(),
            w: String::new(),
            h: String::new(),
            disk_now: 0,
            iso: None,
        })
    });
    render_wizard();
    window.center();
    window.makeKeyAndOrderFront(None);
}

/// Edit…: a stopped VM's settings, in the wizard's window (one of the two at a time).
fn edit_vm(name: &str) {
    if wiz(|_| ()).is_some() {
        close_wizard();
    }
    let Some(c) = std::fs::read(vm_dir(name).join("vm.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else { return };
    new_vm();
    let disk_now = std::fs::metadata(vm_dir(name).join("disk.img")).map(|m| m.len() >> 30).unwrap_or(0);
    let window = wiz(|w| {
        w.step = Step::Edit;
        w.edit = name.to_string();
        w.os = match str_of(&c, "os") {
            Some("macos") => Os::Mac,
            Some("windows") => Os::Windows,
            Some("freebsd") => Os::FreeBsd,
            _ => Os::Linux,
        };
        w.cpus = num_of(&c, "cpus").to_string();
        w.mem = num_of(&c, "mem_gb").to_string();
        w.w = num_of(&c, "w").to_string();
        w.h = num_of(&c, "h").to_string();
        w.disk = disk_now.to_string();
        w.disk_now = disk_now;
        w.bridged = str_of(&c, "net").is_some();
        w.image = str_of(&c, "iso").map(PathBuf::from);
        w.window.clone()
    });
    if let Some(window) = window {
        window.setTitle(&ns(&if zh() { format!("编辑「{name}」") } else { format!("Edit “{name}”") }));
    }
    render_wizard();
}

fn step_edit(root_stack: &NSStackView, width: f64) {
    let Some((name, os, cpus, mem, disk, disk_now, w_, h_, bridged, image, iso, note)) = wiz(|w| {
        (w.edit.clone(), w.os, w.cpus.clone(), w.mem.clone(), w.disk.clone(), w.disk_now, w.w.clone(), w.h.clone(), w.bridged, w.image.clone(), w.iso.clone(), w.note.clone())
    }) else {
        return;
    };
    let bhyve = std::fs::read(vm_dir(&name).join("vm.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|c| str_of(&c, "engine") == Some("bhyve"));
    let net = switch(bridged, |on| {
        wiz(|w| w.bridged = on);
    });
    let size = hstack(&[&*text_field("w", &w_, 60.0, false), &*label("×", 13.0, unsafe { NSFontWeightRegular }, &NSColor::secondaryLabelColor()), &*text_field("h", &h_, 60.0, false)], 6.0);
    let mut rows = vec![
        row(None, t("CPU 核数", "CPUs"), None, Some(&text_field("cpus", &cpus, 60.0, false))),
        row(None, t("内存（GB）", "Memory (GB)"), None, Some(&text_field("mem", &mem, 60.0, false))),
        row(
            None,
            t("磁盘（GB）", "Disk (GB)"),
            Some(&if zh() { format!("现在 {disk_now} GB；只能调大，调大后要在系统里自己扩展分区") } else { format!("{disk_now} GB now; it only grows, and the system has to extend its partition itself") }),
            Some(&text_field("disk", &disk, 60.0, false)),
        ),
        row(
            None,
            t("屏幕", "Display"),
            Some(if bhyve { t("宽 × 高，最大 1920 × 1200", "Width × height, 1920 × 1200 at most") } else { t("宽 × 高", "Width × height") }),
            Some(&size),
        ),
        row(
            None,
            t("桥接到局域网", "Bridged to the LAN"),
            Some(t("虚拟机在局域网上有自己的地址，别的电脑能直接连", "The VM gets its own LAN address; other computers reach it directly")),
            Some(&net),
        ),
    ];
    if os != Os::Mac {
        // what will be plugged in at the next start
        let now = match &iso {
            Some(x) => x.clone(),
            None => image.clone(),
        };
        let title = now.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| t("没有插安装盘", "No installer attached").to_string());
        let choose = button(t("选择…", "Choose…"), || {
            save_fields();
            if let Some(p) = choose_file(t("选一个安装盘（.iso）", "Choose an installer (.iso)")) {
                wiz(|w| w.iso = Some(Some(p)));
            }
            render_wizard();
        });
        let eject = button(t("弹出", "Eject"), || {
            save_fields();
            wiz(|w| w.iso = Some(None));
            render_wizard();
        });
        eject.setEnabled(now.is_some());
        let acc = hstack(&[&*choose, &*eject], 8.0);
        rows.push(row(None, t("安装盘", "Installer"), Some(&title), Some(&acc)));
    }
    section(root_stack, t("设置", "SETTINGS"), &rows, Some(t("下次启动时生效。", "Takes effect at the next start.")));
    if !note.is_empty() {
        root_stack.addArrangedSubview(&wrapping(&note, 13.0, &NSColor::systemRedColor(), width - 40.0));
    }
    super::buttons_right(root_stack, None, &[button(t("取消", "Cancel"), close_wizard), super::primary(t("保存", "Save"), save_vm)]);
}

fn save_vm() {
    save_fields();
    let Some((name, cpus, mem, disk, disk_now, w_, h_, bridged, iso)) = wiz(|w| (w.edit.clone(), w.cpus.clone(), w.mem.clone(), w.disk.clone(), w.disk_now, w.w.clone(), w.h.clone(), w.bridged, w.iso.clone())) else {
        return;
    };
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(4);
    let s = |zh_: &'static str, en: &'static str| t(zh_, en).to_string();
    let problem = match (cpus.parse::<u64>(), mem.parse::<u64>(), disk.parse::<u64>(), w_.parse::<u64>(), h_.parse::<u64>()) {
        (Ok(c), ..) if !(1..=host_cpus).contains(&c) => Some(if zh() { format!("CPU 核数要在 1 到 {host_cpus} 之间。") } else { format!("CPUs: 1 to {host_cpus}.") }),
        (_, Ok(m), ..) if !(1..=512).contains(&m) => Some(s("内存至少 1 GB。", "Memory: 1 GB at least.")),
        (_, _, Ok(d), ..) if d < disk_now || d > 8192 => Some(s("磁盘只能调大。", "The disk can only grow.")),
        (_, _, _, Ok(w), Ok(h)) if !(640..=7680).contains(&w) || !(480..=4320).contains(&h) => Some(s("屏幕至少 640 × 480。", "Display: 640 × 480 at least.")),
        (Ok(_), Ok(_), Ok(_), Ok(_), Ok(_)) => None,
        _ => Some(s("每一项都要填数字。", "Every field takes a number.")),
    };
    // running meanwhile (started from elsewhere)?
    let problem = problem.or_else(|| ctl_request(&name, "info").is_ok().then(|| s("虚拟机正在运行：先停止它。", "The VM is running: stop it first.")));
    if let Some(p) = problem {
        wiz(|w| w.note = p);
        return render_wizard();
    }
    let (c, m, d, w, h) = (cpus.parse::<u64>().unwrap(), mem.parse::<u64>().unwrap(), disk.parse::<u64>().unwrap(), w_.parse::<u64>().unwrap(), h_.parse::<u64>().unwrap());
    if d > disk_now {
        // a sparse file: longer costs nothing until the guest writes there
        let grown = std::fs::OpenOptions::new().write(true).open(vm_dir(&name).join("disk.img")).and_then(|f| f.set_len(d << 30));
        if let Err(e) = grown {
            wiz(|w| w.note = e.to_string());
            return render_wizard();
        }
    }
    edit(&name, |j| {
        j.insert("cpus".into(), c.into());
        j.insert("mem_gb".into(), m.into());
        j.insert("w".into(), w.into());
        j.insert("h".into(), h.into());
        if bridged {
            if !j.contains_key("net") {
                j.insert("net".into(), "bridged".into());
            }
        } else {
            j.remove("net");
        }
        match &iso {
            Some(Some(p)) => {
                j.insert("iso".into(), p.to_string_lossy().into_owned().into());
            }
            Some(None) => {
                j.remove("iso");
            }
            None => {}
        }
    });
    close_wizard();
}

/// Rebuild the wizard's window for its current step.
fn render_wizard() {
    let Some(window) = wiz(|w| w.window.clone()) else { return };
    with(|u| {
        u.wizard_actions.clear();
        u.binding_wizard = true;
    });
    let root = super::vstack(&[], 12.0);
    root.setEdgeInsets(NSEdgeInsets { top: 16.0, left: 20.0, bottom: 16.0, right: 20.0 });
    root.setDistribution(NSStackViewDistribution::Fill);
    wizard_page(&root, super::MAIN_WIDTH);
    with(|u| u.binding_wizard = false);
    super::set_content(&window, &root, super::MAIN_WIDTH);
}

/// Cancel, or Create went off: close the wizard's window (which ends the wizard: `wizard_closed`).
pub fn close_wizard() {
    if let Some(w) = wiz(|w| w.window.clone()) {
        w.close();
    }
}

/// The wizard's window is closing (its close button, or `close_wizard`): forget the wizard and
/// show the list as it is now (a VM just created is in it).
pub fn wizard_closed() {
    if with(|u| u.wizard.take()).is_some() {
        with(|u| u.wizard_actions.clear());
        render();
    }
}

fn go_step(step: Step) {
    save_fields();
    wiz(|w| {
        w.step = step;
        w.note.clear();
    });
    render_wizard();
}

fn save_fields() {
    wiz(|w| {
        for (k, f) in &w.fields {
            let v = f.stringValue().to_string();
            match *k {
                "name" => w.name = v.trim().to_string(),
                "cpus" => w.cpus = v.trim().to_string(),
                "mem" => w.mem = v.trim().to_string(),
                "disk" => w.disk = v.trim().to_string(),
                "w" => w.w = v.trim().to_string(),
                "h" => w.h = v.trim().to_string(),
                "user" => w.user = v.trim().to_string(),
                "password" => w.password = v,
                _ => {}
            }
        }
    });
}

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Mac => "macOS",
        Os::Windows => "Windows 11",
        Os::Linux => "Linux",
        Os::FreeBsd => "FreeBSD",
    }
}

fn wizard_page(root_stack: &NSStackView, width: f64) {
    let Some((step, os)) = wiz(|w| {
        w.fields.clear();
        (w.step, w.os)
    }) else {
        return;
    };
    if step == Step::Edit {
        return step_edit(root_stack, width);
    }
    let (n, what) = match step {
        Step::Edit => unreachable!(),
        Step::Os => (1, t("选择系统", "Choose the system")),
        Step::Image => (2, t("选择安装映像", "Choose the installer")),
        Step::Settings => (3, t("设置", "Settings")),
    };
    let title = if zh() { format!("新建虚拟机 · 第 {n} 步，共 3 步：{what}") } else { format!("New Virtual Machine · step {n} of 3: {what}") };
    let h = label(&title, 15.0, unsafe { NSFontWeightMedium }, &NSColor::labelColor());
    root_stack.addArrangedSubview(&h);
    match step {
        Step::Os => step_os(root_stack),
        Step::Image => step_image(root_stack, os, width),
        Step::Settings => step_settings(root_stack, os, width),
        Step::Edit => {}
    }
}

fn step_os(root_stack: &NSStackView) {
    let choose = |os: Os| {
        move || {
            wiz(|w| {
                if w.os != os {
                    // another system: its own defaults and image
                    w.image = None;
                    w.name.clear();
                    w.cpus.clear();
                    w.mem.clear();
                    w.disk.clear();
                }
                w.os = os;
            });
            go_step(Step::Image);
        }
    };
    let rows = vec![
        row(
            Some(("apple.logo", NSColor::systemGrayColor())),
            "macOS",
            Some(t("用 .ipsw 恢复映像全自动安装，10–20 分钟", "Installs by itself from a .ipsw restore image, 10–20 minutes")),
            Some(&button(t("选择", "Choose"), choose(Os::Mac))),
        ),
        row(
            Some(("pc", NSColor::systemBlueColor())),
            "Windows 11",
            Some(t(
                "用微软的 ARM64 .iso，在「控制台」里安装，或者无人值守装到桌面",
                "From Microsoft's ARM64 .iso: install in the Console, or unattended to the desktop",
            )),
            Some(&button(t("选择", "Choose"), choose(Os::Windows))),
        ),
        row(
            Some(("terminal.fill", NSColor::systemOrangeColor())),
            "Linux",
            Some(t("从 arm64 安装盘 .iso 启动，在「控制台」里安装", "Boots an arm64 installer .iso: install it in the Console")),
            Some(&button(t("选择", "Choose"), choose(Os::Linux))),
        ),
    ];
    let mut rows = rows;
    // FreeBSD runs on the bhyve engine only
    if bhyve_ok() {
        rows.push(row(
            Some(("server.rack", NSColor::systemRedColor())),
            "FreeBSD",
            Some(t("从 arm64 安装盘 .iso 启动，在「控制台」里安装（跑在 bhyve 上）", "Boots an arm64 installer .iso: install it in the Console (runs on bhyve)")),
            Some(&button(t("选择", "Choose"), choose(Os::FreeBsd))),
        ));
    }
    section(root_stack, t("系统", "SYSTEM"), &rows, None);
    super::buttons_right(root_stack, None, &[button(t("取消", "Cancel"), close_wizard)]);
}

/// Folders looked in for installers: hyperMAC's iso folders and Downloads.
fn image_dirs() -> Vec<PathBuf> {
    // the storage folder's iso/ (vms.rs `storage`), the state folder's from before that setting
    let mut dirs = vec![storage().join("iso"), super::state().join("iso")];
    // an older setup: vms is a link to another disk, its iso folder beside them
    if let Some(p) = std::fs::canonicalize(guests_dir()).ok().and_then(|v| v.parent().map(|p| p.join("iso"))) {
        dirs.push(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Downloads"));
    }
    let mut seen = Vec::new();
    dirs.retain(|d| {
        let c = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        if seen.contains(&c) {
            false
        } else {
            seen.push(c);
            true
        }
    });
    dirs
}

fn files_in(dirs: &[PathBuf], keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()))
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| keep(&n.to_string_lossy().to_lowercase())))
        .collect();
    v.sort();
    v
}

/// The installers for `os` the folders hold.
fn candidates(os: Os) -> Vec<PathBuf> {
    files_in(&image_dirs(), |n| match os {
        Os::Mac => n.ends_with(".ipsw"),
        Os::Windows => n.ends_with(".iso") && n.contains("win") && !n.contains("virtio"),
        Os::Linux => n.ends_with(".iso") && !n.contains("win") && !n.contains("freebsd"),
        Os::FreeBsd => n.ends_with(".iso") && n.contains("freebsd"),
    })
}

fn virtio_candidates() -> Vec<PathBuf> {
    files_in(&image_dirs(), |n| n.starts_with("virtio-win") && n.ends_with(".iso"))
}

fn size_text(p: &std::path::Path) -> String {
    let b = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    format!("{:.1} GB", b as f64 / 1e9)
}

fn step_image(root_stack: &NSStackView, os: Os, width: f64) {
    let pick = |p: PathBuf| {
        move || {
            wiz(|w| w.image = Some(p.clone()));
            go_step(Step::Settings);
        }
    };
    let list = candidates(os);
    let rows: Vec<Retained<NSView>> = list
        .iter()
        .map(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let sub = format!("{} · {}", p.parent().map(|d| d.display().to_string()).unwrap_or_default(), size_text(p));
            row(None, &name, Some(&sub), Some(&button(t("用这个", "Use This"), pick(p.clone()))))
        })
        .collect();
    let mut rows = rows;
    if os == Os::Mac {
        // no file needed: `create --ipsw latest` asks Apple and downloads it (vm/src/vm.rs)
        rows.insert(
            0,
            row(
                Some(("arrow.down.circle.fill", NSColor::systemBlueColor())),
                t("从苹果下载最新的 macOS", "Download the newest macOS from Apple"),
                Some(t("这台 Mac 能跑的最新版，20 多 GB，存到存储位置的 iso 文件夹", "The newest this Mac can run, over 20 GB, kept in the storage folder's iso")),
                Some(&button(t("用这个", "Use This"), pick(PathBuf::from("latest")))),
            ),
        );
    }
    let hint = match os {
        Os::Mac => t(
            "也可以用自己的恢复映像（.ipsw，ipsw.me 上有各个版本）；版本不能比这台 Mac 新。",
            "Or use a restore image of your own (.ipsw; ipsw.me has every version); not newer than this Mac's macOS.",
        ),
        Os::Windows => t(
            "在微软官网下载「Windows 11 ARM64」的 ISO（任何语言都行，装好的系统用 ISO 的语言）。",
            "Download Microsoft's “Windows 11 ARM64” ISO (any language: the system gets the ISO's).",
        ),
        Os::Linux => t(
            "各发行版的 arm64（aarch64）安装盘：Fedora、Debian、Ubuntu 等。",
            "A distribution's arm64 (aarch64) installer: Fedora, Debian, Ubuntu, …",
        ),
        Os::FreeBsd => t(
            "在 download.freebsd.org 下载 arm64 aarch64 的 disc1.iso（或 bootonly.iso）。",
            "From download.freebsd.org: the arm64 aarch64 disc1.iso (or bootonly.iso).",
        ),
    };
    if rows.is_empty() {
        let where_ = image_dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join("、");
        let text = if zh() { format!("在 {where_} 里没找到 {} 的安装映像。{hint}", os_name(os)) } else { format!("No {} installer in {where_}. {hint}", os_name(os)) };
        root_stack.addArrangedSubview(&wrapping(&text, 13.0, &NSColor::secondaryLabelColor(), width - 40.0));
    } else {
        section(root_stack, t("找到的映像", "INSTALLERS FOUND"), &rows, Some(hint));
    }
    super::buttons_right(
        root_stack,
        Some(as_view(&button(t("上一步", "Back"), || go_step(Step::Os)))),
        &[button(t("取消", "Cancel"), close_wizard), button(t("其他文件…", "Other File…"), move || other_image(os))],
    );
}

fn other_image(os: Os) {
    let Some(path) = choose_file(match os {
        Os::Mac => t("选一个 macOS 恢复映像（.ipsw）", "Choose a macOS restore image (.ipsw)"),
        Os::Windows => t("选 Windows 11 ARM64 的 .iso", "Choose the Windows 11 ARM64 .iso"),
        Os::Linux => t("选一个 Linux arm64 安装盘（.iso）", "Choose a Linux arm64 installer (.iso)"),
        Os::FreeBsd => t("选一个 FreeBSD arm64 安装盘（.iso）", "Choose a FreeBSD arm64 installer (.iso)"),
    }) else {
        return;
    };
    let lower = path.to_string_lossy().to_lowercase();
    let ok = if os == Os::Mac { lower.ends_with(".ipsw") } else { lower.ends_with(".iso") };
    if !ok {
        alert(
            t("不是这种映像", "Not that kind of image"),
            if os == Os::Mac { t("要 .ipsw。", "It takes .ipsw.") } else { t("要 .iso。", "It takes .iso.") },
            &["OK"],
            None,
            true,
        );
        return;
    }
    wiz(|w| w.image = Some(path));
    go_step(Step::Settings);
}

fn choose_file(message: &str) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm());
    panel.setMessage(Some(&ns(message)));
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != 1 {
        return None;
    }
    panel.URL().and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string()))
}

/// The VM program's helpers a Windows install needs (beside hypermac-vm; winsetup.rs).
fn efiprobe_ok() -> bool {
    runner().and_then(|r| r.parent().map(|d| d.join("efiprobe.efi"))).is_some_and(|p| p.exists())
}

/// The bhyve engine beside hypermac-vm (build.sh installs it): Windows runs on it, and the
/// install then needs neither wimlib nor efiprobe (vm/src/bhyve.rs).
fn bhyve_ok() -> bool {
    // beside the VM program (build.sh), or in hyperMAC.app the firmware is in Contents/Resources
    runner().and_then(|r| r.parent().map(|d| d.to_path_buf())).is_some_and(|d| {
        d.join("bhyve").exists() && (d.join("QEMU_EFI.fd").exists() || d.join("../Resources/QEMU_EFI.fd").exists())
    })
}

fn wimlib_ok() -> bool {
    ["/opt/homebrew/bin/wimlib-imagex", "/usr/local/bin/wimlib-imagex"].iter().any(|p| std::path::Path::new(p).exists())
}

fn text_field(key: &'static str, value: &str, width: f64, secure: bool) -> Retained<NSTextField> {
    let f: Retained<NSTextField> = if secure {
        let s = NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm()), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 24.0)));
        s.setStringValue(&ns(value));
        s.into_super()
    } else {
        NSTextField::textFieldWithString(&ns(value), mtm())
    };
    fixed_size(&f, width, 24.0);
    wiz(|w| w.fields.push((key, f.clone())));
    f
}

fn step_settings(root_stack: &NSStackView, os: Os, width: f64) {
    // defaults the first time here
    // (looked up before the wizard's state is borrowed: the lookup reads the state directory
    // from the same cell, and doing it inside crashed the app at this step for Windows)
    let found_virtio = if os == Os::Windows { virtio_candidates().into_iter().next() } else { None };
    wiz(|w| {
        let (prefix, mem, disk) = match os {
            Os::Mac => ("mac", 6, 64),
            Os::Windows => ("win", 8, 64),
            Os::Linux => ("linux", 4, 32),
            Os::FreeBsd => ("freebsd", 4, 32),
        };
        if w.name.is_empty() {
            w.name = (1..).map(|i| format!("{prefix}{i}")).find(|n| !vm_dir(n).exists()).unwrap();
        }
        if w.cpus.is_empty() {
            w.cpus = "4".into();
        }
        if w.mem.is_empty() {
            w.mem = mem.to_string();
        }
        if w.disk.is_empty() {
            w.disk = disk.to_string();
        }
        if os == Os::Windows && w.virtio.as_ref().is_none_or(|p| !p.exists()) {
            w.virtio = found_virtio;
        }
    });
    let Some((image, name, cpus, mem, disk, bridged, user, password, virtio, note)) = wiz(|w| {
        (w.image.clone(), w.name.clone(), w.cpus.clone(), w.mem.clone(), w.disk.clone(), w.bridged, w.user.clone(), w.password.clone(), w.virtio.clone(), w.note.clone())
    }) else {
        return;
    };
    let image_name = image.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let image_name = if os == Os::Mac && image_name == "latest" { t("从苹果下载最新的 macOS", "The newest macOS, downloaded from Apple").to_string() } else { image_name };
    let net = switch(bridged, |on| {
        wiz(|w| w.bridged = on);
    });
    let mut rows = vec![
        row(None, t("名称", "Name"), Some(t("字母、数字、- 和 _", "Letters, digits, - and _")), Some(&text_field("name", &name, 160.0, false))),
        row(None, t("CPU 核数", "CPUs"), None, Some(&text_field("cpus", &cpus, 60.0, false))),
        row(None, t("内存（GB）", "Memory (GB)"), None, Some(&text_field("mem", &mem, 60.0, false))),
        row(
            None,
            t("磁盘（GB）", "Disk (GB)"),
            Some(t("稀疏文件：用多少占多少", "Sparse: takes only what is used")),
            Some(&text_field("disk", &disk, 60.0, false)),
        ),
        row(
            None,
            t("桥接到局域网", "Bridged to the LAN"),
            Some(t("虚拟机在局域网上有自己的地址，别的电脑能直接连", "The VM gets its own LAN address; other computers reach it directly")),
            Some(&net),
        ),
        row(None, t("安装映像", "Installer"), Some(&image_name), None),
    ];
    if os == Os::Mac {
        rows.push(row(None, t("安装", "Install"), Some(t("创建后自动安装 macOS，10–20 分钟", "macOS installs by itself after Create, 10–20 minutes")), None));
    }
    if os == Os::Linux || os == Os::FreeBsd {
        rows.push(row(None, t("安装", "Install"), Some(t("创建后点「启动」，在「控制台」里装系统", "After Create: Start it and install in the Console")), None));
    }
    section(root_stack, t("虚拟机", "VIRTUAL MACHINE"), &rows, None);

    if os == Os::Windows {
        // by hand in the Console (the default, user 2026-11-11), or by itself; the old engine
        // can only do it by itself
        let unattended = wiz(|w| w.unattended).unwrap_or(false) || !bhyve_ok();
        let mut install = Vec::new();
        if bhyve_ok() {
            let sw = switch(unattended, |on| {
                save_fields();
                wiz(|w| w.unattended = on);
                render_wizard();
            });
            install.push(row(
                Some(("wand.and.stars", NSColor::systemPurpleColor())),
                t("无人值守安装", "Unattended install"),
                Some(t("自己装到桌面，约 10 分钟；关掉就在「控制台」里手动装", "Installs to the desktop by itself, about 10 minutes; off: install by hand in the Console")),
                Some(&sw),
            ));
        }
        if unattended {
            install.push(row(Some(("person.fill", NSColor::systemBlueColor())), t("用户名", "User name"), Some(t("管理员账号，自动登录", "An administrator, logged on automatically")), Some(&text_field("user", &user, 160.0, false))));
            install.push(row(Some(("key.fill", NSColor::systemOrangeColor())), t("密码", "Password"), Some(t("远程桌面（RDP）登录用", "For Remote Desktop (RDP) logons")), Some(&text_field("password", &password, 160.0, true))));
        }
        section(
            root_stack,
            t("安装 Windows", "INSTALLING WINDOWS"),
            &install,
            Some(if unattended {
                t("语言跟随 ISO，电脑名用虚拟机的名字，装 Pro 版。", "The language is the ISO's, the computer name the VM's; the Pro edition goes in.")
            } else {
                t(
                    "创建后点「启动」，再点「控制台」，像在真机上一样安装。硬件检查（TPM、安全启动）已经替你关掉，网卡驱动会自动装上。装完后关机，点「弹出安装盘」。",
                    "After Create: Start it, open the Console and install as on a real PC. The hardware checks (TPM, Secure Boot) are switched off for you and the network driver goes in by itself. When done, shut down and click Eject Installer.",
                )
            }),
        );
        let virtio_row = {
            let (title, sub) = match &virtio {
                Some(p) => (p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), p.parent().map(|d| d.display().to_string()).unwrap_or_default()),
                None => (
                    t("没找到 virtio-win.iso", "virtio-win.iso not found").to_string(),
                    t("网卡和显卡的驱动：在 fedorapeople.org 下载 virtio-win.iso", "Drivers for the NIC and graphics: download virtio-win.iso from fedorapeople.org").to_string(),
                ),
            };
            let b = button(t("选择…", "Choose…"), || {
                save_fields();
                if let Some(p) = choose_file(t("选 virtio-win.iso", "Choose virtio-win.iso")) {
                    wiz(|w| w.virtio = Some(p));
                }
                render_wizard();
            });
            let color = if virtio.is_some() { NSColor::systemGreenColor() } else { NSColor::systemRedColor() };
            row(Some(("shippingbox.fill", color)), &title, Some(&sub), Some(&b))
        };
        let wim = if wimlib_ok() {
            row(Some(("checkmark.circle.fill", NSColor::systemGreenColor())), "wimlib", Some(t("拆分 install.wim 用，已安装", "Splits install.wim; installed")), None)
        } else {
            let b = button(t("拷贝命令", "Copy Command"), || copy("brew install wimlib".into()));
            row(
                Some(("exclamationmark.triangle.fill", NSColor::systemRedColor())),
                t("缺 wimlib", "wimlib is missing"),
                Some(t("在终端运行 brew install wimlib，再回来", "Run brew install wimlib in Terminal, then come back")),
                Some(&b),
            )
        };
        let mut need = if bhyve_ok() { vec![virtio_row] } else { vec![virtio_row, wim] };
        if !bhyve_ok() && !efiprobe_ok() {
            need.push(row(
                Some(("exclamationmark.triangle.fill", NSColor::systemRedColor())),
                t("缺 efiprobe.efi", "efiprobe.efi is missing"),
                Some(t("重新运行 build.sh 安装它", "Run build.sh again to install it")),
                None,
            ));
        }
        section(root_stack, t("安装需要", "THE INSTALL NEEDS"), &need, None);
    }
    if !note.is_empty() {
        root_stack.addArrangedSubview(&wrapping(&note, 13.0, &NSColor::systemRedColor(), width - 40.0));
    }
    super::buttons_right(
        root_stack,
        Some(as_view(&button(t("上一步", "Back"), || go_step(Step::Image)))),
        &[button(t("取消", "Cancel"), close_wizard), super::primary(t("创建", "Create"), create_vm)],
    );
}

fn create_vm() {
    save_fields();
    let Some(w) = with(|u| u.wizard.as_ref().map(|w| (w.os, w.image.clone(), w.virtio.clone(), w.name.clone(), w.cpus.clone(), w.mem.clone(), w.disk.clone(), w.bridged, w.user.clone(), w.password.clone()))) else {
        return;
    };
    let (os, image, virtio, name, cpus, mem, disk, bridged, user, password) = w;
    let manual = os == Os::Windows && bhyve_ok() && !wiz(|w| w.unattended).unwrap_or(false);
    // a manual install has no account to check
    let (user, password) = if manual { ("win".to_string(), "-".to_string()) } else { (user, password) };
    match check(os, &image, &virtio, &name, &cpus, &mem, &disk, &user, &password) {
        Err(e) => {
            wiz(|w| w.note = e);
            render_wizard();
        }
        Ok(()) => {
            let image = image.unwrap_or_default().to_string_lossy().into_owned();
            let mut args: Vec<String> = vec!["create".into(), name.clone()];
            match os {
                Os::Mac => args.extend(["--ipsw".into(), image]),
                Os::Linux => args.extend(["--os".into(), "linux".into(), "--iso".into(), image]),
                Os::FreeBsd => args.extend(["--os".into(), "freebsd".into(), "--iso".into(), image]),
                Os::Windows => {
                    args.extend(["--os".into(), "windows".into(), "--iso".into(), image, "--virtio".into(), virtio.unwrap_or_default().to_string_lossy().into_owned()]);
                    if manual {
                        args.push("--manual".into());
                    } else {
                        args.extend(["--user".into(), user]);
                    }
                }
            }
            args.extend(["--cpus".into(), cpus, "--mem-gb".into(), mem, "--disk-gb".into(), disk]);
            if bridged {
                args.extend(["--net".into(), "bridged".into()]);
            }
            let _ = std::fs::create_dir_all(root().join("logs"));
            let log = root().join("logs").join(format!("{name}.create.log"));
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            // the password goes in the environment, not on the command line (ps shows that)
            let env: Vec<(&str, &str)> = if os == Os::Windows { vec![("HYPERMAC_WIN_PASSWORD", password.as_str())] } else { vec![] };
            match spawn_detached(&refs, &log, &env) {
                Ok(()) => close_wizard(),
                Err(e) => {
                    wiz(|w| w.note = e.to_string());
                    render_wizard();
                }
            }
        }
    }
}

/// What is wrong with the wizard's answers, in words.
#[allow(clippy::too_many_arguments)]
fn check(os: Os, image: &Option<PathBuf>, virtio: &Option<PathBuf>, name: &str, cpus: &str, mem: &str, disk: &str, user: &str, password: &str) -> Result<(), String> {
    let s = |zh_: &'static str, en: &'static str| t(zh_, en).to_string();
    // ("latest": macOS downloaded from Apple, vm/src/vm.rs)
    let from_apple = os == Os::Mac && image.as_ref().is_some_and(|p| p.as_os_str() == "latest");
    if !from_apple && image.as_ref().is_none_or(|p| !p.exists()) {
        return Err(s("安装映像不见了：点「上一步」重新选。", "The installer is gone: go Back and choose it again."));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(s("名称只能用字母、数字、- 和 _。", "The name takes letters, digits, - and _ only."));
    }
    if vm_dir(name).exists() {
        return Err(s("已经有这个名字的虚拟机了。", "A VM of that name exists."));
    }
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(4);
    let (min_mem, min_disk) = match os {
        Os::Windows => (4, 64),
        Os::Mac => (4, 32),
        Os::Linux | Os::FreeBsd => (1, 8),
    };
    match cpus.parse::<u64>() {
        Ok(n) if (1..=host_cpus).contains(&n) => {}
        _ => return Err(if zh() { format!("CPU 核数要在 1 到 {host_cpus} 之间。") } else { format!("CPUs: 1 to {host_cpus}.") }),
    }
    match mem.parse::<u64>() {
        Ok(n) if (min_mem..=512).contains(&n) => {}
        _ => return Err(if zh() { format!("内存至少 {min_mem} GB。") } else { format!("Memory: {min_mem} GB at least.") }),
    }
    match disk.parse::<u64>() {
        Ok(n) if (min_disk..=8192).contains(&n) => {}
        _ => return Err(if zh() { format!("磁盘至少 {min_disk} GB。") } else { format!("Disk: {min_disk} GB at least.") }),
    }
    if os == Os::Windows {
        let user_ok = !user.is_empty() && user.len() <= 20 && user.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !user.eq_ignore_ascii_case("administrator");
        if !user_ok {
            return Err(s("用户名只能用字母、数字、- _ .，最多 20 个，不能是 Administrator。", "User name: letters, digits, - _ . only, 20 at most, not Administrator."));
        }
        if password.is_empty() {
            return Err(s("要设一个密码：远程桌面登录要用。", "Set a password: Remote Desktop logons need one."));
        }
        if virtio.as_ref().is_none_or(|p| !p.exists()) {
            return Err(s("还没选 virtio-win.iso。", "virtio-win.iso is not chosen yet."));
        }
        if bhyve_ok() {
            return Ok(());
        }
        if !wimlib_ok() {
            return Err(s("先装 wimlib：brew install wimlib。", "Install wimlib first: brew install wimlib."));
        }
        if !efiprobe_ok() {
            return Err(s("缺 efiprobe.efi：重新运行 build.sh。", "efiprobe.efi is missing: run build.sh again."));
        }
    }
    Ok(())
}

/// The row's state while `create` works on the VM (the `installing` file).
fn install_state(what: &str) -> String {
    if let Some(why) = what.strip_prefix("win:failed:") {
        return if zh() { format!("Windows 安装失败：{why}（详情在 logs 文件夹的 create.log）") } else { format!("Windows install failed: {why} (details: create.log in the logs folder)") };
    }
    match what {
        "win:stick" => t("正在准备安装盘", "Preparing the install media").into(),
        "win:phase1" => t("正在安装 Windows：1/2 复制系统文件", "Installing Windows: 1/2 copying the system").into(),
        "win:phase2" => t("正在安装 Windows：2/2 设置和首次登录", "Installing Windows: 2/2 setup and first logon").into(),
        pct => match pct.strip_prefix("dl:") {
            Some(p) => format!("{} {p}%", t("正在从苹果下载 macOS", "Downloading macOS from Apple")),
            None => format!("{} {pct}%", t("正在安装 macOS", "Installing macOS")),
        },
    }
}
