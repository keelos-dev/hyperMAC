//! hyperMAC's window (gui.rs opens it): System Settings–style grouped cards with switches.
//!
//! Pages: not in /Applications (just says so) -> first run (the Mac password, the two Privacy
//! grants, Remote Login) -> the main page (connection, settings, permissions, account). The
//! whole content is rebuilt by `render` after every action, and by a 2-second timer when what
//! it shows (service, grants, SSH) has changed. Buttons and switches carry a tag; the one
//! Objective-C target looks the tag up in this render's action list.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, Message, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezelStyle, NSBox, NSBoxType, NSButton,
    NSColor, NSControl, NSFont, NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSImage, NSImageScaling, NSImageSymbolConfiguration, NSImageView, NSLayoutAttribute, NSLayoutConstraintOrientation, NSMenu, NSMenuItem,
    NSPasteboard, NSPasteboardTypeString, NSSecureTextField, NSSegmentStyle, NSSegmentSwitchTracking, NSSegmentedControl, NSStackView, NSStackViewDistribution, NSSwitch,
    NSTextField, NSTitlePosition, NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{NSEdgeInsets, NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::gui::{self, ns, t, zh};

mod vmui;

const WIDTH: f64 = 540.0;
/// The main page: two columns of this width (user 2026-11-10: one column was too tall for a
/// MacBook Air's screen), 20 between them and at the sides.
const COLUMN: f64 = 470.0;
const MAIN_WIDTH: f64 = COLUMN * 2.0 + 60.0;

/// What `same_width` leaves out of a stack's width for its own side insets.
fn insets(root: &NSStackView) -> f64 {
    let e = root.edgeInsets();
    e.left + e.right
}

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    NotInApplications,
    Welcome,
    Permissions,
    Ssh,
    Main,
    /// the tokens there are (from the Host tab's token row)
    Tokens,
}

/// The main page's two tabs (user 2026-11-10): this Mac's settings, and the virtual machines.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tab {
    Host,
    Vms,
}

struct Ui {
    mtm: MainThreadMarker,
    state: PathBuf,
    window: Retained<NSWindow>,
    target: Retained<Target>,
    page: Page,
    actions: Vec<Rc<dyn Fn()>>,
    /// what the timer compares to decide on a re-render
    seen: String,
    /// the first-run password field and the message under it
    password: Option<Retained<NSSecureTextField>>,
    note: String,
    /// the first-run Remote Login switch (default off)
    ssh_choice: bool,
    /// Check for Updates (update.rs)
    upd: Upd,
    /// the About row's status line, updated in place while downloading
    upd_label: Option<Retained<NSTextField>>,
    tab: Tab,
    /// VMs just started (vmui.rs): "Starting…" until their ctl.sock answers
    starting: Vec<(String, std::time::Instant)>,
    /// New Virtual Machine's steps (vmui.rs), while open
    wizard: Option<vmui::Wizard>,
    /// the wizard is a window of its own, with its own action list: the main window re-renders
    /// (and renumbers its actions) behind it
    wizard_actions: Vec<Rc<dyn Fn()>>,
    /// controls bound now belong to the wizard's window (vmui::render_wizard)
    binding_wizard: bool,
}

/// tags from here on index Ui.wizard_actions
const WIZARD_TAG: isize = 1 << 20;

#[derive(Clone)]
enum Upd {
    Idle,
    Checking,
    UpToDate,
    Downloading(Arc<crate::update::Progress>, u64),
    /// the new app is in place; the service and this window still run the old one until reloaded
    Installed(String, PathBuf),
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACTarget"]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        // the app menu's items (main_menu)
        #[unsafe(method(menuUpdate:))]
        fn menu_update(&self, _sender: &AnyObject) {
            if with(|u| matches!(u.upd, Upd::Idle | Upd::UpToDate)) {
                check_for_updates();
            }
        }

        #[unsafe(method(menuClient:))]
        fn menu_client(&self, _sender: &AnyObject) {
            gui::open(gui::KRDPCLIENT_URL);
        }

        #[unsafe(method(menuRestart:))]
        fn menu_restart(&self, _sender: &AnyObject) {
            if crate::agent_plist().map(|p| p.exists()).unwrap_or(false) {
                gui::restart_service();
                render();
            }
        }

        #[unsafe(method(menuUninstall:))]
        fn menu_uninstall(&self, _sender: &AnyObject) {
            if !crate::agent_plist().map(|p| p.exists()).unwrap_or(false) {
                return;
            }
            if gui::confirm_uninstall(mtm()) {
                if let Err(e) = crate::uninstall() {
                    gui::error(mtm(), t("卸载失败", "Uninstall failed"), &format!("{e:#}"));
                }
            }
            render();
        }

        #[unsafe(method(act:))]
        fn act(&self, sender: &NSControl) {
            let tag = sender.tag();
            let action = UI.with(|u| {
                u.borrow().as_ref().and_then(|u| {
                    if tag >= WIZARD_TAG { u.wizard_actions.get((tag - WIZARD_TAG) as usize).cloned() } else { u.actions.get(tag as usize).cloned() }
                })
            });
            if let Some(a) = action {
                a();
            }
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            // the menu bar process keeps the closed window: nothing to keep fresh then
            if !UI.with(|u| u.borrow().as_ref().is_some_and(|u| u.window.isVisible())) {
                return;
            }
            let page = UI.with(|u| u.borrow().as_ref().map(|u| u.page));
            if !matches!(page, Some(Page::Main | Page::Permissions)) {
                return;
            }
            let now = snapshot();
            let changed = UI.with(|u| u.borrow().as_ref().is_some_and(|u| u.seen != now));
            if changed {
                render();
            }
        }
    }

    unsafe impl NSWindowDelegate for Target {
        #[unsafe(method(windowWillClose:))]
        fn will_close(&self, n: &NSNotification) {
            // the New Virtual Machine window (vmui.rs): only the wizard ends
            let closing = n.object().map(|o| Retained::as_ptr(&o) as usize);
            let main = UI.with(|u| u.borrow().as_ref().map(|u| Retained::as_ptr(&u.window) as usize));
            if closing != main {
                vmui::wizard_closed();
                return;
            }
            // in the menu bar process the window just closes; the menu bar icon stays and
            // the Dock icon goes (with the wizard, if that is still open)
            let app = NSApplication::sharedApplication(self.mtm());
            if !crate::menubar::is_menu_process() {
                app.terminate(None);
                return;
            }
            vmui::close_wizard();
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> R {
    UI.with(|u| f(u.borrow_mut().as_mut().expect("ui")))
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("main thread")
}

fn state() -> PathBuf {
    with(|u| u.state.clone())
}

fn go(page: Page) {
    with(|u| u.page = page);
    render();
}

/// Can this Mac run virtual machines? Not when it is one itself (hyperMAC installed in a macOS
/// guest so that the guest serves its own desktop): there it is the RDP server only. Asked of
/// the system: `kern.hv_vmm_present` (1 inside a virtual machine) and `kern.hv_support` (0 where
/// neither Virtualization.framework nor Hypervisor.framework works). Not known = yes.
/// (Should macOS guests ever get nested virtualization, hv_support alone is the test.)
fn can_virtualize() -> bool {
    static CAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    fn sysctl(name: &std::ffi::CStr) -> Option<libc::c_int> {
        let (mut v, mut len): (libc::c_int, libc::size_t) = (0, std::mem::size_of::<libc::c_int>());
        let r = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut v as *mut libc::c_int).cast(), &mut len, std::ptr::null_mut(), 0) };
        (r == 0).then_some(v)
    }
    *CAN.get_or_init(|| sysctl(c"kern.hv_support") != Some(0) && sysctl(c"kern.hv_vmm_present") != Some(1))
}

