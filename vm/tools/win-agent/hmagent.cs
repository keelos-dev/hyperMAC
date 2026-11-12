// hyperMAC's agent in a Windows guest: tells the host which windows are on the screen, so the
// host can show each as a window of the Mac (vm/src/coherence.rs), and does what the host asks
// about them. C# 5 on purpose: install.cmd compiles this file in the guest with the csc.exe
// that is part of every Windows (.NET Framework 4), so no binary is shipped.
//
// The host reaches the agent through the machine's own channel, a virtio serial port named
// hypermac.agent (\\.\hypermac.agent here, a socket in the VM's folder on the host), or, in a
// guest without that port's driver, by TCP 47800. Either way it says "T <token>" first
// (agent.token beside the program). On the port nobody ever hangs up: the host asks for the
// list every second ("R"), and four seconds of silence end what it had asked for. The port is
// opened by a second copy of this program that runs as SYSTEM (`hmagent.exe relay`: Windows
// keeps such ports to administrators), which passes the bytes on. Then:
//   agent -> host, whenever something changed:
//     S <screen width> <screen height> <foreground hwnd>
//     W <hwnd> <x> <y> <width> <height> <flags> <pid> <class>\t<title>     top window first
//     E
//   flags: 1 = a popup (menu, tooltip, drop-down: never the active window), 2 = maximized
//   host -> agent:
//     A <hwnd>            make it the foreground window
//     C <hwnd>            close it
//     B 0|1               the taskbar hides itself (1) or not (0)
//     WA <l> <t> <r> <b>  the part of the screen for windows: what is outside it at the edges
//                         stays free (maximized windows stop there)
//     X <command line>    start a program
//     R                   send the list again
// Numbers are decimal; coordinates are physical pixels of the primary screen.

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

static class Agent
{
    const int Port = 47800;
    /// the relay's port on the loopback address (Relay)
    const int RelayPort = 47801;

    [StructLayout(LayoutKind.Sequential)]
    struct RECT { public int l, t, r, b; }

    [StructLayout(LayoutKind.Sequential)]
    struct APPBARDATA
    {
        public uint cbSize; public IntPtr hWnd; public uint uCallbackMessage; public uint uEdge;
        public RECT rc; public IntPtr lParam;
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] static extern bool IsZoomed(IntPtr h);
    [DllImport("user32.dll")] static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")] static extern IntPtr GetWindowLongPtr(IntPtr h, int i);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr h, int c);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] static extern IntPtr SetProcessDpiAwarenessContext(IntPtr c);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int a, out RECT r, int n);
    [DllImport("dwmapi.dll", EntryPoint = "DwmGetWindowAttribute")] static extern int DwmGetWindowAttributeInt(IntPtr h, int a, out int v, int n);
    [DllImport("shell32.dll")] static extern UIntPtr SHAppBarMessage(uint msg, ref APPBARDATA d);

    const int GWL_STYLE = -16, GWL_EXSTYLE = -20;
    const long WS_EX_TOOLWINDOW = 0x80, WS_EX_NOACTIVATE = 0x08000000, WS_EX_TRANSPARENT = 0x20, WS_POPUP = 0x80000000, WS_CAPTION = 0x00C00000;
    const int DWMWA_EXTENDED_FRAME_BOUNDS = 9, DWMWA_CLOAKED = 14;

    // the desktop and the taskbar are not windows of a program
    static readonly HashSet<string> Shell = new HashSet<string> { "Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd" };

    static volatile bool again;
    static bool barWasAutoHide, barChanged;
    // The room the host wants kept free at the screen's edges (the Mac's menu bar and Dock are
    // there): left, top, right, bottom, in pixels. Kept free the way a taskbar keeps its own:
    // an "app bar" at each edge, a window that is never shown. (Setting the work area directly,
    // SPI_SETWORKAREA, does not last: Explorer puts its own back.)
    static readonly object barsLock = new object();
    static int[] want = new int[4];
    static int[] have = new int[4];
    static IntPtr[] bar = new IntPtr[4];
    static int haveW, haveH;

    [StructLayout(LayoutKind.Sequential)]
    struct MSG { public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public int x, y; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr CreateWindowEx(uint ex, string cls, string name, uint style, int x, int y, int w, int h, IntPtr parent, IntPtr menu, IntPtr inst, IntPtr param);
    [DllImport("user32.dll")] static extern bool DestroyWindow(IntPtr h);
    [DllImport("user32.dll")] static extern bool PeekMessage(out MSG m, IntPtr h, uint min, uint max, uint remove);
    [DllImport("user32.dll")] static extern IntPtr DispatchMessage(ref MSG m);

