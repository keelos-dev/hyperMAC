//! The console of a headless VM: Virtualization.framework's own VNC server (`_VZVNCServer`,
//! private; Tart's --vnc-experimental uses the same). It serves the VM's display and takes
//! keyboard and pointer without any window or view on the host.
//! It listens on every interface; the RFB password is the only protection, so the port is
//! for the install / login window / rescue, not for daily use (that is macRDP in the guest).

use anyhow::Context;
use dispatch2::DispatchQueue;
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSString;
use objc2_virtualization::VZVirtualMachine;

pub struct Vnc(Retained<AnyObject>);

impl Vnc {
    /// port 0 = any free port (read it back with port() once the server is up)
    pub fn start(vm: &VZVirtualMachine, port: u16, password: &str) -> anyhow::Result<Vnc> {
        let cls = |n: &std::ffi::CStr| AnyClass::get(n).with_context(|| format!("{n:?} is gone from Virtualization.framework"));
        unsafe {
            let sc: Allocated<AnyObject> = msg_send![cls(c"_VZVNCAuthenticationSecurityConfiguration")?, alloc];
            let sc: Retained<AnyObject> = msg_send![sc, initWithPassword: &*NSString::from_str(password)];
            let s: Allocated<AnyObject> = msg_send![cls(c"_VZVNCServer")?, alloc];
            let s: Retained<AnyObject> =
                msg_send![s, initWithPort: port, queue: &*DispatchQueue::main(), securityConfiguration: &*sc];
            let _: () = msg_send![&*s, setVirtualMachine: vm];
            let _: () = msg_send![&*s, start];
            Ok(Vnc(s))
        }
    }

    /// 0 until the listener is up
    pub fn port(&self) -> u16 {
        unsafe { msg_send![&*self.0, port] }
    }
}