/// Everything the main / permissions pages show that can change behind our back.
fn snapshot() -> String {
    if with(|u| u.page == Page::Main && u.tab == Tab::Vms) {
        // plus what the shared header shows (service status, paused)
        let paused = crate::settings::get(&state(), "paused", false);
        return format!("{}{:?}{paused}", vmui::snapshot(), gui::service_pid());
    }
    let st = state();
    let creds = crate::account::read(&st).unwrap_or_default();
    format!(
        "{:?}{}{}{:?}{}{}{}{:?}",
        crate::menubar::status(&st),
        gui::screen_ok(),
        gui::ax_ok(),
        gui::service_pid(),
        gui::ssh_on(),
        crate::agent_plist().map(|p| p.exists()).unwrap_or(false),
        creds.hash_current(),
        gui::addresses()
    )
}

/// Open the window on `page` and run the app until it closes (no menu bar process yet: the
/// first-time setup, or not in /Applications).
pub fn run(mtm: MainThreadMarker, state: PathBuf, page: Page) {
    show(mtm, state, page);
    NSApplication::sharedApplication(mtm).run();
}

/// Open the window on `page` (or bring it to the front there, when it exists already).
pub fn show(mtm: MainThreadMarker, state: PathBuf, page: Page) {
    let app = NSApplication::sharedApplication(mtm);
    // a Dock icon while the window is open, none without one (user 2026-11-11; before that:
    // never). The menu bar icon stands for hyperMAC the rest of the time; LSUIElement too.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    gui::set_app_icon(&app);
    if let Some(window) = UI.with(|u| u.borrow().as_ref().map(|u| u.window.clone())) {
        with(|u| {
            u.state = state;
            u.page = page;
            if !matches!(u.upd, Upd::Downloading(..) | Upd::Installed(..)) {
                u.upd = Upd::Idle;
            }
        });
        render();
        if !window.isVisible() {
            window.center();
        }
        window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        return;
    }
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 600.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns("hyperMAC"));
    let target = Target::new(mtm);
    window.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    app.setMainMenu(Some(&main_menu(mtm, &target)));
    UI.with(|u| {
        *u.borrow_mut() = Some(Ui {
            mtm,
            state,
            window: window.clone(),
            target: target.clone(),
            page,
            actions: Vec::new(),
            seen: String::new(),
            password: None,
            note: String::new(),
            ssh_choice: false,
            upd: Upd::Idle,
            upd_label: None,
            tab: Tab::Host,
            starting: Vec::new(),
            wizard: None,
            wizard_actions: Vec::new(),
            binding_wizard: false,
        })
    });
    render();
    window.center();
    window.makeKeyAndOrderFront(None);
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(2.0, &target, sel!(tick:), None, true)
    };
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

/// Re-render the window if it is open (the menu bar changed something it shows).
pub fn refresh_if_open() {
    if UI.with(|u| u.borrow().as_ref().is_some_and(|u| u.window.isVisible())) {
        render();
    }
}

/// Quit / Cancel: the window closes; the app ends too, unless it is the menu bar process.
fn quit() {
    if crate::menubar::is_menu_process() {
        if let Some(w) = UI.with(|u| u.borrow().as_ref().map(|u| u.window.clone())) {
            w.performClose(None);
        }
    } else {
        NSApplication::sharedApplication(mtm()).terminate(None);
    }
}

/// The menu bar while the window is open. The app menu has what is not a setting and is
/// seldom needed (user 2026-11-11: not in the window): the version, Check for Updates,
/// Get kRDPclient, Restart Service, Uninstall.
fn main_menu(mtm: MainThreadMarker, target: &Target) -> Retained<NSMenu> {
    let item = |title: &str, action: objc2::runtime::Sel, key: &str| unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), Some(action), &ns(key))
    };
    let ours = |title: &str, action: objc2::runtime::Sel| {
        let i = item(title, action, "");
        unsafe { i.setTarget(Some(target)) };
        i
    };
    let bar = NSMenu::new(mtm);
    let app_menu = NSMenu::new(mtm);
    let about = NSMenuItem::new(mtm);
    about.setTitle(&ns(&format!("hyperMAC {}", gui::version())));
    about.setEnabled(false);
    app_menu.addItem(&about);
    app_menu.addItem(&ours(t("检查更新…", "Check for Updates…"), sel!(menuUpdate:)));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&ours(t("下载 kRDPclient", "Get kRDPclient"), sel!(menuClient:)));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&ours(t("重启服务", "Restart Service"), sel!(menuRestart:)));
    app_menu.addItem(&ours(t("卸载…", "Uninstall…"), sel!(menuUninstall:)));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // ⌘Q: in the menu bar process it closes the window (the icon stays; Pause is in its menu)
    if crate::menubar::is_menu_process() {
        app_menu.addItem(&item(t("关闭窗口", "Close Window"), sel!(performClose:), "q"));
    } else {
        app_menu.addItem(&item(t("退出 hyperMAC", "Quit hyperMAC"), sel!(terminate:), "q"));
    }
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(t("编辑", "Edit")));
    edit.addItem(&item(t("剪切", "Cut"), sel!(cut:), "x"));
    edit.addItem(&item(t("拷贝", "Copy"), sel!(copy:), "c"));
    edit.addItem(&item(t("粘贴", "Paste"), sel!(paste:), "v"));
    edit.addItem(&item(t("全选", "Select All"), sel!(selectAll:), "a"));
    let window_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(t("窗口", "Window")));
    window_menu.addItem(&item(t("关闭", "Close"), sel!(performClose:), "w"));
    for sub in [app_menu, edit, window_menu] {
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&sub));
        bar.addItem(&holder);
    }
    bar
}

// ---- building blocks ----------------------------------------------------------------------

/// An action for a control: its tag indexes this render's action list.
fn bind(control: &NSControl, action: impl Fn() + 'static) {
    with(|u| {
        if u.binding_wizard {
            u.wizard_actions.push(Rc::new(action));
            control.setTag(WIZARD_TAG + (u.wizard_actions.len() - 1) as isize);
        } else {
            u.actions.push(Rc::new(action));
            control.setTag((u.actions.len() - 1) as isize);
        }
        unsafe {
            control.setTarget(Some(&u.target));
            control.setAction(Some(sel!(act:)));
        }
    });
}

/// Any view as a plain NSView (retained).
fn as_view(v: &NSView) -> Retained<NSView> {
    v.retain()
}

fn label(text: &str, size: f64, weight: f64, color: &NSColor) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(text), mtm());
    l.setFont(Some(&NSFont::systemFontOfSize_weight(size, weight)));
    l.setTextColor(Some(color));
    l
}

fn wrapping(text: &str, size: f64, color: &NSColor, width: f64) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&ns(text), mtm());
    l.setFont(Some(&NSFont::systemFontOfSize(size)));
    l.setTextColor(Some(color));
    l.setPreferredMaxLayoutWidth(width);
    l.setSelectable(false);
    l
}

fn fixed_size(v: &NSView, w: f64, h: f64) {
    v.setTranslatesAutoresizingMaskIntoConstraints(false);
    v.widthAnchor().constraintEqualToConstant(w).setActive(true);
    v.heightAnchor().constraintEqualToConstant(h).setActive(true);
}

fn same_width(v: &NSView, of: &NSView, minus: f64) {
    v.setTranslatesAutoresizingMaskIntoConstraints(false);
    v.widthAnchor().constraintEqualToAnchor_constant(&of.widthAnchor(), -minus).setActive(true);
}