    /// One thread owns the app bars' windows and answers their messages.
    static void Bars()
    {
        while (true)
        {
            MSG m;
            while (PeekMessage(out m, IntPtr.Zero, 0, 0, 1)) DispatchMessage(ref m);
            int sw = GetSystemMetrics(0), sh = GetSystemMetrics(1);
            int[] w;
            lock (barsLock) w = (int[])want.Clone();
            for (int e = 0; e < 4; e++)
            {
                // edges as the shell numbers them: 0 left, 1 top, 2 right, 3 bottom
                if (w[e] == have[e] && (w[e] == 0 || (sw == haveW && sh == haveH))) continue;
                APPBARDATA d = new APPBARDATA();
                d.cbSize = (uint)Marshal.SizeOf(typeof(APPBARDATA));
                if (w[e] <= 0)
                {
                    if (bar[e] != IntPtr.Zero)
                    {
                        d.hWnd = bar[e];
                        SHAppBarMessage(1 /* ABM_REMOVE */, ref d);
                        DestroyWindow(bar[e]);
                        bar[e] = IntPtr.Zero;
                    }
                    have[e] = 0;
                    continue;
                }
                if (bar[e] == IntPtr.Zero)
                {
                    bar[e] = CreateWindowEx(0x80 /* WS_EX_TOOLWINDOW */, "Static", "hyperMAC edge", 0x80000000 /* WS_POPUP */, 0, 0, 0, 0, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
                    d.hWnd = bar[e];
                    d.uCallbackMessage = 0x8000 + 77;
                    SHAppBarMessage(0 /* ABM_NEW */, ref d);
                }
                d.hWnd = bar[e];
                d.uEdge = (uint)e;
                d.rc.l = e == 2 ? sw - w[e] : 0;
                d.rc.t = e == 3 ? sh - w[e] : 0;
                d.rc.r = e == 0 ? w[e] : sw;
                d.rc.b = e == 1 ? w[e] : sh;
                SHAppBarMessage(2 /* ABM_QUERYPOS */, ref d);
                SHAppBarMessage(3 /* ABM_SETPOS */, ref d);
                have[e] = w[e];
            }
            haveW = sw; haveH = sh;
            Thread.Sleep(50);
        }
    }

    static string Class(IntPtr h)
    {
        StringBuilder s = new StringBuilder(128);
        GetClassName(h, s, s.Capacity);
        return s.ToString();
    }

    /// The windows on the screen, top first, as the lines of one message.
    static string Snapshot()
    {
        int sw = GetSystemMetrics(0), sh = GetSystemMetrics(1);
        StringBuilder o = new StringBuilder();
        o.Append("S ").Append(sw).Append(' ').Append(sh).Append(' ').Append(GetForegroundWindow().ToInt64()).Append('\n');
        // Every top-level window, top first. Not EnumWindows: that leaves out the shell's own
        // (the Start menu, the search, the notification panes), which are windows to show too.
        IntPtr h = IntPtr.Zero;
        for (int guard = 0; guard < 4096; guard++)
        {
            h = FindWindowEx(IntPtr.Zero, h, null, null);
            if (h == IntPtr.Zero) break;
            if (!IsWindowVisible(h) || IsIconic(h)) continue;
            int cloaked;
            if (DwmGetWindowAttributeInt(h, DWMWA_CLOAKED, out cloaked, 4) == 0 && cloaked != 0) continue;
            string cls = Class(h);
            if (Shell.Contains(cls)) continue;
            long ex = GetWindowLongPtr(h, GWL_EXSTYLE).ToInt64(), style = GetWindowLongPtr(h, GWL_STYLE).ToInt64();
            // what the frame looks like on the screen (GetWindowRect has the invisible resize borders in it)
            RECT r;
            if (DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, out r, 16) != 0 && !GetWindowRect(h, out r)) continue;
            int w = r.r - r.l, ht = r.b - r.t;
            if (w <= 0 || ht <= 0) continue;
            // click-through overlays that cover the screen (input indicators and the like)
            if ((ex & WS_EX_TRANSPARENT) != 0 && w >= sw && ht >= sh) continue;
            // The shell's panes (the Start menu, the notification center) each come with a
            // see-through window over the whole screen behind them, to catch the click that
            // closes them. Shown on the Mac it would be the guest's desktop over everything.
            if (cls == "Windows.UI.Core.CoreWindow" && w >= sw && ht >= sh * 9 / 10) continue;
            // Windows 11's own overlays (the snap layouts bar that comes down while a window is
            // dragged, the Alt-Tab and task views): a wide window that is mostly see-through, so
            // the desktop behind it would show on the Mac as a slab over its windows
            if (cls == "XamlExplorerHostIslandWindow") continue;
            StringBuilder t = new StringBuilder(256);
            GetWindowText(h, t, t.Capacity);
            int flags = 0;
            bool popup = (ex & WS_EX_NOACTIVATE) != 0 || cls == "#32768" || cls.StartsWith("tooltips_class") ||
                ((ex & WS_EX_TOOLWINDOW) != 0 && (style & WS_CAPTION) != WS_CAPTION) || cls == "ComboLBox" || cls == "DropDown";
            if (popup) flags |= 1;
            if (IsZoomed(h)) flags |= 2;
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            o.Append("W ").Append(h.ToInt64()).Append(' ').Append(r.l).Append(' ').Append(r.t).Append(' ').Append(w).Append(' ').Append(ht)
                .Append(' ').Append(flags).Append(' ').Append(pid).Append(' ').Append(cls.Replace(' ', '_')).Append('\t')
                .Append(t.ToString().Replace('\t', ' ').Replace('\n', ' ').Replace('\r', ' ')).Append('\n');
        }
        o.Append("E\n");
        return o.ToString();
    }

