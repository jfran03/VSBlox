//! Windows implementation: find windows, capture VS Code, pin Studio.
//!
//! Two threads:
//! - The calibrator thread captures the VS Code window (~26 ms on 1440p) to
//!   find where the panel is inside it. It never touches Studio.
//! - The main thread runs a message loop. It hears about VS Code moving the
//!   moment it happens (WinEvent hook) and moves Studio right away, using the
//!   last panel offset the calibrator found.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateRectRgn, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, SetWindowRgn, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetClientRect, GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    PostThreadMessageW, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    EVENT_OBJECT_LOCATIONCHANGE, GWLP_HWNDPARENT, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, MSG,
    SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, WS_CAPTION, WS_EX_CLIENTEDGE,
    WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_WINDOWEDGE, WS_THICKFRAME,
    SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOWNOACTIVATE,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_TIMER,
};

use crate::{emit, Cmd};

/// Corner marker squares. Must match CORNER in extension/extension.js.
const CORNER: (u8, u8, u8) = (254, 1, 253);
/// Panel fill, close to Studio's dark background so any gap blends in.
/// Must match FILL in extension/extension.js.
const FILL: (u8, u8, u8) = (46, 46, 47);
const TOLERANCE: i32 = 4;
const STUDIO_EXE: &str = "RobloxStudioBeta.exe";
const PW_RENDERFULLCONTENT: u32 = 2;
/// The panel draws four 8x8 squares (in screen pixels), but VS Code zoom and
/// fractional scaling blur their edges, so only ask for a few exact pixels.
const MIN_CORNER_PIXELS: u32 = 12;
/// How far from each bounding-box corner we look for a corner pixel.
const CORNER_SLACK: i32 = 4;
/// Below this share of fill pixels inside the panel, something (a menu, the
/// command palette) is drawn over it, so Studio steps aside.
const MIN_COVERAGE: f32 = 0.92;
const OBJID_WINDOW: i32 = 0;

const CALIBRATE_EVERY: Duration = Duration::from_millis(100);
const SEARCH_HOST_EVERY: Duration = Duration::from_millis(400);
const SEARCH_STUDIO_EVERY: Duration = Duration::from_millis(500);

pub fn init() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ---------- geometry and window helpers ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        (x1 > x0 && y1 > y0).then(|| Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut r) } == 0 {
        return None;
    }
    Some(Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top })
}

/// Invisible resize borders (left, top, right, bottom) that Windows 10 adds
/// around some windows.
fn invisible_borders(hwnd: HWND) -> (i32, i32, i32, i32) {
    let Some(outer) = window_rect(hwnd) else { return (0, 0, 0, 0) };
    let mut f: RECT = unsafe { std::mem::zeroed() };
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut f as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if hr != 0 {
        return (0, 0, 0, 0);
    }
    (
        f.left - outer.x,
        f.top - outer.y,
        (outer.x + outer.w) - f.right,
        (outer.y + outer.h) - f.bottom,
    )
}

/// Space (left, top, right, bottom) between the window rect and its client
/// area: the title bar plus borders. Lining up the client area with the panel
/// and clipping the rest hides Studio's Windows title bar.
fn chrome_insets(hwnd: HWND) -> (i32, i32, i32, i32) {
    let Some(win) = window_rect(hwnd) else { return (0, 0, 0, 0) };
    let mut c: RECT = unsafe { std::mem::zeroed() };
    let mut origin = POINT { x: 0, y: 0 };
    unsafe {
        if GetClientRect(hwnd, &mut c) == 0 || ClientToScreen(hwnd, &mut origin) == 0 {
            return (0, 0, 0, 0);
        }
    }
    let (l, t) = (origin.x - win.x, origin.y - win.y);
    (l, t, win.w - c.right - l, win.h - c.bottom - t)
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid
}

fn process_exe(hwnd: HWND) -> String {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, window_pid(hwnd));
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
    }
}