fn symbol(name: &str, size: f64) -> Option<Retained<NSImage>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None)?;
    let cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, unsafe { NSFontWeightSemibold });
    img.imageWithSymbolConfiguration(&cfg)
}

/// A white symbol on a coloured rounded square, like System Settings' rows.
fn tile(name: &str, color: &NSColor) -> Retained<NSView> {
    let b = NSBox::new(mtm());
    b.setBoxType(NSBoxType::Custom);
    b.setTitlePosition(NSTitlePosition::NoTitle);
    b.setBorderWidth(0.0);
    b.setCornerRadius(7.0);
    b.setFillColor(color);
    b.setContentViewMargins(NSSize::new(0.0, 0.0));
    if let Some(img) = symbol(name, 13.0) {
        let iv = NSImageView::imageViewWithImage(&img, mtm());
        iv.setContentTintColor(Some(&NSColor::whiteColor()));
        iv.setImageScaling(NSImageScaling::ScaleNone);
        b.setContentView(Some(&iv));
    }
    fixed_size(&b, 28.0, 28.0);
    b.into_super()
}

fn hstack(views: &[&NSView], spacing: f64) -> Retained<NSStackView> {
    let s = NSStackView::new(mtm());
    s.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    s.setAlignment(NSLayoutAttribute::CenterY);
    s.setSpacing(spacing);
    for v in views {
        s.addArrangedSubview(v);
    }
    s
}

fn vstack(views: &[&NSView], spacing: f64) -> Retained<NSStackView> {
    let s = NSStackView::new(mtm());
    s.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    s.setAlignment(NSLayoutAttribute::Leading);
    s.setSpacing(spacing);
    for v in views {
        s.addArrangedSubview(v);
    }
    s
}

fn spacer() -> Retained<NSView> {
    let v = NSView::new(mtm());
    v.setContentHuggingPriority_forOrientation(1.0, NSLayoutConstraintOrientation::Horizontal);
    v
}

/// One row: icon tile, title (+ subtitle), the control at the right.
fn row(icon: Option<(&str, Retained<NSColor>)>, title: &str, subtitle: Option<&str>, accessory: Option<&NSView>) -> Retained<NSView> {
    let title_l = label(title, 13.0, unsafe { NSFontWeightRegular }, &NSColor::labelColor());
    let texts = vstack(&[&title_l], 2.0);
    if let Some(s) = subtitle {
        let sub = wrapping(s, 11.0, &NSColor::secondaryLabelColor(), 300.0);
        sub.setMaximumNumberOfLines(2);
        texts.addArrangedSubview(&sub);
    }
    texts.setContentHuggingPriority_forOrientation(1.0, NSLayoutConstraintOrientation::Horizontal);
    let r = NSStackView::new(mtm());
    r.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    r.setAlignment(NSLayoutAttribute::CenterY);
    r.setSpacing(10.0);
    r.setEdgeInsets(NSEdgeInsets { top: 5.0, left: 12.0, bottom: 5.0, right: 12.0 });
    if let Some((name, color)) = icon {
        r.addArrangedSubview(&tile(name, &color));
    }
    r.addArrangedSubview(&texts);
    r.addArrangedSubview(&spacer());
    if let Some(a) = accessory {
        r.addArrangedSubview(a);
    }
    r.setTranslatesAutoresizingMaskIntoConstraints(false);
    r.heightAnchor().constraintGreaterThanOrEqualToConstant(40.0).setActive(true);
    r.into_super()
}

/// Rows on one rounded card, hairlines between them.
fn card(rows: &[Retained<NSView>]) -> Retained<NSView> {
    let b = NSBox::new(mtm());
    b.setBoxType(NSBoxType::Custom);
    b.setTitlePosition(NSTitlePosition::NoTitle);
    b.setCornerRadius(10.0);
    b.setBorderWidth(1.0);
    b.setBorderColor(&NSColor::separatorColor());
    b.setFillColor(&NSColor::controlBackgroundColor());
    b.setContentViewMargins(NSSize::new(0.0, 0.0));
    let s = vstack(&[], 0.0);
    // trailing: the hairlines start after the icon column, like System Settings
    s.setAlignment(NSLayoutAttribute::Trailing);
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            let line = NSBox::new(mtm());
            line.setBoxType(NSBoxType::Separator);
            s.addArrangedSubview(&line);
            same_width(&line, &s, 52.0);
            s.setCustomSpacing_afterView(0.0, &line);
        }
        s.addArrangedSubview(r);
        same_width(r, &s, 0.0);
    }
    b.setContentView(Some(&s));
    b.into_super()
}

/// A section: a small heading over a card (and an optional footnote under it).
fn section(root: &NSStackView, heading: &str, rows: &[Retained<NSView>], footnote: Option<&str>) {
    let h = label(heading, 12.0, unsafe { NSFontWeightSemibold }, &NSColor::secondaryLabelColor());
    let c = card(rows);
    let s = vstack(&[&h, &c], 5.0);
    s.setEdgeInsets(NSEdgeInsets { top: 0.0, left: 0.0, bottom: 0.0, right: 0.0 });
    if let Some(f) = footnote {
        let l = wrapping(f, 11.0, &NSColor::secondaryLabelColor(), COLUMN - 20.0);
        s.addArrangedSubview(&l);
    }
    root.addArrangedSubview(&s);
    same_width(&s, root, insets(root));
    same_width(&c, &s, 0.0);
}

fn button(title: &str, action: impl Fn() + 'static) -> Retained<NSButton> {
    let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), None, None, mtm()) };
    b.setBezelStyle(NSBezelStyle::Push);
    bind(&b, action);
    b
}

fn primary(title: &str, action: impl Fn() + 'static) -> Retained<NSButton> {
    let b = button(title, action);
    b.setKeyEquivalent(&ns("\r"));
    b
}

fn switch(on: bool, action: impl Fn(bool) + 'static) -> Retained<NSSwitch> {
    let s = NSSwitch::new(mtm());
    s.setState(if on { 1 } else { 0 });
    let me = s.clone();
    bind(&s, move || action(me.state() == 1));
    s
}

fn check(ok: bool, fix: impl Fn() + 'static) -> Retained<NSView> {
    if ok {
        let mark = symbol("checkmark.circle.fill", 15.0).map(|img| {
            let iv = NSImageView::imageViewWithImage(&img, mtm());
            iv.setContentTintColor(Some(&NSColor::systemGreenColor()));
            iv
        });
        let l = label(t("已允许", "Allowed"), 12.0, unsafe { NSFontWeightMedium }, &NSColor::systemGreenColor());
        match mark {
            Some(m) => as_view(&hstack(&[&m, &l], 4.0)),
            None => as_view(&l),
        }
    } else {
        as_view(&button(t("去设置…", "Open Settings…"), fix))
    }
}

fn copy(text: String) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    unsafe { pb.setString_forType(&ns(&text), NSPasteboardTypeString) };
}

/// The app icon, big, with the title and a status line beside it; returns the row (the main
/// page puts the About card at its right end).
fn header(root: &NSStackView, title: &str, status: Option<(&str, Retained<NSColor>)>) -> Retained<NSStackView> {
    let icon = NSApplication::sharedApplication(mtm()).applicationIconImage();
    let mut views: Vec<Retained<NSView>> = Vec::new();
    if let Some(img) = icon {
        let iv = NSImageView::imageViewWithImage(&img, mtm());
        fixed_size(&iv, 64.0, 64.0);
        views.push(as_view(&iv));
    }
    let title_l = label(title, 22.0, unsafe { NSFontWeightBold }, &NSColor::labelColor());
    let texts = vstack(&[&title_l], 3.0);
    if let Some((text, color)) = status {
        let dot = label("●", 11.0, unsafe { NSFontWeightRegular }, &color);
        let l = label(text, 12.0, unsafe { NSFontWeightMedium }, &NSColor::secondaryLabelColor());
        texts.addArrangedSubview(&hstack(&[&dot, &l], 4.0));
    }
    views.push(as_view(&texts));
    let refs: Vec<&NSView> = views.iter().map(|v| &**v).collect();
    let h = hstack(&refs, 14.0);
    root.addArrangedSubview(&h);
    h
}