    static void Foreground(IntPtr h)
    {
        if (!IsWindow(h) || GetForegroundWindow() == h) return;
        // Windows lets a program in the background take the foreground only right after input
        // of its own: a tap on Alt is that input
        keybd_event(0x12, 0, 0, UIntPtr.Zero);
        keybd_event(0x12, 0, 2, UIntPtr.Zero);
        if (IsIconic(h)) ShowWindow(h, 9);
        SetForegroundWindow(h);
    }

    static void Taskbar(bool hide)
    {
        APPBARDATA d = new APPBARDATA();
        d.cbSize = (uint)Marshal.SizeOf(typeof(APPBARDATA));
        bool was = (SHAppBarMessage(4 /* ABM_GETSTATE */, ref d).ToUInt64() & 1) != 0;
        if (!barChanged) { barWasAutoHide = was; barChanged = true; }
        d.lParam = (IntPtr)(hide ? 1 : 0);
        SHAppBarMessage(10 /* ABM_SETSTATE */, ref d);
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, string l, uint flags, uint ms, out IntPtr result);
    const string Advanced = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced", Ours = "Software\\hyperMAC";

    /// Windows 11's snap layouts bar (it comes down from the top of the screen while a window is
    /// dragged, and the window shrinks to a thumbnail over it) off while the Mac shows the
    /// windows: the Mac's window is a cut of the screen where Windows says the window is, so it
    /// would show the desktop with a small window in it. What the setting was is kept in the
    /// registry, so an agent that was killed in between still puts it back the next time.
    static void SnapBar(bool off)
    {
        try
        {
            using (Microsoft.Win32.RegistryKey adv = Microsoft.Win32.Registry.CurrentUser.CreateSubKey(Advanced))
            using (Microsoft.Win32.RegistryKey ours = Microsoft.Win32.Registry.CurrentUser.CreateSubKey(Ours))
            {
                object was = ours.GetValue("SnapBarWas");
                if (off)
                {
                    if (was == null) ours.SetValue("SnapBarWas", adv.GetValue("EnableSnapBar", 1), Microsoft.Win32.RegistryValueKind.DWord);
                    adv.SetValue("EnableSnapBar", 0, Microsoft.Win32.RegistryValueKind.DWord);
                }
                else
                {
                    if (was == null) return;
                    adv.SetValue("EnableSnapBar", was, Microsoft.Win32.RegistryValueKind.DWord);
                    ours.DeleteValue("SnapBarWas", false);
                }
            }
            // Explorer reads the setting again when it is told that settings changed
            // (on a thread of its own: the call waits for every window on the desktop in turn.
            // Not SendNotifyMessage: a message with a string in it cannot be sent without waiting)
            Thread t = new Thread(delegate ()
            {
                IntPtr r;
                SendMessageTimeout((IntPtr)0xffff, 0x001A /* WM_SETTINGCHANGE */, IntPtr.Zero, "TraySettings", 2 /* SMTO_ABORTIFHUNG */, 1000, out r);
            });
            t.IsBackground = true;
            t.Start();
        }
        catch (Exception) { }
    }