/// Visible, unowned, titled top-level windows belonging to `exe`.
fn top_windows_of(exe: &str) -> Vec<HWND> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam as *mut Vec<HWND>);
        out.push(hwnd);
        1
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(cb), &mut all as *mut Vec<HWND> as LPARAM) };
    all.into_iter()
        .filter(|&h| unsafe {
            IsWindowVisible(h) != 0
                && GetWindow(h, GW_OWNER).is_null()
                && GetWindowTextLengthW(h) > 0
        })
        .filter(|&h| process_exe(h).eq_ignore_ascii_case(exe))
        .collect()
}

// ---------- finding the panel ----------

/// Where the panel sits inside the host window, relative to its window rect.
#[derive(Clone, Copy, Debug)]
struct Marker {
    rect: Rect,
    coverage: f32,
}

/// Capture the window's own rendering (works even where Studio covers it)
/// and locate the panel by its corner squares.
fn find_marker(hwnd: HWND) -> Option<Marker> {
    let r = window_rect(hwnd)?;
    if r.w <= 0 || r.h <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: r.w,
            biHeight: -r.h, // top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut c_void = null_mut();
        let dib = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let mut result = None;
        if !dib.is_null() && !bits.is_null() {
            let old = SelectObject(mem, dib);
            if PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT) != 0 {
                let px = std::slice::from_raw_parts(bits as *const u32, (r.w * r.h) as usize);
                if let Ok(dir) = std::env::var("VSBLOX_DUMP") {
                    dump_bmp(&format!("{dir}/capture-{:x}.bmp", hwnd as usize), px, r.w, r.h);
                }
                result = scan(px, r.w, r.h);
            }
            SelectObject(mem, old);
            DeleteObject(dib);
        }
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        result
    }
}

/// Debug aid: save a capture as a .bmp (set VSBLOX_DUMP to a folder).
fn dump_bmp(path: &str, px: &[u32], w: i32, h: i32) {
    let data_len = (w * h * 4) as u32;
    let mut out = Vec::with_capacity(54 + data_len as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + data_len).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&(-h).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for p in px {
        out.extend_from_slice(&p.to_le_bytes());
    }
    let _ = std::fs::write(path, out);
}

fn matches(p: u32, c: (u8, u8, u8)) -> bool {
    let b = (p & 0xFF) as i32;
    let g = ((p >> 8) & 0xFF) as i32;
    let r = ((p >> 16) & 0xFF) as i32;
    (r - c.0 as i32).abs() <= TOLERANCE
        && (g - c.1 as i32).abs() <= TOLERANCE
        && (b - c.2 as i32).abs() <= TOLERANCE
}

fn scan(px: &[u32], w: i32, h: i32) -> Option<Marker> {
    let at = |x: i32, y: i32| px[(y * w + x) as usize];
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, -1, -1);
    let mut count = 0u32;
    for y in 0..h {
        let row = &px[(y * w) as usize..((y + 1) * w) as usize];
        for (x, &p) in row.iter().enumerate() {
            if matches(p, CORNER) {
                let x = x as i32;
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
                count += 1;
            }
        }
    }
    if count < MIN_CORNER_PIXELS {
        return None;
    }
    let rect = Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 };
    // All four corners must be there, or part of the panel is covered or
    // these pixels belong to something else.
    let has_corner_near = |cx: i32, cy: i32, dx: i32, dy: i32| {
        (0..CORNER_SLACK).any(|i| {
            (0..CORNER_SLACK).any(|j| {
                let (x, y) = (cx + dx * i, cy + dy * j);
                x >= 0 && y >= 0 && x < w && y < h && matches(at(x, y), CORNER)
            })
        })
    };
    let corners_ok = has_corner_near(x0, y0, 1, 1)
        && has_corner_near(x1, y0, -1, 1)
        && has_corner_near(x0, y1, 1, -1)
        && has_corner_near(x1, y1, -1, -1);
    if !corners_ok {
        return Some(Marker { rect, coverage: 0.0 });
    }
    // Sample the inside on a grid: how much of it is still plain fill?
    let step = 6;
    let (mut total, mut plain) = (0u32, 0u32);
    let mut y = y0;
    while y <= y1 {
        let mut x = x0;
        while x <= x1 {
            let p = at(x, y);
            total += 1;
            if matches(p, FILL) || matches(p, CORNER) {
                plain += 1;
            }
            x += step;
        }
        y += step;
    }
    Some(Marker { rect, coverage: plain as f32 / total.max(1) as f32 })
}