fn paragraph(root: &NSStackView, text: &str) {
    let p = wrapping(text, 13.0, &NSColor::labelColor(), WIDTH - 40.0);
    root.addArrangedSubview(&p);
    same_width(&p, root, insets(root));
}

fn buttons_right(root: &NSStackView, left: Option<Retained<NSView>>, buttons: &[Retained<NSButton>]) {
    let mut views: Vec<Retained<NSView>> = Vec::new();
    if let Some(l) = left {
        views.push(l);
    }
    views.push(spacer());
    for b in buttons {
        views.push(as_view(b));
    }
    let refs: Vec<&NSView> = views.iter().map(|v| &**v).collect();
    let bar = hstack(&refs, 8.0);
    root.addArrangedSubview(&bar);
    same_width(&bar, root, insets(root));
}

// ---- pages --------------------------------------------------------------------------------

/// `root` as the window's content, `width` wide and as high as it needs.
fn set_content(window: &NSWindow, root: &NSStackView, width: f64) {
    root.setTranslatesAutoresizingMaskIntoConstraints(false);
    root.widthAnchor().constraintEqualToConstant(width).setActive(true);
    // keep the window's top edge where it is while its height follows the content
    let old = window.frame();
    window.setContentView(Some(root));
    root.layoutSubtreeIfNeeded();
    let size = root.fittingSize();
    let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, size.height));
    let frame = window.frameRectForContentRect(content);
    let top = old.origin.y + old.size.height;
    window.setFrame_display(
        NSRect::new(NSPoint::new(old.origin.x, top - frame.size.height), frame.size),
        true,
    );
}

/// Rebuild the window's content for the current page.
pub fn render() {
    let (window, page) = with(|u| {
        u.actions.clear();
        u.password = None;
        (u.window.clone(), u.page)
    });
    let root = vstack(&[], 12.0);
    root.setEdgeInsets(NSEdgeInsets { top: 16.0, left: 20.0, bottom: 16.0, right: 20.0 });
    root.setDistribution(NSStackViewDistribution::Fill);
    match page {
        Page::NotInApplications => page_not_in_applications(&root),
        Page::Welcome => page_welcome(&root),
        Page::Permissions => page_permissions(&root),
        Page::Ssh => page_ssh(&root),
        Page::Tokens => page_tokens(&root),
        Page::Main => {
            main_header(&root);
            tabs(&root);
            match with(|u| u.tab) {
                Tab::Host => page_main(&root),
                Tab::Vms if can_virtualize() => vmui::page(&root, MAIN_WIDTH),
                // a Mac that is itself a virtual machine has no hypervisor to offer: the tab
                // says so (user 2026-11-11: keep the tab, say why it is empty)
                Tab::Vms => {
                    let card = vstack(&[], 8.0);
                    card.setEdgeInsets(NSEdgeInsets { top: 28.0, left: 0.0, bottom: 28.0, right: 0.0 });
                    card.addArrangedSubview(&label(
                        t("这台 Mac 本身是一台虚拟机", "This Mac is itself a virtual machine"),
                        15.0,
                        unsafe { NSFontWeightSemibold },
                        &NSColor::labelColor(),
                    ));
                    card.addArrangedSubview(&wrapping(
                        t(
                            "虚拟机里不能再运行虚拟机，所以这里的功能不可用。hyperMAC 在这台机器上只做远程桌面：别的电脑可以连它的桌面，设置在「主机」页。",
                            "A virtual machine cannot run virtual machines, so this part is not available here. On this machine hyperMAC is the remote desktop only: other computers can connect to its desktop; the settings are on the Host tab.",
                        ),
                        13.0,
                        &NSColor::secondaryLabelColor(),
                        MAIN_WIDTH - 40.0,
                    ));
                    root.addArrangedSubview(&card);
                }
            }
        }
    }
    let width = if page == Page::Main { MAIN_WIDTH } else { WIDTH };
    set_content(&window, &root, width);
    let pw = with(|u| {
        u.seen = String::new();
        u.password.clone()
    });
    let now = snapshot();
    with(|u| u.seen = now);
    if let Some(f) = pw {
        window.makeFirstResponder(Some(&f));
    }
}

/// The Host | Virtual Machines switch at the top of the main page.
fn tabs(root: &NSStackView) {
    let labels = [ns(t("主机", "Host")), ns(t("虚拟机", "Virtual Machines"))];
    let refs: Vec<&NSString> = labels.iter().map(|l| &**l).collect();
    let seg = unsafe { NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
        &objc2_foundation::NSArray::from_slice(&refs),
        NSSegmentSwitchTracking::SelectOne,
        None,
        None,
        mtm(),
    ) };
    seg.setSegmentStyle(NSSegmentStyle::Automatic);
    seg.setSelectedSegment(if with(|u| u.tab) == Tab::Vms { 1 } else { 0 });
    let weak = objc2::rc::Weak::from_retained(&seg);
    bind(&seg, move || {
        let Some(seg) = weak.load() else { return };
        let tab = if seg.selectedSegment() == 1 { Tab::Vms } else { Tab::Host };
        if with(|u| std::mem::replace(&mut u.tab, tab)) != tab {
            render();
        }
    });
    // pinned to the centre: between two spacers in a stack it jumped from centre to right
    // between the tabs (the layout picked either spacer to stretch)
    let bar = NSView::new(mtm());
    seg.setTranslatesAutoresizingMaskIntoConstraints(false);
    bar.addSubview(&seg);
    seg.centerXAnchor().constraintEqualToAnchor(&bar.centerXAnchor()).setActive(true);
    seg.topAnchor().constraintEqualToAnchor(&bar.topAnchor()).setActive(true);
    seg.bottomAnchor().constraintEqualToAnchor(&bar.bottomAnchor()).setActive(true);
    root.addArrangedSubview(&bar);
    same_width(&bar, root, insets(root));
}

fn page_not_in_applications(root: &NSStackView) {
    // an installed copy is there already: this one (opened in the dmg) can replace it itself,
    // which Finder will not do while the menu bar icon or a virtual machine runs from it
    let installed = std::path::Path::new("/Applications/hyperMAC.app").exists();
    let this = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).and_then(|e| crate::app_bundle(&e));
    if let (true, Some(this)) = (installed, this) {
        header(root, t("更新已安装的 hyperMAC", "Update the installed hyperMAC"), None);
        paragraph(
            root,
            &if zh() {
                format!("「应用程序」里已经有一份 hyperMAC。点「更新」用这一份（{}）替换它，然后重启服务和菜单栏图标；正在运行的虚拟机不受影响，正在连着的远程会话会断开几秒。", gui::version())
            } else {
                format!("There is a hyperMAC in Applications already. Update replaces it with this copy ({}) and restarts the service and the menu bar icon; virtual machines that are running go on, a remote session in progress drops for a few seconds.", gui::version())
            },
        );
        buttons_right(
            root,
            None,
            &[
                button(t("退出", "Quit"), quit),
                primary(t("更新", "Update"), move || match crate::update::install_from(&this) {
                    Ok(app) => {
                        // the service and the menu bar icon start again on the new files; the
                        // icon's process opens the window. Never set up: just open the new app.
                        if crate::agent_plist().map(|p| p.exists()).unwrap_or(false) {
                            gui::restart_service();
                            crate::menubar::restart(&state(), true);
                        } else {
                            let _ = std::process::Command::new("/bin/sh").arg("-c").arg(format!("sleep 1; /usr/bin/open -n '{}'", app.display())).spawn();
                        }
                        NSApplication::sharedApplication(mtm()).terminate(None);
                    }
                    Err(e) => gui::error(mtm(), t("没能更新", "The update failed"), &format!("{e:#}")),
                }),
            ],
        );
        return;
    }
    header(root, t("先把 hyperMAC 拖进「应用程序」", "Move hyperMAC to Applications first"), None);
    paragraph(
        root,
        t(
            "hyperMAC 要从「应用程序」文件夹里运行，开机自启才能一直找到它。请把 hyperMAC 拖进「应用程序」（dmg 窗口里的箭头指的就是那里），然后从那里打开。",
            "hyperMAC has to run from the Applications folder so that the login item always finds it. Drag hyperMAC into Applications (where the arrow in the dmg window points), then open it from there.",
        ),
    );
    buttons_right(
        root,
        None,
        &[
            button(t("退出", "Quit"), quit),
            primary(t("打开「应用程序」", "Open Applications"), || gui::open("/Applications")),
        ],
    );
}