    /// The host went away: the desktop as it was.
    static void Restore()
    {
        lock (barsLock) want = new int[4];
        SnapBar(false);
        if (barChanged) { Taskbar(barWasAutoHide); barChanged = false; }
    }

    static void Command(string line)
    {
        string[] a = line.Split(new char[] { ' ' }, 2);
        string rest = a.Length > 1 ? a[1] : "";
        long n;
        switch (a[0])
        {
            case "A": if (long.TryParse(rest, out n)) Foreground((IntPtr)n); break;
            case "C": if (long.TryParse(rest, out n)) PostMessage((IntPtr)n, 0x0112 /* WM_SYSCOMMAND */, (IntPtr)0xF060 /* SC_CLOSE */, IntPtr.Zero); break;
            case "B": Taskbar(rest == "1"); SnapBar(rest == "1"); break;
            case "WA":
                string[] p = rest.Split(' ');
                RECT r = new RECT();
                if (p.Length == 4 && int.TryParse(p[0], out r.l) && int.TryParse(p[1], out r.t) && int.TryParse(p[2], out r.r) && int.TryParse(p[3], out r.b))
                {
                    int sw = GetSystemMetrics(0), sh = GetSystemMetrics(1);
                    int[] w = new int[] { Math.Max(0, r.l), Math.Max(0, r.t), Math.Max(0, sw - r.r), Math.Max(0, sh - r.b) };
                    // (a third of the screen at most: a wrong number must not leave Windows without room)
                    for (int e = 0; e < 4; e++) w[e] = Math.Min(w[e], (e % 2 == 0 ? sw : sh) / 3);
                    lock (barsLock) want = w;
                }
                break;
            case "X":
                try
                {
                    ProcessStartInfo si = new ProcessStartInfo("cmd.exe", "/c start \"\" " + rest);
                    si.CreateNoWindow = true; si.UseShellExecute = false;
                    Process.Start(si);
                }
                catch (Exception) { }
                break;
            case "R": again = true; break;
        }
    }

    static void Serve(TcpClient c, string token)
    {
        c.NoDelay = true;
        NetworkStream s = c.GetStream();
        StreamReader rd = new StreamReader(s, new UTF8Encoding(false));
        // who is it? Ten seconds to say so
        s.ReadTimeout = 10000;
        string hello = rd.ReadLine();
        if (hello == null || hello != "T " + token) return;
        s.ReadTimeout = Timeout.Infinite;
        bool gone = false;
        Thread t = new Thread(delegate ()
        {
            try { string l; while ((l = rd.ReadLine()) != null) Command(l); } catch (Exception) { }
            gone = true;
        });
        t.IsBackground = true;
        t.Start();
        string last = "";
        UTF8Encoding utf8 = new UTF8Encoding(false);
        try
        {
            while (!gone)
            {
                string now = Snapshot();
                if (again || now != last)
                {
                    again = false;
                    last = now;
                    byte[] b = utf8.GetBytes(now);
                    s.Write(b, 0, b.Length);
                }
                Thread.Sleep(16);
            }
        }
        catch (Exception) { }
    }