// ---------- calibrator thread ----------

/// State the calibrator publishes for the main thread.
struct Shared {
    /// The VS Code window showing our panel (HWND as usize, 0 = none yet).
    host: AtomicUsize,
    marker: Mutex<Option<Marker>>,
}

fn calibrator(shared: Arc<Shared>, host_exe: String, main_tid: u32, wake: Receiver<()>) {
    loop {
        let host = shared.host.load(Ordering::SeqCst) as HWND;
        let (marker, wait) = if host.is_null() {
            // Our VS Code window is the one showing the panel's corners.
            let found = top_windows_of(&host_exe)
                .into_iter()
                .find_map(|h| find_marker(h).map(|m| (h, m)));
            match found {
                Some((h, m)) => {
                    shared.host.store(h as usize, Ordering::SeqCst);
                    (Some(m), CALIBRATE_EVERY)
                }
                None => (None, SEARCH_HOST_EVERY),
            }
        } else if unsafe { IsWindow(host) } == 0 {
            shared.host.store(0, Ordering::SeqCst);
            (None, SEARCH_HOST_EVERY)
        } else {
            (find_marker(host), CALIBRATE_EVERY)
        };
        *shared.marker.lock().unwrap() = marker;
        if unsafe { PostThreadMessageW(main_tid, WM_APP, 0, 0) } == 0 {
            return; // main thread is gone
        }
        match wake.recv_timeout(wait) {
            Err(RecvTimeoutError::Disconnected) => return,
            _ => {}
        }
    }
}

// ---------- main thread: docking Studio ----------

/// Studio's state before we touched it, so we can put it back.
struct Saved {
    owner: isize,
    rect: Rect,
    style: isize,
    exstyle: isize,
}

/// Window style bits that draw the title bar and frame.
const FRAME_STYLE: isize = (WS_CAPTION | WS_THICKFRAME) as isize;
const FRAME_EXSTYLE: isize =
    (WS_EX_DLGMODALFRAME | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_WINDOWEDGE) as isize;