fn page_welcome(root: &NSStackView) {
    let user = crate::account::mac_user();
    header(root, t("欢迎使用 hyperMAC", "Welcome to hyperMAC"), None);
    paragraph(
        root,
        t(
            "从别的电脑连进这台 Mac 的桌面。登录用的就是这台 Mac 的账号和登录密码，不用另记密码。",
            "Reach this Mac's desktop from another computer. You log in with this Mac's own account and login password; there is nothing new to remember.",
        ),
    );
    let field = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm()),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(220.0, 24.0)),
    );
    field.setPlaceholderString(Some(&ns(t("登录密码", "Login password"))));
    fixed_size(&field, 220.0, 24.0);
    bind(&field, welcome_continue);
    let subtitle = t("请输入这台 Mac 的登录密码", "Type this Mac's login password");
    section(
        root,
        t("账号", "ACCOUNT"),
        &[row(Some(("person.fill", NSColor::systemBlueColor())), &user, Some(subtitle), Some(&field))],
        Some(t(
            "系统会核对这个密码。hyperMAC 不保存密码本身，只存 mstsc 登录要用的一个哈希；你以后改了 Mac 密码，旧密码立刻就不能再登录。",
            "The system checks the password. hyperMAC does not keep it, only a hash that mstsc's login needs; once you change the Mac password, the old one no longer logs in.",
        )),
    );
    let note = with(|u| {
        u.password = Some(field.clone());
        u.note.clone()
    });
    let left = (!note.is_empty())
        .then(|| as_view(&label(&note, 12.0, unsafe { NSFontWeightMedium }, &NSColor::systemRedColor())));
    buttons_right(
        root,
        left,
        &[
            button(t("取消", "Cancel"), quit),
            primary(t("继续", "Continue"), welcome_continue),
        ],
    );
}

fn welcome_continue() {
    let Some(field) = with(|u| u.password.clone()) else { return };
    let pw = field.stringValue().to_string();
    let user = crate::account::mac_user();
    let note = if pw.is_empty() {
        t("请输入密码（Mac 没设登录密码的话，先在「用户与群组」里设一个）", "Type the password (no login password on this Mac? set one in Users & Groups first)")
    } else if crate::account::set_system(&state(), &user, &pw).is_ok() {
        with(|u| u.note.clear());
        gui::request_permissions();
        return go(Page::Permissions);
    } else {
        t("密码不对，请再试一次", "That is not the password, try again")
    };
    with(|u| u.note = note.into());
    render();
}

fn permission_rows() -> Vec<Retained<NSView>> {
    vec![
        row(
            Some(("record.circle", NSColor::systemRedColor())),
            t("屏幕录制", "Screen Recording"),
            Some(t("没有它客户端是黑屏", "Without it the screen stays black")),
            Some(&check(gui::screen_ok(), || gui::open(gui::SETTINGS_SCREEN))),
        ),
        row(
            Some(("hand.point.up.left.fill", NSColor::systemBlueColor())),
            t("辅助功能", "Accessibility"),
            Some(t("键盘鼠标要用；需自己在列表里打开开关", "For keyboard and mouse; switch it on in the list yourself")),
            Some(&check(gui::ax_ok(), || gui::open(gui::SETTINGS_AX))),
        ),
    ]
}

fn page_permissions(root: &NSStackView) {
    header(root, t("两项授权", "Two permissions"), None);
    paragraph(
        root,
        t(
            "在「系统设置 → 隐私与安全性」里允许下面两项。允许后这里会自动变成绿色（「屏幕录制」有时要重开 hyperMAC 才显示）。",
            "Allow these two in System Settings → Privacy & Security. They turn green here once allowed (Screen Recording sometimes only after hyperMAC is reopened).",
        ),
    );
    section(
        root,
        t("权限", "PERMISSIONS"),
        &permission_rows(),
        Some(t(
            "macOS 26 在第一次有人连上时，还会再问一次「要采集屏幕和音频」，选允许。",
            "On macOS 26 the first connection also asks once whether hyperMAC may capture the screen and audio: allow it.",
        )),
    );
    let go_on = || {
        if let Err(e) = crate::install(&state()) {
            gui::error(mtm(), t("没能装上自启", "Could not install the login item"), &format!("{e:#}"));
        }
        go(if gui::ssh_on() { Page::Main } else { Page::Ssh });
    };
    buttons_right(
        root,
        None,
        &[
            // someone who wants the virtual machines only: no grants, the Mac's own desktop off
            // (the switch is on the main page's Host tab)
            button(t("只用虚拟机，跳过", "Virtual Machines Only"), move || {
                if let Err(e) = crate::settings::set(&state(), "host_desktop", false) {
                    gui::error(mtm(), t("没能保存", "Could not save"), &format!("{e:#}"));
                }
                go_on();
            }),
            primary(t("继续", "Continue"), go_on),
        ][if can_virtualize() { 0 } else { 1 }..],
    );
}

/// "3 分钟前" for the token list.
fn ago(then: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let d = now.saturating_sub(then);
    let (n, zh_unit, en_unit) = match d {
        0..=119 => return t("刚刚", "just now").into(),
        120..=7199 => (d / 60, "分钟前", "minutes ago"),
        7200..=172_799 => (d / 3600, "小时前", "hours ago"),
        _ => (d / 86400, "天前", "days ago"),
    };
    if zh() { format!("{n} {zh_unit}") } else { format!("{n} {en_unit}") }
}