    // ---- the machine's own channel -------------------------------------------------------------

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFile(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool ReadFile(IntPtr h, IntPtr buf, uint n, IntPtr done, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool WriteFile(IntPtr h, IntPtr buf, uint n, IntPtr done, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetOverlappedResult(IntPtr h, IntPtr overlapped, out uint done, bool wait);
    [DllImport("kernel32.dll")] static extern IntPtr CreateEvent(IntPtr security, bool manual, bool initial, IntPtr name);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);

    /// One direction of the port (in the relay). Reading and writing go on at the same time from two threads,
    /// which a handle allows only with overlapped I/O (on a plain one the write would wait for
    /// the read to end): each direction has its OVERLAPPED, its event and its buffer, in memory
    /// the garbage collector does not move.
    class Half
    {
        public readonly IntPtr Buf;
        readonly int size;
        readonly IntPtr ov, ev;
        public Half(int bytes)
        {
            size = bytes;
            Buf = Marshal.AllocHGlobal(bytes);
            ov = Marshal.AllocHGlobal(2 * IntPtr.Size + 8 + IntPtr.Size);
            ev = CreateEvent(IntPtr.Zero, true, false, IntPtr.Zero);
        }
        /// Read up to the buffer's size, or write its first n bytes: how many it was, -1 = the port is gone.
        public int Io(IntPtr h, bool write, int n)
        {
            for (int i = 0; i < 2 * IntPtr.Size + 8 + IntPtr.Size; i++) Marshal.WriteByte(ov, i, 0);
            Marshal.WriteIntPtr(ov, 2 * IntPtr.Size + 8, ev);
            bool ok = write ? WriteFile(h, Buf, (uint)n, IntPtr.Zero, ov) : ReadFile(h, Buf, (uint)size, IntPtr.Zero, ov);
            if (!ok && Marshal.GetLastWin32Error() != 997 /* ERROR_IO_PENDING */) return -1;
            uint done;
            return GetOverlappedResult(h, ov, out done, true) ? (int)done : -1;
        }
    }

    /// `hmagent.exe relay`, a service (as SYSTEM, from the start of Windows; install.cmd makes it): the
    /// port's other end for the agent. Windows lets only administrators and the system open a
    /// virtio serial port, and the agent is a program of whoever is logged on: this one opens
    /// the port and hands its bytes on over the loopback address, where the agent connects
    /// (Channel). Nothing of it leaves the machine.
    class RelayService : System.ServiceProcess.ServiceBase
    {
        protected override void OnStart(string[] args)
        {
            Thread t = new Thread(Relay);
            t.IsBackground = true;
            t.Start();
        }
    }

    /// What the relay did, the first few times (relay.log beside the program): it has no
    /// window and no console to say it in.
    static int logged;
    static void Log(string what)
    {
        if (logged++ > 200) return;
        try
        {
            string dir = Path.GetDirectoryName(System.Reflection.Assembly.GetExecutingAssembly().Location);
            File.AppendAllText(Path.Combine(dir, "relay.log"), DateTime.Now.ToString("HH:mm:ss ") + what + "\r\n");
        }
        catch (Exception) { }
    }

    /// Log, unless it is what was said last.
    static string said;
    static void Say(string what)
    {
        if (what == said) return;
        said = what;
        Log(what);
    }

    static void Relay()
    {
        bool first;
        Mutex one = new Mutex(true, "Global\\hyperMACAgentRelay", out first);
        if (!first) return;
        TcpListener l = new TcpListener(IPAddress.Loopback, RelayPort);
        l.Start();
        object gate = new object();
        NetworkStream client = null;
        Thread accept = new Thread(delegate ()
        {
            while (true)
            {
                try
                {
                    TcpClient c = l.AcceptTcpClient();
                    c.NoDelay = true;
                    lock (gate) { if (client != null) try { client.Close(); } catch (Exception) { } client = c.GetStream(); }
                }
                catch (Exception) { Thread.Sleep(500); }
            }
        });
        accept.IsBackground = true;
        accept.Start();
        while (true)
        {
            // GENERIC_READ | GENERIC_WRITE, OPEN_EXISTING, FILE_FLAG_OVERLAPPED
            IntPtr h = CreateFile("\\\\.\\hypermac.agent", 0xC0000000, 0, IntPtr.Zero, 3, 0x40000000, IntPtr.Zero);
            if (h == (IntPtr)(-1)) { Log("the port does not open: error " + Marshal.GetLastWin32Error()); Thread.Sleep(3000); continue; }   // no such port (yet): no driver, or an older machine
            Log("the port is open");
            bool broken = false;
            // the host's bytes to the agent, when one is connected (else they are dropped: the host says everything again)
            Thread down = new Thread(delegate ()
            {
                Half r = new Half(4096);
                byte[] b = new byte[4096];
                while (!broken)
                {
                    int n = r.Io(h, false, 0);
                    if (n < 0)
                    {
                        int e = Marshal.GetLastWin32Error();
                        // 1450: the driver's answer while nobody is connected to the port on the host. Not an end.
                        if (e == 1450) { Say("waiting for the host to connect to the port"); Thread.Sleep(300); continue; }
                        Log("reading the port: error " + e);
                        broken = true;
                        break;
                    }
                    Say("the host is there");
                    if (n == 0) { Thread.Sleep(20); continue; }
                    Marshal.Copy(r.Buf, b, 0, n);
                    NetworkStream to;
                    lock (gate) to = client;
                    if (to == null) continue;
                    try { to.Write(b, 0, n); } catch (Exception) { lock (gate) if (client == to) client = null; }
                }
            });
            down.IsBackground = true;
            down.Start();
            // the agent's bytes to the host
            Half w = new Half(4096);
            byte[] up = new byte[4096];
            while (!broken)
            {
                NetworkStream from;
                lock (gate) from = client;
                if (from == null) { Thread.Sleep(50); continue; }
                int n;
                try { from.ReadTimeout = 500; n = from.Read(up, 0, up.Length); }
                catch (IOException x)
                {
                    // nothing from the agent for half a second: look at the port's state and wait on
                    SocketException se = x.InnerException as SocketException;
                    if (se != null && se.SocketErrorCode == SocketError.TimedOut) continue;
                    n = 0;
                }
                catch (Exception) { n = 0; }
                if (n <= 0) { lock (gate) if (client == from) client = null; continue; }
                for (int at = 0; at < n && !broken;)
                {
                    Marshal.Copy(up, at, w.Buf, n - at);
                    int done = w.Io(h, true, n - at);
                    if (done <= 0)
                    {
                        // (nobody on the host's side: what the agent said is dropped, as the host's words are when no agent listens)
                        int e = Marshal.GetLastWin32Error();
                        if (e == 1450) break;
                        Log("writing the port: error " + e);
                        broken = true;
                    }
                    else at += done;
                }
            }
            CloseHandle(h);
            Thread.Sleep(1000);
        }
    }

    /// The agent's side of the machine's own channel: the relay on the loopback address.
    static void Channel(string token)
    {
        UTF8Encoding utf8 = new UTF8Encoding(false);
        while (true)
        {
            TcpClient c = new TcpClient();
            try { c.Connect(IPAddress.Loopback, RelayPort); } catch (Exception) { Thread.Sleep(3000); continue; }   // no relay: installed before there was one
            c.NoDelay = true;
            NetworkStream s = c.GetStream();
            bool broken = false, session = false;
            int heard = 0;
            Thread rd = new Thread(delegate ()
            {
                try
                {
                    StreamReader lines = new StreamReader(s, utf8);
                    string l;
                    while ((l = lines.ReadLine()) != null)
                    {
                        if (l.StartsWith("T "))
                        {
                            session = l == "T " + token;
                            again = true;
                            heard = Environment.TickCount;
                        }
                        else if (session)
                        {
                            heard = Environment.TickCount;
                            try { Command(l); } catch (Exception) { }
                        }
                    }
                }
                catch (Exception) { }
                broken = true;
            });
            rd.IsBackground = true;
            rd.Start();
            string last = "";
            try
            {
                while (!broken)
                {
                    if (session && unchecked(Environment.TickCount - heard) > 4000)
                    {
                        // the host is gone (it says something every second; on the port nobody hangs up)
                        session = false;
                        Restore();
                    }
                    if (!session) { last = ""; Thread.Sleep(50); continue; }
                    string now = Snapshot();
                    if (again || now != last)
                    {
                        again = false;
                        last = now;
                        byte[] b = utf8.GetBytes(now);
                        s.Write(b, 0, b.Length);
                    }
                    Thread.Sleep(16);
                }
            }
            catch (Exception) { }
            if (session) Restore();
            try { c.Close(); } catch (Exception) { }
            Thread.Sleep(1000);
        }
    }

    [STAThread]
    static void Main(string[] args)
    {
        if (args.Length > 0 && args[0] == "relay") { System.ServiceProcess.ServiceBase.Run(new RelayService()); return; }
        bool first;
        Mutex one = new Mutex(true, "Local\\hyperMACAgent", out first);
        if (!first) return;
        try { SetProcessDpiAwarenessContext((IntPtr)(-4)); } catch (Exception) { }
        string dir = Path.GetDirectoryName(System.Reflection.Assembly.GetExecutingAssembly().Location);
        string token;
        try { token = File.ReadAllText(Path.Combine(dir, "agent.token")).Trim(); } catch (Exception) { return; }
        if (token.Length < 16) return;
        SnapBar(false);   // (left off by an agent that did not end properly)
        Thread bars = new Thread(Bars);
        bars.IsBackground = true;
        bars.Start();
        Thread channel = new Thread(delegate () { Channel(token); });
        channel.IsBackground = true;
        channel.Start();
        TcpListener l = new TcpListener(IPAddress.Any, Port);
        l.Start();
        while (true)
        {
            try
            {
                using (TcpClient c = l.AcceptTcpClient()) Serve(c, token);
            }
            catch (Exception) { Thread.Sleep(500); }
            Restore();
        }
    }
}