/// Set Studio's frame style bits to `style`/`exstyle` (only the frame bits are
/// touched). Returns true if anything changed.
unsafe fn set_frame(studio: HWND, style: isize, exstyle: isize) -> bool {
    let cur = GetWindowLongPtrW(studio, GWL_STYLE);
    let cur_ex = GetWindowLongPtrW(studio, GWL_EXSTYLE);
    let new = (cur & !FRAME_STYLE) | (style & FRAME_STYLE);
    let new_ex = (cur_ex & !FRAME_EXSTYLE) | (exstyle & FRAME_EXSTYLE);
    if new == cur && new_ex == cur_ex {
        return false;
    }
    SetWindowLongPtrW(studio, GWL_STYLE, new);
    SetWindowLongPtrW(studio, GWL_EXSTYLE, new_ex);
    SetWindowPos(
        studio,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
    true
}

struct Dock {
    shared: Arc<Shared>,
    wake: Sender<()>,
    host: HWND,
    hook: HWINEVENTHOOK,
    studio: Option<HWND>,
    saved: Option<Saved>,
    want_visible: bool,
    /// Fixed Studio size in physical pixels; None = fill the panel.
    fixed_size: Option<(i32, i32)>,
    /// Hide Studio's Windows title bar by clipping it off.
    hide_title_bar: bool,
    shown: bool,
    placed: Option<Rect>,
    clip: Option<Rect>,
    last_host_size: Option<(i32, i32)>,
    last_studio_search: Instant,
    last_state: String,
}

thread_local! {
    static DOCK: RefCell<Option<Dock>> = const { RefCell::new(None) };
}

/// Called by Windows the moment any window of the VS Code process moves.
unsafe extern "system" fn on_location_change(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW {
        return;
    }
    DOCK.with(|d| {
        // Skip if we're already inside an update (re-entrant delivery).
        if let Ok(mut d) = d.try_borrow_mut() {
            if let Some(d) = d.as_mut() {
                if hwnd == d.host {
                    d.update();
                }
            }
        }
    });
}

pub fn run(host_exe: String, rx: Receiver<Option<Cmd>>) {
    let main_tid = unsafe { GetCurrentThreadId() };
    let shared = Arc::new(Shared { host: AtomicUsize::new(0), marker: Mutex::new(None) });
    let (wake_tx, wake_rx) = mpsc::channel();
    {
        let shared = shared.clone();
        std::thread::spawn(move || calibrator(shared, host_exe, main_tid, wake_rx));
    }

    let past = Instant::now() - Duration::from_secs(10);
    DOCK.with(|d| {
        *d.borrow_mut() = Some(Dock {
            shared,
            wake: wake_tx,
            host: null_mut(),
            hook: null_mut(),
            studio: None,
            saved: None,
            want_visible: true,
            fixed_size: None,
            hide_title_bar: true,
            shown: true,
            placed: None,
            clip: None,
            last_host_size: None,
            last_studio_search: past,
            last_state: String::new(),
        })
    });

    // Fallback tick: commands, Studio search, visibility changes.
    unsafe { SetTimer(null_mut(), 0, 16, None) };

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    'pump: while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        if msg.message == WM_TIMER {
            loop {
                match rx.try_recv() {
                    Ok(Some(cmd)) => {
                        if !with_dock(|d| d.command(cmd)) {
                            break 'pump;
                        }
                    }
                    Ok(None) | Err(mpsc::TryRecvError::Disconnected) => break 'pump,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        if msg.message == WM_TIMER || msg.message == WM_APP {
            with_dock(|d| d.update());
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    with_dock(|d| d.detach());
    DOCK.with(|d| {
        if let Some(d) = d.borrow_mut().take() {
            if !d.hook.is_null() {
                unsafe { UnhookWinEvent(d.hook) };
            }
        }
    });
}

fn with_dock<R: Default>(f: impl FnOnce(&mut Dock) -> R) -> R {
    DOCK.with(|d| match d.try_borrow_mut() {
        Ok(mut d) => d.as_mut().map(f).unwrap_or_default(),
        Err(_) => R::default(),
    })
}

impl Dock {
    fn status(&mut self, state: &str, detail: &str) {
        let key = format!("{state}|{detail}");
        if key != self.last_state {
            emit(state, detail);
            self.last_state = key;
        }
    }

    /// Returns false when the helper should exit.
    fn command(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Show => self.want_visible = true,
            Cmd::Hide => self.want_visible = false,
            Cmd::Recalibrate => {
                let _ = self.wake.send(());
            }
            Cmd::Size { width: Some(w), height: Some(h) } if w > 0 && h > 0 => {
                self.fixed_size = Some((w, h));
                self.placed = None;
            }
            Cmd::Size { .. } => {
                self.fixed_size = None;
                self.placed = None;
            }
            Cmd::TitleBar { hide } => {
                self.hide_title_bar = hide;
                self.placed = None;
            }
            Cmd::Detach => return false,
        }
        true
    }

    fn set_host(&mut self, host: HWND) {
        self.detach();
        if !self.hook.is_null() {
            unsafe { UnhookWinEvent(self.hook) };
            self.hook = null_mut();
        }
        self.host = host;
        self.last_host_size = None;
        if !host.is_null() {
            self.hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    null_mut(),
                    Some(on_location_change),
                    window_pid(host),
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            self.status("host_found", "");
        }
    }

    fn update(&mut self) {
        let host = self.shared.host.load(Ordering::SeqCst) as HWND;
        if host != self.host {
            self.set_host(host);
        }
        if self.host.is_null() {
            self.status("searching_host", "Looking for the VS Code panel");
            return;
        }

        if let Some(s) = self.studio {
            if unsafe { IsWindow(s) } == 0 {
                self.studio = None;
                self.saved = None;
            }
        }
        if self.studio.is_none() && self.last_studio_search.elapsed() >= SEARCH_STUDIO_EVERY {
            self.last_studio_search = Instant::now();
            let studio = top_windows_of(STUDIO_EXE)
                .into_iter()
                .max_by_key(|&h| window_rect(h).map(|r| r.w as i64 * r.h as i64).unwrap_or(0));
            if let Some(s) = studio {
                self.attach(s);
            }
        }

        // The panel offset only changes when VS Code resizes or its layout
        // changes. Ask for a fresh capture right away on resize.
        let host_rect = window_rect(self.host);
        let size = host_rect.map(|r| (r.w, r.h));
        if size != self.last_host_size {
            self.last_host_size = size;
            let _ = self.wake.send(());
        }

        let Some(studio) = self.studio else {
            self.status("waiting_for_studio", "Open Roblox Studio to dock it here");
            return;
        };

        let marker = *self.shared.marker.lock().unwrap();
        let minimized = unsafe { IsIconic(self.host) } != 0;
        let panel = match (marker, host_rect) {
            (Some(m), Some(hr)) if self.want_visible && !minimized && m.coverage >= MIN_COVERAGE => {
                Some(Rect { x: hr.x + m.rect.x, y: hr.y + m.rect.y, w: m.rect.w, h: m.rect.h })
            }
            _ => None,
        };

        let Some(panel) = panel else {
            if self.shown {
                unsafe { ShowWindow(studio, SW_HIDE) };
                self.shown = false;
            }
            let why = if !self.want_visible || minimized {
                "panel hidden"
            } else if marker.is_some() {
                "panel covered"
            } else {
                "panel not on screen"
            };
            self.status("hidden", why);
            return;
        };

        // Where Studio's visible frame should go.
        let target = match self.fixed_size {
            None => panel,
            Some((w, h)) => Rect {
                // Centered; if bigger than the panel, pinned to its top-left
                // and cut off at the panel edge.
                x: panel.x + ((panel.w - w) / 2).max(0),
                y: panel.y + ((panel.h - h) / 2).max(0),
                w,
                h,
            },
        };

        // Borderless while docked: no title bar or frame, like borderless
        // fullscreen. Re-checked every update in case Studio puts them back.
        if let Some(saved) = &self.saved {
            let (style, exstyle) = if self.hide_title_bar { (0, 0) } else { (saved.style, saved.exstyle) };
            if unsafe { set_frame(studio, style, exstyle) } {
                self.placed = None;
            }
        }

        // What surrounds the part of Studio we show. Borderless, this is all
        // zero; if Studio refuses to drop its frame, the title bar is clipped.
        let (bl, bt, br, bb) = if self.hide_title_bar {
            chrome_insets(studio)
        } else {
            invisible_borders(studio)
        };
        if self.placed != Some(target) {
            unsafe {
                SetWindowPos(
                    studio,
                    null_mut(),
                    target.x - bl,
                    target.y - bt,
                    target.w + bl + br,
                    target.h + bt + bb,
                    // ASYNC: don't wait for Studio to handle it, so a busy
                    // Studio can't stall us.
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
                );
            }
            self.placed = Some(target);
        }

        self.update_clip(studio, panel, (bl, bt, br, bb));

        if !self.shown {
            unsafe { ShowWindow(studio, SW_SHOWNOACTIVATE) };
            self.shown = true;
        }
        self.status("docked", "");
    }

    /// Show only Studio's frame (`borders` trimmed off) and only where it is
    /// inside the panel. Trims the title bar, a fixed size bigger than the
    /// panel, or a panel smaller than Studio's minimum size.
    fn update_clip(&mut self, studio: HWND, panel: Rect, borders: (i32, i32, i32, i32)) {
        let Some(win) = window_rect(studio) else { return };
        let (bl, bt, br, bb) = borders;
        let frame = Rect { x: win.x + bl, y: win.y + bt, w: win.w - bl - br, h: win.h - bt - bb };
        let vis = frame.intersect(&panel).unwrap_or(Rect { x: frame.x, y: frame.y, w: 0, h: 0 });
        let want = if vis == win {
            None // nothing to trim
        } else {
            // Region coordinates are relative to the window's top-left.
            Some(Rect { x: vis.x - win.x, y: vis.y - win.y, w: vis.w, h: vis.h })
        };
        if want == self.clip {
            return;
        }
        unsafe {
            let rgn = match want {
                Some(r) => CreateRectRgn(r.x, r.y, r.x + r.w, r.y + r.h),
                None => null_mut(),
            };
            // On success Windows owns the region; no need to delete it.
            if SetWindowRgn(studio, rgn, 1) == 0 && !rgn.is_null() {
                DeleteObject(rgn);
            }
        }
        self.clip = want;
    }

    fn attach(&mut self, studio: HWND) {
        unsafe {
            if IsZoomed(studio) != 0 || IsIconic(studio) != 0 {
                ShowWindow(studio, SW_RESTORE);
            }
            let rect = window_rect(studio).unwrap_or(Rect { x: 100, y: 100, w: 1280, h: 800 });
            let owner = GetWindowLongPtrW(studio, GWLP_HWNDPARENT);
            let style = GetWindowLongPtrW(studio, GWL_STYLE);
            let exstyle = GetWindowLongPtrW(studio, GWL_EXSTYLE);
            self.saved = Some(Saved { owner, rect, style, exstyle });
            // Owned windows always sit above their owner and minimize with it,
            // so Studio stays on top of VS Code without being "always on top".
            SetWindowLongPtrW(studio, GWLP_HWNDPARENT, self.host as isize);
        }
        self.studio = Some(studio);
        self.shown = true;
        self.placed = None;
        self.clip = None;
        self.status("attached", "");
    }

    /// Give Studio back its original owner, position, shape and visibility.
    fn detach(&mut self) {
        let (Some(studio), Some(saved)) = (self.studio.take(), self.saved.take()) else {
            return;
        };
        unsafe {
            if IsWindow(studio) != 0 {
                SetWindowRgn(studio, null_mut(), 1);
                set_frame(studio, saved.style, saved.exstyle);
                SetWindowLongPtrW(studio, GWLP_HWNDPARENT, saved.owner);
                let r = saved.rect;
                SetWindowPos(studio, null_mut(), r.x, r.y, r.w, r.h, SWP_NOZORDER | SWP_NOACTIVATE);
                ShowWindow(studio, SW_SHOWNOACTIVATE);
            }
        }
        self.placed = None;
        self.clip = None;
    }
}

/// `--probe`: print what the helper can see, without touching anything.
pub fn probe(host_exe: &str) {
    println!("Host windows ({host_exe}):");
    for h in top_windows_of(host_exe) {
        let r = window_rect(h);
        let t0 = Instant::now();
        let m = find_marker(h);
        println!("  {h:?} rect={r:?} marker={m:?} capture_ms={}", t0.elapsed().as_millis());
    }
    println!("Studio windows ({STUDIO_EXE}):");
    for h in top_windows_of(STUDIO_EXE) {
        println!(
            "  {h:?} rect={:?} borders={:?} chrome={:?}",
            window_rect(h),
            invisible_borders(h),
            chrome_insets(h)
        );
    }
}