/// The tokens made so far: who for, when, last used; each can be taken away.
fn page_tokens(root: &NSStackView) {
    header(root, t("kRDPclient token", "kRDPclient tokens"), None);
    let st = state();
    let tokens = crate::web::read_tokens(&st.join("tokens.json"));
    let mut list: Vec<(String, crate::web::TokenInfo)> =
        tokens.sha256.iter().map(|h| (h.clone(), tokens.info.get(h).cloned().unwrap_or_default())).collect();
    list.sort_by_key(|(_, i)| std::cmp::Reverse(i.last_used.max(i.created)));
    let rows: Vec<Retained<NSView>> = list
        .iter()
        .map(|(hash, i)| {
            let title = match (i.name.is_empty(), i.id.is_empty()) {
                (false, false) => format!("{} · keel_{}_…", i.name, i.id),
                (true, false) => format!("keel_{}_…", i.id),
                // made before the list existed: only the hash is known
                _ => format!("{} {}…", t("早期的 token，哈希", "An early token, hash"), &hash[..hash.len().min(12)]),
            };
            let made = if i.created == 0 { t("创建时间不详", "made: not known").to_string() } else { format!("{} {}", t("创建于", "made"), ago(i.created)) };
            let used = if i.last_used == 0 { t("还没用过", "never used").to_string() } else { format!("{} {}", t("最后使用", "last used"), ago(i.last_used)) };
            let hash = hash.clone();
            let del = button(t("删除", "Delete"), move || {
                if let Err(e) = crate::web::revoke_token(&state(), &hash) {
                    gui::error(mtm(), t("没能删除", "Could not delete"), &format!("{e:#}"));
                }
                render();
            });
            del.setHasDestructiveAction(true);
            row(Some(("key.fill", NSColor::systemOrangeColor())), &title, Some(&format!("{made} · {used}")), Some(&del))
        })
        .collect();
    if rows.is_empty() {
        paragraph(root, t("还没有 token。", "No tokens yet."));
    } else {
        section(
            root,
            t("已发出的 token", "TOKENS"),
            &rows,
            Some(t(
                "kRDPclient 用 Mac 密码添加主机时会自己换一个 token。删除后，用它的客户端要重新添加主机；已经连着的会话不受影响。「最后使用」从这一版开始记。",
                "kRDPclient trades the Mac password for a token of its own when it adds this host. After a delete, the client that used it must add the host again; a session that is up stays. Last used is recorded from this version on.",
            )),
        );
    }
    buttons_right(
        root,
        None,
        &[
            button(t("新建…", "New…"), || {
                gui::token_dialog(mtm(), &state());
                render();
            }),
            primary(t("完成", "Done"), || go(Page::Main)),
        ],
    );
}

fn page_ssh(root: &NSStackView) {
    header(root, t("远程登录（SSH）", "Remote Login (SSH)"), None);
    paragraph(
        root,
        t(
            "打开后，可以在 kRDPclient 里直接开这台 Mac 的终端。SSH 也用这台 Mac 的账号和密码；局域网里能连到这台 Mac 22 端口的人都可以尝试登录。不需要的话保持关闭，以后随时能在 hyperMAC 里打开。",
            "With it on, kRDPclient can open a terminal on this Mac. SSH also logs in with this Mac's account and password; anyone who can reach this Mac's port 22 can try. Leave it off if you do not need it; you can turn it on in hyperMAC later.",
        ),
    );
    let on = with(|u| u.ssh_choice);
    let sw = switch(on, |v| with(|u| u.ssh_choice = v));
    section(
        root,
        t("可选", "OPTIONAL"),
        &[row(
            Some(("terminal.fill", NSColor::systemGrayColor())),
            t("打开远程登录", "Turn on Remote Login"),
            Some(t("会带你到「系统设置 → 通用 → 共享」，在那里打开", "Takes you to System Settings → General → Sharing to switch it on")),
            Some(&sw),
        )],
        None,
    );
    buttons_right(
        root,
        None,
        &[primary(t("完成", "Done"), || {
            if with(|u| u.ssh_choice) {
                gui::open(gui::SETTINGS_SHARING);
            }
            go(Page::Main);
        })],
    );
}

/// The top of the main page, the same on both tabs: name, service status, and an update's
/// progress while there is one.
fn main_header(root: &NSStackView) {
    let st = state();
    let _ = crate::ensure_cert(&st); // the fingerprint, before the service has made it
    let installed = crate::agent_plist().map(|p| p.exists()).unwrap_or(false);
    let pid = gui::service_pid();
    let paused = crate::settings::get(&st, "paused", false);
    let status = match (installed, pid) {
        (_, Some(_)) if paused => (
            t("已暂停远程：不接受新连接（在菜单栏图标里恢复）", "Remote paused: no new connections (resume from the menu bar icon)"),
            NSColor::systemOrangeColor(),
        ),
        (_, Some(_)) if !crate::settings::remote(&st) => {
            (t("远程连接已关闭：没有开放端口", "Remote access is off: no port is open"), NSColor::systemGrayColor())
        }
        (_, Some(_)) => (t("服务运行中", "Service running"), NSColor::systemGreenColor()),
        (true, None) => (t("服务没在运行：检查授权，或稍等它重启", "Service not running: check the permissions, or wait for its restart"), NSColor::systemOrangeColor()),
        (false, None) => (t("没有开启自动运行", "Not set to run"), NSColor::systemGrayColor()),
    };
    let top = header(root, "hyperMAC", Some((status.0, status.1)));
    // Check for Updates is in the app menu; its progress and result show here, top right,
    // only while there is something to say (update.rs)
    let upd = with(|u| u.upd.clone());
    if matches!(upd, Upd::Idle) {
        with(|u| u.upd_label = None);
        same_width(&top, root, insets(root));
        return;
    }
    let status = upd_status(&upd);
    let restart = match &upd {
        Upd::Installed(_, app) => {
            let app = app.clone();
            Some(primary(t("立即重启", "Restart Now"), move || reload(app.clone())))
        }
        _ => None,
    };
    let about_row = row(
        Some(("arrow.triangle.2.circlepath", NSColor::systemBlueColor())),
        &format!("hyperMAC {}", gui::version()),
        Some(&status),
        restart.as_deref().map(|b| -> &NSView { b }),
    );
    let about_card = card(&[about_row.clone()]);
    about_card.setTranslatesAutoresizingMaskIntoConstraints(false);
    about_card.widthAnchor().constraintEqualToConstant(COLUMN).setActive(true);
    top.addArrangedSubview(&spacer());
    top.addArrangedSubview(&about_card);
    same_width(&top, root, insets(root));
    // the status line is the row's second label: kept to update it in place
    let sub = find_subtitle(&about_row);
    with(|u| u.upd_label = sub);
}

fn page_main(root: &NSStackView) {
    let st = state();
    let installed = crate::agent_plist().map(|p| p.exists()).unwrap_or(false);
    let pid = gui::service_pid();

    // two columns: what you look at (remote, connection, permissions) | what you set
    let left = vstack(&[], 12.0);
    let right = vstack(&[], 12.0);
    for c in [&left, &right] {
        c.setTranslatesAutoresizingMaskIntoConstraints(false);
        c.widthAnchor().constraintEqualToConstant(COLUMN).setActive(true);
    }

    // remote: who is connected, Disconnect, Pause — also in the menu bar icon's menu, which is
    // hidden while a client is connected (menubar.rs); this window is reached then from the
    // purple screen-recording indicator (clicking hyperMAC there opens it)
    let now = crate::menubar::status(&st);
    if pid.is_some() {
        let who = if now.connections > 0 {
            format!("{} {}", now.connections, t("个连接", if now.connections == 1 { "client connected" } else { "clients connected" }))
        } else {
            t("没有连接", "Nobody connected").to_string()
        };
        let disconnect = button(t("断开", "Disconnect"), || {
            crate::menubar::disconnect(&state());
            render();
        });
        disconnect.setEnabled(now.connections > 0);
        section(
            &left,
            t("远程", "REMOTE"),
            &[
                row(Some(("person.2.fill", NSColor::systemIndigoColor())), &who, None, Some(&disconnect)),
                row(
                    Some(("pause.circle.fill", NSColor::systemOrangeColor())),
                    t("暂停远程", "Pause remote"),
                    Some(t("不接受新连接；已连着的不断开", "No new connections; the ones up stay")),
                    Some(&switch(now.paused, |v| {
                        crate::menubar::set_paused(mtm(), &state(), v);
                        render();
                    })),
                ),
            ],
            None,
        );
    }

    // connection
    let remote = crate::settings::remote(&st);
    let (rdp_port, web_port) = crate::settings::ports(&st);
    let addrs = gui::addresses();
    let addr_text = if addrs.is_empty() { "-".to_string() } else { addrs.join(", ") };
    let first_addr = addrs.first().cloned().unwrap_or_default();
    section(
        &left,
        t("连接", "CONNECTION"),
        &[
            row(
                Some(("network", NSColor::systemBlueColor())),
                &addr_text,
                Some(&if remote { format!("RDP {rdp_port} · kRDPclient {web_port}") } else { t("远程连接已关闭，没有开放端口", "Remote access is off: no port is open").to_string() }),
                Some(&button(t("拷贝", "Copy"), move || copy(first_addr.clone()))),
            ),
            row(
                Some(("number", NSColor::systemGrayColor())),
                t("端口", "Ports"),
                Some(t("443 被别的程序占用时可以换", "Change them when another program has 443")),
                Some(&button(t("更改…", "Change…"), move || {
                    if let Some((r, w)) = gui::ports_dialog(mtm(), rdp_port, web_port) {
                        match crate::settings::set_ports(&state(), r, w) {
                            Ok(()) => gui::restart_service(),
                            Err(e) => gui::error(mtm(), t("没能保存", "Could not save"), &format!("{e:#}")),
                        }
                    }
                    render();
                })),
            ),
            row(
                Some(("key.fill", NSColor::systemOrangeColor())),
                t("kRDPclient token", "kRDPclient token"),
                Some(t("添加主机时直接填 Mac 密码也行", "Typing the Mac password in Add host works too")),
                Some(&as_view(&hstack(
                    &[
                        &as_view(&button(t("列表…", "List…"), || go(Page::Tokens))),
                        &as_view(&button(t("新建…", "New…"), move || gui::token_dialog(mtm(), &state()))),
                    ],
                    8.0,
                ))),
            ),
        ],
        None,
    );

    // settings
    let feature = |key: &'static str| crate::settings::on(&st, key, "MACRDP_NONE");
    let set = |key: &'static str| {
        move |v: bool| {
            if let Err(e) = crate::settings::set(&state(), key, v) {
                gui::error(mtm(), t("没能保存", "Could not save"), &format!("{e:#}"));
            }
            render();
        }
    };
    let ssh = gui::ssh_on();
    section(
        &right,
        t("设置", "SETTINGS"),
        &[
            row(
                Some(("antenna.radiowaves.left.and.right", NSColor::systemBlueColor())),
                t("允许远程连接", "Remote access"),
                Some(t("关掉后不开放任何端口，虚拟机只能在这台 Mac 上用", "Off: no port is open; the virtual machines are used on this Mac only")),
                Some(&switch(crate::settings::remote(&st), |v| {
                    match crate::settings::set(&state(), "remote", v) {
                        // the service reads it when it starts
                        Ok(()) => gui::restart_service(),
                        Err(e) => gui::error(mtm(), t("没能保存", "Could not save"), &format!("{e:#}")),
                    }
                    render();
                })),
            ),
            row(
                Some(("desktopcomputer", NSColor::systemIndigoColor())),
                t("远程连接这台 Mac 的桌面", "Remote access to this Mac's desktop"),
                Some(t("关掉后只能连虚拟机，这台 Mac 自己连不上", "Off: only the virtual machines can be connected to, not this Mac")),
                Some(&switch(crate::settings::host_desktop(&st), set("host_desktop"))),
            ),
            row(
                Some(("power", NSColor::systemGreenColor())),
                t("登录时自动运行", "Run at login"),
                Some(t("关掉会停止服务，断开当前连接", "Off stops the service and its sessions")),
                Some(&switch(installed, |v| {
                    if v {
                        if let Err(e) = crate::install(&state()) {
                            gui::error(mtm(), t("没能装上自启", "Could not install the login item"), &format!("{e:#}"));
                        }
                    } else if gui::confirm_uninstall(mtm()) {
                        if let Err(e) = crate::uninstall() {
                            gui::error(mtm(), t("卸载失败", "Uninstall failed"), &format!("{e:#}"));
                        }
                    }
                    render();
                })),
            ),
            row(
                Some(("film.fill", NSColor::systemPurpleColor())),
                t("H.264 硬件视频", "H.264 hardware video"),
                Some(t("kRDPclient 用；关掉则走 RemoteFX", "For kRDPclient; off falls back to RemoteFX")),
                Some(&switch(feature("video"), set("video"))),
            ),
            row(
                Some(("speaker.wave.2.fill", NSColor::systemPinkColor())),
                t("声音", "Sound"),
                Some(t("把这台 Mac 的声音传到客户端", "Plays this Mac's sound on the client")),
                Some(&switch(feature("sound"), set("sound"))),
            ),
            row(
                Some(("speaker.slash.fill", NSColor::systemOrangeColor())),
                t("远程时 Mac 静音", "Mute the Mac while remote"),
                Some(t(
                    "有连接时这台 Mac 不出声，立即生效；客户端响不响看客户端自己的声音开关",
                    "No sound from this Mac while connected, at once; the client's own sound switch decides there",
                )),
                Some(&switch(feature("mute_mac"), set("mute_mac"))),
            ),
            row(
                Some(("doc.on.clipboard.fill", NSColor::systemTealColor())),
                t("剪贴板", "Clipboard"),
                Some(t("文字和图片双向复制粘贴", "Copy and paste text and pictures both ways")),
                Some(&switch(feature("clip"), set("clip"))),
            ),
            row(
                Some(("terminal.fill", NSColor::systemGrayColor())),
                t("远程登录（SSH）", "Remote Login (SSH)"),
                Some(t("在 kRDPclient 里开这台 Mac 的终端", "A terminal on this Mac from kRDPclient")),
                Some(&switch(ssh, move |_| {
                    if ssh {
                        gui::open(gui::SETTINGS_SHARING);
                    } else {
                        gui::ssh_dialog(mtm(), false);
                    }
                    render();
                })),
            ),
        ],
        Some(t(
            "视频、声音、剪贴板从下一次连接起生效。",
            "Video, sound and clipboard apply from the next connection on.",
        )),
    );

    // permissions
    let host = crate::settings::host_desktop(&st);
    section(
        &left,
        t("权限", "PERMISSIONS"),
        &permission_rows(),
        (!host).then_some(t("只连虚拟机时这两项都不需要。", "Neither is needed while only the virtual machines are served.")),
    );

    // account
    let creds = crate::account::read(&st).unwrap_or_default();
    let (sub, warn) = if !creds.is_system() {
        (t("单独设的 hyperMAC 密码（hypermac passwd）", "A separate hyperMAC password (hypermac passwd)"), false)
    } else if creds.hash_current() {
        (t("密码 = 这台 Mac 的登录密码", "Password: this Mac's login password"), false)
    } else {
        (
            t(
                "Mac 密码改过了：kRDPclient 照常；mstsc 要先用新密码登录一次 kRDPclient，或在这里重新输入",
                "The Mac password changed: kRDPclient works; mstsc needs one kRDPclient login with the new one, or type it here",
            ),
            true,
        )
    };
    section(
        &right,
        t("账号", "ACCOUNT"),
        &[row(
            Some(if warn {
                ("exclamationmark.triangle.fill", NSColor::systemOrangeColor())
            } else {
                ("person.fill", NSColor::systemBlueColor())
            }),
            &creds.user,
            Some(sub),
            Some(&button(t("重新输入密码…", "Type password again…"), || {
                let _ = gui::password_dialog(mtm(), &state());
                render();
            })),
        )],
        None,
    );


    // the shorter column's spare height goes to an empty view at its end, not into its cards
    for c in [&left, &right] {
        let fill = NSView::new(mtm());
        fill.setContentHuggingPriority_forOrientation(1.0, NSLayoutConstraintOrientation::Vertical);
        fill.setContentCompressionResistancePriority_forOrientation(1.0, NSLayoutConstraintOrientation::Vertical);
        c.addArrangedSubview(&fill);
    }
    let columns = hstack(&[&left, &right], 20.0);
    columns.setAlignment(NSLayoutAttribute::Top);
    root.addArrangedSubview(&columns);

    let _ = zh();
}

// ---- Check for Updates ----------------------------------------------------------------------

fn upd_status(upd: &Upd) -> String {
    match upd {
        Upd::Idle => match crate::update::running_tag() {
            Some(_) => t("从 GitHub 上的 hyperMAC 检查新版本", "Checks hyperMAC on GitHub for a newer version").into(),
            None => t("开发版（没有发布标签）", "Development build (no release tag)").into(),
        },
        Upd::Checking => t("正在检查…", "Checking…").into(),
        Upd::UpToDate => t("✓ 已是最新版本", "✓ Up to date").into(),
        Upd::Installed(tag, _) => format!(
            "{} {}",
            short_tag(tag),
            t("已装好，重启后生效（正在连着的会话会断开几秒）", "is installed; it takes over after a restart (a session drops for a few seconds)")
        ),
        Upd::Downloading(p, total) => {
            let got = p.received.load(Ordering::Relaxed);
            if *total > 0 && got < *total {
                format!("{} {}%", t("正在下载新版本…", "Downloading the new version…"), got * 100 / total)
            } else {
                t("正在检查并安装…", "Checking and installing…").into()
            }
        }
    }
}

/// The subtitle label of a row built by `row` (its texts stack's second view).
fn find_subtitle(row_view: &NSView) -> Option<Retained<NSTextField>> {
    let subviews = row_view.subviews();
    for v in subviews.iter() {
        if let Some(stack) = v.downcast_ref::<NSStackView>() {
            let inner = stack.arrangedSubviews();
            if inner.count() >= 2 {
                if let Ok(l) = inner.objectAtIndex(1).downcast::<NSTextField>() {
                    return Some(l);
                }
            }
        }
    }
    None
}

fn set_upd(upd: Upd) {
    with(|u| u.upd = upd);
    render();
}

fn check_for_updates() {
    set_upd(Upd::Checking);
    std::thread::spawn(|| {
        let result = crate::update::check().map_err(|e| format!("{e:#}"));
        dispatch2::DispatchQueue::main().exec_async(move || on_checked(result));
    });
}

fn on_checked(result: Result<Option<crate::update::Release>, String>) {
    let release = match result {
        Ok(Some(r)) => r,
        Ok(None) => return set_upd(Upd::UpToDate),
        Err(e) => {
            set_upd(Upd::Idle);
            return gui::error(mtm(), t("没能检查更新", "Cannot check for updates"), &e);
        }
    };
    let mut text = String::new();
    if !release.notes.is_empty() {
        let notes: String = release.notes.chars().take(700).collect();
        text.push_str(&notes);
        text.push_str("\n\n");
    }
    if !release.changes.is_empty() {
        text.push_str(if release.partial { t("最近的改动：\n", "Latest changes:\n") } else { t("这次的改动：\n", "What changed:\n") });
        for c in release.changes.iter().take(15) {
            text.push_str(&format!("• {c}\n"));
        }
        if release.changes.len() > 15 {
            text.push_str("…\n");
        }
        text.push('\n');
    }
    text.push_str(t(
        "更新会下载新版本、核对签名，替换「应用程序」里的 hyperMAC；装好后再问你要不要立即重启（重启时正在连着的会话会断开几秒）。授权不用重新给。",
        "Updating downloads the new version, checks its signature and replaces hyperMAC in Applications; then it asks whether to restart now (a session in progress drops for a few seconds then). The permissions stay.",
    ));
    let title = format!("{} {}", t("有新版本：", "New version:"), short_tag(&release.tag));
    if gui::ask(mtm(), &title, &text, &[t("以后再说", "Later"), t("更新", "Update")]) != 1 {
        return set_upd(Upd::Idle);
    }
    start_install(release);
}

/// "hypermac-0.1-8dd0ff0-20261109T064810Z-macos-arm64" -> "0.1 (8dd0ff0, 2026-11-09)"
fn short_tag(tag: &str) -> String {
    let p: Vec<&str> = tag.split('-').collect();
    match p.as_slice() {
        [_, mm, commit, time, ..] if time.len() >= 8 => format!("{mm} ({commit}, {}-{}-{})", &time[..4], &time[4..6], &time[6..8]),
        _ => tag.to_string(),
    }
}

fn start_install(release: crate::update::Release) {
    let Some(app) = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).and_then(|e| crate::app_bundle(&e)) else {
        return gui::error(mtm(), t("没能更新", "Cannot update"), t("找不到 hyperMAC.app", "hyperMAC.app not found"));
    };
    let progress = Arc::new(crate::update::Progress::default());
    set_upd(Upd::Downloading(progress.clone(), release.size));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // the status line, twice a second while it runs
    {
        let done = done.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                dispatch2::DispatchQueue::main().exec_async(|| {
                    if let Some((l, upd)) = UI.with(|u| u.borrow().as_ref().and_then(|u| u.upd_label.clone().map(|l| (l, u.upd.clone())))) {
                        l.setStringValue(&ns(&upd_status(&upd)));
                    }
                });
            }
        });
    }
    let tag = release.tag.clone();
    std::thread::spawn(move || {
        let result = crate::update::install(&release, &app, progress).map_err(|e| format!("{e:#}"));
        done.store(true, Ordering::Relaxed);
        dispatch2::DispatchQueue::main().exec_async(move || on_installed(result, app, tag));
    });
}

fn on_installed(result: Result<(), String>, app: PathBuf, tag: String) {
    if let Err(e) = result {
        set_upd(Upd::Idle);
        return gui::error(mtm(), t("更新没有完成", "The update did not complete"), &e);
    }
    // ask before the restart: whoever is connected through hyperMAC drops for a few seconds
    let title = format!("{} {}", short_tag(&tag), t("已经装好", "is installed"));
    let text = t(
        "现在重启 hyperMAC 就换成新版本：后台服务重启，正在通过 hyperMAC 连着的会话会断开几秒，之后重新连上即可；这个窗口也会重新打开。\n\n选「稍后」的话，新版本在下次重启服务（或下次打开 hyperMAC）时生效。",
        "Restart hyperMAC now to switch to the new version: the background service restarts, a session connected through hyperMAC drops for a few seconds (reconnect then), and this window reopens.\n\nWith \"Later\" the new version takes over at the next service restart (or the next time you open hyperMAC).",
    );
    if gui::ask(mtm(), &title, text, &[t("稍后", "Later"), t("立即重启", "Restart Now")]) != 1 {
        return set_upd(Upd::Installed(tag, app));
    }
    reload(app);
}

/// The service runs the app's executable: restart it on the new one, then reopen the window —
/// in the menu bar process when this is it (restarted by launchd on the new executable).
fn reload(app: PathBuf) {
    if crate::agent_plist().map(|p| p.exists()).unwrap_or(false) {
        gui::restart_service();
    }
    if crate::menubar::is_menu_process() {
        crate::menubar::restart(&state(), true);
        NSApplication::sharedApplication(mtm()).terminate(None);
        return;
    }
    let _ = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("sleep 1; /usr/bin/open -n '{}'", app.display()))
        .spawn();
    NSApplication::sharedApplication(mtm()).terminate(None);
}
