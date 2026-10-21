//! Native Win32 front-end.
//!
//! Everything the user sees is a real system control: `EDIT` for the two text
//! fields, `BUTTON`, a virtual `SysListView32` for the results (so 100k rows
//! cost nothing), a `msctls_progress32` for indexing and a status bar. No
//! self-drawn widgets, no GPU, no extra runtime.
//!
//! The indexing/search core is unchanged; this module only drives it.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetSysColor, GetSysColorBrush,
    GetTextExtentPoint32W, InvalidateRect, SelectObject, SetBkMode, SetTextColor,
    UpdateWindow, COLOR_BTNFACE, COLOR_BTNTEXT, DT_END_ELLIPSIS,
    DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, LOGFONTW, RDW_ALLCHILDREN, RDW_ERASE,
    RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVCFMT_LEFT,
    LVCFMT_RIGHT, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVN_GETDISPINFO,
    LVN_ITEMCHANGED, LVN_KEYDOWN, LVM_INSERTCOLUMNW, LVM_SETCOLUMNWIDTH,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMCOUNT, LVM_SETITEMSTATE, LVSICF_NOINVALIDATEALL,
    LVSICF_NOSCROLL, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_LABELTIP, LVS_OWNERDATA,
    LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NM_DBLCLK, NMHDR, NMITEMACTIVATE, NM_RCLICK,
    NMLVDISPINFOW, NMLVKEYDOWN, NMLISTVIEW, PBM_SETPOS, PBM_SETRANGE32, PBS_SMOOTH,
    PROGRESS_CLASSW, SB_SETPARTS, SB_SETTEXT, SBARS_SIZEGRIP, STATUSCLASSNAMEW, WC_LISTVIEWW,
    CCS_BOTTOM, EM_SETCUEBANNER, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_PROGRESS_CLASS,
    ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, SetWindowTheme,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, AppendMenuW, BringWindowToTop, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW,
    GetParent, GetSystemMetrics, GetWindowTextLengthW, GetWindowTextW,
    KillTimer, LoadCursorW, LoadImageW, MessageBoxW, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassW, SendMessageW, SetForegroundWindow, SetMenuDefaultItem, SetTimer,
    SetWindowPos, SetWindowTextW, ShowWindow, SystemParametersInfoW, TrackPopupMenu,
    TranslateMessage, BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, EN_CHANGE, BN_CLICKED,
    ES_AUTOHSCROLL, ICON_BIG, ICON_SMALL, IDC_ARROW, IDOK, IMAGE_ICON,
    LR_DEFAULTCOLOR, MB_ICONWARNING, MB_OKCANCEL, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG,
    SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
    SW_HIDE, SW_SHOW, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_APP, WM_CHAR, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_NCDESTROY,
    WM_NOTIFY, WM_NULL, WM_PAINT, WM_PRINTCLIENT, WM_SETFOCUS, WM_SETFONT, WM_SETICON,
    WM_SIZE,
    WM_SYSKEYDOWN,
    WM_TIMER, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, HWND_TOPMOST,
};

use crate::index::VolumeIndex;
use crate::mft;
use crate::search::{self, Request, Response, VolumeSet};
use crate::volume::{self, Volume};
use crate::winutil::{
    clipboard_files, clipboard_text, format_count, format_filetime, format_size, open_folder,
    pick_folder, wide,
};
use crate::{elevate, flog, i18n};

const ID_PATTERN: usize = 1001;
const ID_DIR: usize = 1002;
const ID_REINDEX: usize = 1003;
const ID_CLEAR: usize = 1004;
const ID_LIST: usize = 1005;
const ID_PROGRESS: usize = 1006;
const ID_STATUS: usize = 1007;

const IDM_COPY_FILE: u32 = 2001;
const IDM_COPY_NAME: u32 = 2002;
const IDM_COPY_PATH: u32 = 2003;

const TIMER_DEBOUNCE: usize = 1;
const TIMER_POLL: usize = 2;
const TIMER_STARTUP: usize = 3;

const WM_SEARCH_NOW: u32 = WM_APP + 1;
const WM_PICK_DIR: u32 = WM_APP + 2;
const WM_CLEAR_ALL: u32 = WM_APP + 3;
const CLASS_NAME: &str = "FastFindMainWindow";

#[derive(Default)]
pub struct Options {
    pub shot: Option<PathBuf>,
    pub win_shot: Option<PathBuf>,
    pub copy_test: Option<String>,
    pub shot_pattern: String,
    pub shot_dir: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VolStatus {
    Scanning,
    Done,
    Failed,
}

struct VolState {
    drive: char,
    total: u64,
    done: u64,
    status: VolStatus,
    ms: u64,
    err: Option<String>,
    /// files, directories, bytes, skipped records
    stats: Option<(u64, u64, u64, u64)>,
}

enum IndexEvent {
    Start { drive: char, total: u64 },
    Progress { drive: char, done: u64 },
    Done { drive: char, index: Arc<VolumeIndex> },
    Failed { drive: char, msg: String },
    AllDone,
}

#[derive(Default)]
struct Update {
    list_count: Option<usize>,
    relayout: bool,
    /// None hides the progress bar.
    progress: Option<(u32, u32)>,
    status: bool,
    hint: bool,
}

struct App {
    hwnd: HWND,
    dpi: u32,
    font: *mut core::ffi::c_void,

    h_pattern: HWND,
    h_dir: HWND,
    h_reindex: HWND,
    h_clear: HWND,
    h_list: HWND,
    h_progress: HWND,
    h_status: HWND,

    pattern: String,
    dir: String,
    gen: u64,
    req_tx: Sender<Request>,
    res_rx: Receiver<Response>,
    response: Option<Response>,
    pending: bool,

    volumes: VolumeSet,
    vols: Vec<Arc<VolumeIndex>>,
    vol_states: Vec<VolState>,
    index_tx: Sender<IndexEvent>,
    index_rx: Receiver<IndexEvent>,
    indexing: bool,

    hits: Arc<Vec<(char, u32)>>,
    selected: Option<usize>,
    status: String,
    /// Reused buffer for `LVN_GETDISPINFO` (the pointer must stay valid).
    disp_buf: Vec<u16>,

    started: Instant,
    shot: Option<PathBuf>,
    win_shot: Option<PathBuf>,
    copy_test: Option<String>,
    shot_pattern: String,
    shot_dir: String,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static READY: Cell<bool> = const { Cell::new(false) };
    static SUPPRESS_CHANGE: Cell<bool> = const { Cell::new(false) };
    static OPTS: RefCell<Option<Options>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------- entry point

pub fn run(opts: Options) -> i32 {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_PROGRESS_CLASS | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        if !elevate::is_elevated() {
            let body = wide(i18n::admin_body());
            let title = wide(i18n::admin_title());
            let answer = MessageBoxW(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                MB_ICONWARNING | MB_OKCANCEL,
            );
            if answer == IDOK {
                if let Err(e) = elevate::relaunch_elevated() {
                    flog!("提权失败: {e:#}");
                }
            }
            return 0;
        }

        OPTS.with(|c| *c.borrow_mut() = Some(opts));

        let inst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS_NAME);
        let icon = LoadImageW(
            inst,
            1 as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXICON),
            GetSystemMetrics(SM_CYICON),
            LR_DEFAULTCOLOR,
        );
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: inst,
            hIcon: icon,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            flog!("RegisterClassW 失败");
            return 1;
        }

        let mut frame = RECT {
            left: 0,
            top: 0,
            right: 1180,
            bottom: 760,
        };
        AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, 0, 0);
        let w = frame.right - frame.left;
        let h = frame.bottom - frame.top;

        let mut work = RECT::default();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut work as *mut _ as *mut core::ffi::c_void,
            0,
        );
        let x = work.left + ((work.right - work.left) - w) / 2;
        let y = work.top + ((work.bottom - work.top) - h) / 2;

        let title = wide(i18n::app_title());
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            flog!("CreateWindowExW 失败");
            return 1;
        }

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        msg.wParam as i32
    }
}

// ------------------------------------------------------------------ messaging

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        return on_create(hwnd);
    }
    if !READY.with(|r| r.get()) {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    match msg {
        WM_SIZE => {
            layout(hwnd);
            0
        }
        WM_PAINT => paint(hwnd),
        WM_PRINTCLIENT => {
            paint_client(hwnd, wparam as *mut core::ffi::c_void);
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            on_timer(hwnd, wparam);
            0
        }
        WM_COMMAND => {
            on_command(hwnd, wparam, lparam);
            0
        }
        WM_NOTIFY => on_notify(hwnd, lparam),
        WM_GETMINMAXINFO => {
            let dpi = with_app(|a| a.dpi).unwrap_or(96);
            let mmi = &mut *(lparam as *mut MINMAXINFO);
            mmi.ptMinTrackSize.x = scale(dpi, 780);
            mmi.ptMinTrackSize.y = scale(dpi, 440);
            0
        }
        WM_DPICHANGED => {
            on_dpi_changed(hwnd, wparam, lparam);
            0
        }
        WM_SETFOCUS => {
            if let Some(h) = with_app(|a| a.h_pattern) {
                SetFocus(h);
            }
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_main_key(hwnd, wparam as u32);
            0
        }
        WM_SEARCH_NOW => {
            search_now(hwnd);
            0
        }
        WM_PICK_DIR => {
            pick_dir(hwnd);
            0
        }
        WM_CLEAR_ALL => {
            clear_all(hwnd);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            for id in [TIMER_DEBOUNCE, TIMER_POLL, TIMER_STARTUP] {
                KillTimer(hwnd, id);
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let opts = OPTS.with(|c| c.borrow_mut().take()).unwrap_or_default();

    let mut app = App::new(hwnd);
    app.shot = opts.shot;
    app.win_shot = opts.win_shot;
    app.copy_test = opts.copy_test;
    app.shot_pattern = opts.shot_pattern;
    app.shot_dir = opts.shot_dir;
    app.indexing = true;
    app.create_children();

    APP.with(|c| *c.borrow_mut() = Some(app));
    READY.with(|r| r.set(true));

    layout(hwnd);
    set_icon(hwnd);

    let (pat, dir) = with_app(|a| (a.shot_pattern.clone(), a.shot_dir.clone())).unwrap_or_default();
    if !pat.is_empty() {
        if let Some(h) = with_app(|a| a.h_pattern) {
            set_edit_text(h, &pat);
        }
    }
    if !dir.is_empty() {
        if let Some(h) = with_app(|a| a.h_dir) {
            set_edit_text(h, &dir);
        }
    }
    if !(pat.is_empty() && dir.is_empty()) {
        // The programmatic text above suppresses EN_CHANGE, so sync the state
        // and schedule the first search by hand.
        with_app_mut(|a| {
            a.pattern = get_text(a.h_pattern);
            a.dir = get_text(a.h_dir);
        });
        SetTimer(hwnd, TIMER_DEBOUNCE, 130, None);
    }

    if let Some(tx) = with_app(|a| a.index_tx.clone()) {
        start_indexer(tx);
    }
    SetTimer(hwnd, TIMER_POLL, 80, None);
    if with_app(|a| a.shot.is_some() || a.win_shot.is_some() || a.copy_test.is_some())
        .unwrap_or(false)
    {
        SetTimer(hwnd, TIMER_STARTUP, 100, None);
    }

    if let Some(h) = with_app(|a| a.h_pattern) {
        SetFocus(h);
    }
    refresh_status_bar();
    refresh_hint();
    InvalidateRect(hwnd, std::ptr::null(), 0);
    0
}

unsafe fn on_timer(hwnd: HWND, id: WPARAM) {
    match id {
        TIMER_DEBOUNCE => {
            KillTimer(hwnd, TIMER_DEBOUNCE);
            search_now(hwnd);
        }
        TIMER_POLL => {
            let update = with_app_mut(|a| a.drain()).unwrap_or_default();
            apply_update(hwnd, update);
            let busy = with_app(|a| a.indexing || a.pending).unwrap_or(false);
            if !busy {
                KillTimer(hwnd, TIMER_POLL);
            }
        }
        TIMER_STARTUP => startup_tick(hwnd),
        _ => {}
    }
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let id = (wparam & 0xffff) as usize;
    // For WM_COMMAND the notification code lives in the high word of wParam;
    // lParam is the control handle.
    let code = ((wparam >> 16) & 0xffff) as u32;
    let _ = lparam;
    match id {
        ID_PATTERN | ID_DIR if code == EN_CHANGE => edit_changed(hwnd),
        ID_REINDEX if code == BN_CLICKED => reindex(hwnd),
        ID_CLEAR if code == BN_CLICKED => clear_all(hwnd),
        _ => {}
    }
}

unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let nm = lparam as *const NMHDR;
    match (*nm).code {
        LVN_GETDISPINFO => {
            // Arrives synchronously from LVM_SETITEMCOUNT; never while borrowed.
            let di = lparam as *mut NMLVDISPINFOW;
            try_with_app_mut(|a| a.fill_dispinfo(di));
            0
        }
        LVN_ITEMCHANGED => {
            let nv = &*(lparam as *const NMLISTVIEW);
            let was = nv.uOldState & LVIS_SELECTED != 0;
            let is = nv.uNewState & LVIS_SELECTED != 0;
            if was != is {
                let sel = if is && nv.iItem >= 0 {
                    Some(nv.iItem as usize)
                } else {
                    None
                };
                with_app_mut(|a| {
                    a.selected = sel;
                    a.status.clear();
                });
                refresh_status_bar();
            }
            0
        }
        LVN_KEYDOWN => {
            let key = &*(lparam as *const NMLVKEYDOWN);
            on_list_key(hwnd, key.wVKey as u32);
            0
        }
        NM_DBLCLK => {
            let it = &*(lparam as *const NMITEMACTIVATE);
            if it.iItem >= 0 {
                select_item(it.iItem as usize);
                open_selected();
            }
            0
        }
        NM_RCLICK => {
            let it = &*(lparam as *const NMITEMACTIVATE);
            if it.iItem >= 0 {
                select_item(it.iItem as usize);
                show_menu(hwnd);
            }
            0
        }
        _ => 0,
    }
}

unsafe fn on_dpi_changed(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let dpi = (wparam & 0xffff) as u32;
    let rect = &*(lparam as *const RECT);
    SetWindowPos(
        hwnd,
        std::ptr::null_mut(),
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );

    let (old, new) = with_app_mut(|a| {
        a.dpi = dpi.max(48);
        let old = a.font;
        a.font = make_font(a.dpi);
        (old, a.font)
    })
    .unwrap_or((std::ptr::null_mut(), std::ptr::null_mut()));
    if !old.is_null() {
        DeleteObject(old);
    }
    apply_font(new);
    layout(hwnd);
    SetTimer(hwnd, TIMER_POLL, 80, None);
}

unsafe fn on_list_key(hwnd: HWND, key: u32) {
    match key {
        k if k == VK_RETURN as u32 => open_selected(),
        k if k == VK_ESCAPE as u32 => clear_all(hwnd),
        _ => {
            if key == 'C' as u32 && ctrl_down() {
                copy_selected(CopyKind::File);
            }
        }
    }
}

unsafe fn on_main_key(hwnd: HWND, key: u32) {
    match key {
        k if k == VK_ESCAPE as u32 => clear_all(hwnd),
        k if k == VK_RETURN as u32 => open_selected(),
        _ => {
            if key == 'C' as u32 && ctrl_down() {
                copy_selected(CopyKind::File);
            }
        }
    }
}

fn ctrl_down() -> bool {
    unsafe { (GetKeyState(VK_CONTROL as i32) as i16) < 0 }
}

unsafe fn edit_changed(hwnd: HWND) {
    if SUPPRESS_CHANGE.with(|s| s.get()) {
        return;
    }
    let pat = get_text(with_app(|a| a.h_pattern).unwrap_or(std::ptr::null_mut()));
    let dir = get_text(with_app(|a| a.h_dir).unwrap_or(std::ptr::null_mut()));
    let empty = pat.trim().is_empty();
    with_app_mut(|a| {
        a.pattern = pat;
        a.dir = dir;
        if empty {
            a.reset_results();
        }
    });
    if empty {
        set_list_count(0);
        refresh_status_bar();
        refresh_hint();
        InvalidateRect(hwnd, std::ptr::null(), 0);
    } else {
        SetTimer(hwnd, TIMER_DEBOUNCE, 130, None);
        SetTimer(hwnd, TIMER_POLL, 80, None);
    }
}

unsafe fn search_now(hwnd: HWND) {
    let pat = get_text(with_app(|a| a.h_pattern).unwrap_or(std::ptr::null_mut()));
    let dir = get_text(with_app(|a| a.h_dir).unwrap_or(std::ptr::null_mut()));
    let ready = with_app(|a| !a.vols.is_empty()).unwrap_or(false);
    if !ready {
        with_app_mut(|a| a.status = i18n::indexing_wait().to_owned());
        refresh_status_bar();
        return;
    }
    with_app_mut(|a| {
        a.pattern = pat;
        a.dir = dir;
        a.trigger_search();
    });
    SetTimer(hwnd, TIMER_POLL, 80, None);
}

unsafe fn pick_dir(hwnd: HWND) {
    if let Some(path) = pick_folder(i18n::pick_folder_title()) {
        if let Some(h) = with_app(|a| a.h_dir) {
            set_edit_text(h, &path);
        }
        search_now(hwnd);
    }
}

unsafe fn clear_all(hwnd: HWND) {
    if let Some(h) = with_app(|a| a.h_pattern) {
        set_edit_text(h, "");
    }
    if let Some(h) = with_app(|a| a.h_dir) {
        set_edit_text(h, "");
    }
    with_app_mut(|a| {
        a.pattern.clear();
        a.dir.clear();
        a.reset_results();
        a.status.clear();
    });
    set_list_count(0);
    refresh_status_bar();
    refresh_hint();
    InvalidateRect(hwnd, std::ptr::null(), 0);
    if let Some(h) = with_app(|a| a.h_pattern) {
        SetFocus(h);
    }
}

unsafe fn reindex(hwnd: HWND) {
    let tx = with_app_mut(|a| {
        a.reset_results();
        a.vol_states.clear();
        a.vols.clear();
        if let Ok(mut g) = a.volumes.write() {
            g.clear();
        }
        a.indexing = true;
        a.index_tx.clone()
    });
    set_list_count(0);
    refresh_status_bar();
    refresh_hint();
    layout(hwnd);
    if let Some(tx) = tx {
        start_indexer(tx);
    }
    SetTimer(hwnd, TIMER_POLL, 80, None);
}

unsafe fn startup_tick(hwnd: HWND) {
    let elapsed = with_app(|a| a.started.elapsed()).unwrap_or_default();
    let (shot, win_shot, copy_test, ready) = with_app(|a| {
        (
            a.shot.clone(),
            a.win_shot.clone(),
            a.copy_test.clone(),
            !a.indexing && !a.pending,
        )
    })
    .unwrap_or((None, None, None, true));

    if let Some(text) = copy_test {
        if elapsed >= Duration::from_millis(400) {
            let _ = clipboard_text(&text);
            flog!("copy-test: wrote {} chars", text.chars().count());
            std::process::exit(0);
        }
        return;
    }
    if let Some(path) = win_shot {
        if elapsed >= Duration::from_millis(700) {
            with_app_mut(|a| a.win_shot = None);
            spawn_capture(hwnd, path, false);
        }
        return;
    }
    if let Some(path) = shot {
        if (ready && elapsed >= Duration::from_millis(700)) || elapsed >= Duration::from_secs(25) {
            with_app_mut(|a| a.shot = None);
            spawn_capture(hwnd, path, true);
        }
    }
}

/// Grabs the window from a worker thread: capturing from inside a paint cycle
/// makes the compositor hand back half-rendered text.
fn spawn_capture(hwnd: HWND, path: std::path::PathBuf, client_only: bool) {
    let raw = hwnd as usize;
    std::thread::spawn(move || {
        let hwnd = raw as HWND;
        unsafe {
            bring_to_front(hwnd);
        }
        std::thread::sleep(Duration::from_millis(1200));
        if let Err(e) = crate::winutil::capture_window(hwnd, &path, client_only) {
            flog!("截图失败: {e:#}");
        }
        std::process::exit(0);
    });
}

// --------------------------------------------------------------------- state

impl App {
    unsafe fn new(hwnd: HWND) -> Self {
        let volumes: VolumeSet = Arc::new(RwLock::new(Vec::new()));
        let (req_tx, res_rx) = search::spawn_worker(volumes.clone());
        let (index_tx, index_rx) = channel();
        let dpi = GetDpiForWindow(hwnd).max(96);
        Self {
            hwnd,
            dpi,
            font: make_font(dpi),
            h_pattern: std::ptr::null_mut(),
            h_dir: std::ptr::null_mut(),
            h_reindex: std::ptr::null_mut(),
            h_clear: std::ptr::null_mut(),
            h_list: std::ptr::null_mut(),
            h_progress: std::ptr::null_mut(),
            h_status: std::ptr::null_mut(),
            pattern: String::new(),
            dir: String::new(),
            gen: 0,
            req_tx,
            res_rx,
            response: None,
            pending: false,
            volumes,
            vols: Vec::new(),
            vol_states: Vec::new(),
            index_tx,
            index_rx,
            indexing: false,
            hits: Arc::new(Vec::new()),
            selected: None,
            status: String::new(),
            disp_buf: Vec::new(),
            started: Instant::now(),
            shot: None,
            win_shot: None,
            copy_test: None,
            shot_pattern: String::new(),
            shot_dir: String::new(),
        }
    }

    unsafe fn create_children(&mut self) {
        let inst = GetModuleHandleW(std::ptr::null());
        let dpi = self.dpi;
        let edit = wide("EDIT");
        let button = wide("BUTTON");

        let child = |class: *const u16,
                     text: &str,
                     style: u32,
                     ex: u32,
                     id: usize|
         -> HWND {
            let t = wide(text);
            unsafe {
                CreateWindowExW(
                    ex,
                    class,
                    t.as_ptr(),
                    style,
                    0,
                    0,
                    10,
                    10,
                    self.hwnd,
                    id as usize as *mut core::ffi::c_void,
                    inst,
                    std::ptr::null(),
                )
            }
        };

        self.h_pattern = child(
            edit.as_ptr(),
            "",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
            ID_PATTERN,
        );
        self.h_dir = child(
            edit.as_ptr(),
            "",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
            ID_DIR,
        );
        self.h_reindex = child(
            button.as_ptr(),
            i18n::tr("重新索引", "Reindex"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            ID_REINDEX,
        );
        self.h_clear = child(
            button.as_ptr(),
            i18n::tr("清空", "Clear"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            ID_CLEAR,
        );
        self.h_list = child(
            WC_LISTVIEWW,
            "",
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | LVS_REPORT
                | LVS_OWNERDATA
                | LVS_SINGLESEL
                | LVS_SHOWSELALWAYS,
            WS_EX_CLIENTEDGE,
            ID_LIST,
        );
        self.h_progress = child(
            PROGRESS_CLASSW,
            "",
            WS_CHILD | PBS_SMOOTH,
            0,
            ID_PROGRESS,
        );
        self.h_status = child(
            STATUSCLASSNAMEW,
            "",
            WS_CHILD | WS_VISIBLE | SBARS_SIZEGRIP | CCS_BOTTOM as u32,
            0,
            ID_STATUS,
        );
        // Modern Explorer look for the list view.
        let theme = wide("Explorer");
        let _ = SetWindowTheme(self.h_list, theme.as_ptr(), std::ptr::null());
        let ex_style = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        SendMessageW(
            self.h_list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            ex_style as usize,
            ex_style as isize,
        );

        let columns: [(&str, i32, i32); 4] = [
            (i18n::tr("名称", "Name"), scale(dpi, 340), LVCFMT_LEFT),
            (i18n::tr("所在目录", "Folder"), scale(dpi, 420), LVCFMT_LEFT),
            (i18n::tr("大小", "Size"), scale(dpi, 100), LVCFMT_RIGHT),
            (i18n::tr("修改时间", "Modified"), scale(dpi, 150), LVCFMT_LEFT),
        ];
        for (i, (name, width, align)) in columns.iter().enumerate() {
            let mut buf: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let mut col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                fmt: *align,
                cx: *width,
                pszText: buf.as_mut_ptr(),
                cchTextMax: buf.len() as i32,
                iSubItem: i as i32,
                ..Default::default()
            };
            SendMessageW(
                self.h_list,
                LVM_INSERTCOLUMNW,
                i,
                &mut col as *mut _ as isize,
            );
        }

        let cue = wide(i18n::pattern_cue());
        SendMessageW(self.h_pattern, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
        let cue = wide(i18n::dir_cue());
        SendMessageW(self.h_dir, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);

        SetWindowSubclass(self.h_pattern, Some(edit_subclass), 1, 0);
        SetWindowSubclass(self.h_dir, Some(edit_subclass), 2, 1);

        self.apply_font();
    }

    fn apply_font(&self) {
        let font = self.font;
        if font.is_null() {
            return;
        }
        for h in [
            self.h_pattern,
            self.h_dir,
            self.h_reindex,
            self.h_clear,
            self.h_list,
            self.h_status,
        ] {
            if !h.is_null() {
                unsafe { SendMessageW(h, WM_SETFONT, font as usize, 1) };
            }
        }
    }

    fn snapshot_volumes(&self) -> Vec<Arc<VolumeIndex>> {
        self.volumes.read().map(|g| g.clone()).unwrap_or_default()
    }

    fn total_stats(&self) -> (u64, u64, u64) {
        let vols = self.snapshot_volumes();
        let (mut files, mut dirs, mut bytes) = (0, 0, 0);
        for v in &vols {
            files += v.file_count;
            dirs += v.dir_count;
            bytes += v.total_bytes;
        }
        (files, dirs, bytes)
    }

    fn trigger_search(&mut self) {
        self.gen += 1;
        self.pending = true;
        let _ = self.req_tx.send(Request {
            gen: self.gen,
            pattern: self.pattern.clone(),
            dir: self.dir.clone(),
        });
    }

    fn reset_results(&mut self) {
        self.gen += 1;
        self.response = None;
        self.hits = Arc::new(Vec::new());
        self.pending = false;
        self.selected = None;
    }

    fn entry(&self, hit: (char, u32)) -> Option<(&VolumeIndex, &crate::index::Entry)> {
        entry_of(&self.vols, hit)
    }

    fn selected_info(&self) -> Option<(String, String, String)> {
        let sel = self.selected?;
        let hit = *self.hits.get(sel)?;
        let (v, e) = self.entry(hit)?;
        Some((v.full_path(e), v.name(e).to_owned(), v.prefix(e).to_owned()))
    }

    fn total_text(&self) -> String {
        let (files, dirs, bytes) = self.total_stats();
        i18n::total_line(&format_count(files), &format_count(dirs), &format_size(bytes))
    }

    fn status_text(&self) -> String {
        if !self.status.is_empty() {
            self.status.clone()
        } else if let Some((path, _, _)) = self.selected_info() {
            path
        } else {
            i18n::row_help().to_owned()
        }
    }

    fn fill_dispinfo(&mut self, di: *mut NMLVDISPINFOW) {
        unsafe {
            let item = &mut (*di).item;
            if item.mask & LVIF_TEXT == 0 {
                return;
            }
            let idx = item.iItem.max(0) as usize;
            let col = item.iSubItem.max(0) as usize;
            let Some(hit) = self.hits.get(idx).copied() else {
                return;
            };
            let text = match self.entry(hit) {
                Some((v, e)) => match col {
                    0 => v.name(e).to_owned(),
                    1 => v.prefix(e).to_owned(),
                    2 => {
                        if e.is_dir {
                            String::new()
                        } else {
                            format_size(e.size)
                        }
                    }
                    3 => format_filetime(e.mtime),
                    _ => String::new(),
                },
                None => String::new(),
            };
            self.disp_buf.clear();
            self.disp_buf.extend(text.encode_utf16());
            self.disp_buf.push(0);
            item.pszText = self.disp_buf.as_mut_ptr();
        }
    }

    fn drain(&mut self) -> Update {
        let mut update = Update::default();
        let mut vol_count_changed = false;

        while let Ok(ev) = self.index_rx.try_recv() {
            match ev {
                IndexEvent::Start { drive, total } => {
                    if !self.vol_states.iter().any(|v| v.drive == drive) {
                        vol_count_changed = true;
                    }
                    self.vol_states.retain(|v| v.drive != drive);
                    self.vol_states.push(VolState {
                        drive,
                        total,
                        done: 0,
                        status: VolStatus::Scanning,
                        ms: 0,
                        err: None,
                        stats: None,
                    });
                    self.vol_states.sort_by_key(|v| v.drive);
                }
                IndexEvent::Progress { drive, done } => {
                    if let Some(v) = self.vol_states.iter_mut().find(|v| v.drive == drive) {
                        v.done = done;
                    }
                }
                IndexEvent::Done { drive, index } => {
                    let stats = (
                        index.file_count,
                        index.dir_count,
                        index.total_bytes,
                        index.skipped,
                    );
                    let ms = index.scanned_ms;
                    if let Some(v) = self.vol_states.iter_mut().find(|v| v.drive == drive) {
                        v.status = VolStatus::Done;
                        v.done = v.total;
                        v.ms = ms;
                        v.stats = Some(stats);
                    }
                    if let Ok(mut guard) = self.volumes.write() {
                        guard.retain(|v| v.drive != drive);
                        guard.push(index);
                        guard.sort_by_key(|v| v.drive);
                    }
                    self.vols = self.snapshot_volumes();
                    self.trigger_search();
                    update.status = true;
                }
                IndexEvent::Failed { drive, msg } => {
                    flog!("volume {} index failed: {}", drive, msg);
                    if let Some(v) = self.vol_states.iter_mut().find(|v| v.drive == drive) {
                        v.status = VolStatus::Failed;
                        v.err = Some(msg);
                    } else {
                        self.vol_states.push(VolState {
                            drive,
                            total: 0,
                            done: 0,
                            status: VolStatus::Failed,
                            ms: 0,
                            err: Some(msg),
                            stats: None,
                        });
                        self.vol_states.sort_by_key(|v| v.drive);
                        vol_count_changed = true;
                    }
                    update.status = true;
                }
                IndexEvent::AllDone => {
                    self.indexing = false;
                    self.trigger_search();
                }
            }
        }

        while let Ok(resp) = self.res_rx.try_recv() {
            if resp.gen == self.gen {
                self.status = resp.error.clone().unwrap_or_default();
                self.hits = resp.hits.clone();
                self.vols = self.snapshot_volumes();
                self.selected = None;
                self.response = Some(resp);
                self.pending = false;
                update.list_count = Some(self.hits.len());
                update.status = true;
                update.hint = true;
            }
        }

        if vol_count_changed {
            update.relayout = true;
        }

        update.progress = self
            .vol_states
            .iter()
            .find(|v| v.status == VolStatus::Scanning)
            .map(|v| {
                let pos = if v.total > 0 {
                    ((v.done as f64 / v.total as f64) * 1000.0).clamp(0.0, 1000.0) as u32
                } else {
                    0
                };
                (pos, 1000)
            });

        update
    }
}

// -------------------------------------------------------------------- drawing

#[derive(Clone, Copy)]
struct Metrics {
    pad: i32,
    label_w: i32,
    row_h: i32,
    y1: i32,
    y2: i32,
    btn_y: i32,
    btn_h: i32,
    info_y: i32,
    line_h: i32,
    list_y: i32,
    list_h: i32,
}

fn scale(dpi: u32, value: i32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn metrics(dpi: u32, nvol: usize, client_h: i32) -> Metrics {
    let pad = scale(dpi, 12);
    let row_h = scale(dpi, 24);
    let label_w = scale(dpi, 52);
    let y1 = scale(dpi, 10);
    let y2 = y1 + row_h + scale(dpi, 8);
    let btn_y = y2 + row_h + scale(dpi, 8);
    let btn_h = scale(dpi, 26);
    let info_y = btn_y + btn_h + scale(dpi, 10);
    let line_h = scale(dpi, 20);
    let status_h = scale(dpi, 24);
    let list_y = info_y + line_h * nvol as i32 + scale(dpi, 8);
    let list_h = (client_h - status_h - list_y).max(scale(dpi, 80));
    Metrics {
        pad,
        label_w,
        row_h,
        y1,
        y2,
        btn_y,
        btn_h,
        info_y,
        line_h,
        list_y,
        list_h,
    }
}

unsafe fn layout(hwnd: HWND) {
    let (dpi, nvol) = with_app(|a| (a.dpi, a.vol_states.len())).unwrap_or((96, 0));
    let Some((h_pat, h_dir, h_re, h_cl, h_list, h_status)) = with_app(|a| {
        (
            a.h_pattern,
            a.h_dir,
            a.h_reindex,
            a.h_clear,
            a.h_list,
            a.h_status,
        )
    }) else {
        return;
    };

    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc);
    let width = rc.right.max(1);
    let m = metrics(dpi, nvol, rc.bottom.max(1));

    let edit_x = m.pad + m.label_w;
    let edit_w = (width - 2 * m.pad - m.label_w).max(scale(dpi, 120));
    MoveWindow(h_pat, edit_x, m.y1, edit_w, m.row_h, 1);
    MoveWindow(h_dir, edit_x, m.y2, edit_w, m.row_h, 1);
    MoveWindow(h_re, m.pad, m.btn_y, scale(dpi, 100), m.btn_h, 1);
    MoveWindow(h_cl, m.pad + scale(dpi, 108), m.btn_y, scale(dpi, 70), m.btn_h, 1);

    let list_w = (width - 2 * m.pad).max(scale(dpi, 200));
    MoveWindow(h_list, m.pad, m.list_y, list_w, m.list_h, 1);

    // Column widths: name fixed, folder takes the rest.
    let c0 = scale(dpi, 340).min((list_w / 2).max(scale(dpi, 120)));
    let c2 = scale(dpi, 100);
    let c3 = scale(dpi, 150);
    let c1 = (list_w - c0 - c2 - c3 - scale(dpi, 24)).max(scale(dpi, 120));
    for (i, w) in [(0, c0), (1, c1), (2, c2), (3, c3)] {
        SendMessageW(h_list, LVM_SETCOLUMNWIDTH, i, w as isize);
    }

    SendMessageW(h_status, WM_SIZE, 0, 0);
    let parts = [scale(dpi, 380), -1];
    SendMessageW(h_status, SB_SETPARTS, 2, parts.as_ptr() as isize);

    place_progress(hwnd);
    InvalidateRect(hwnd, std::ptr::null(), 0);
}

unsafe fn place_progress(hwnd: HWND) {
    let Some((dpi, idx, h_prog)) = with_app(|a| {
        (
            a.dpi,
            a.vol_states.iter().position(|v| v.status == VolStatus::Scanning),
            a.h_progress,
        )
    }) else {
        return;
    };
    let nvol = with_app(|a| a.vol_states.len()).unwrap_or(0);
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc);
    let m = metrics(dpi, nvol, rc.bottom.max(1));
    match idx {
        Some(i) => {
            let bar_h = scale(dpi, 16);
            let y = m.info_y + i as i32 * m.line_h + (m.line_h - bar_h) / 2;
            MoveWindow(h_prog, m.pad + scale(dpi, 150), y, scale(dpi, 260), bar_h, 1);
            ShowWindow(h_prog, SW_SHOWNA);
        }
        None => {
            ShowWindow(h_prog, SW_HIDE);
        }
    }
}

unsafe fn paint(hwnd: HWND) -> LRESULT {
    let mut ps = windows_sys::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    paint_client(hwnd, hdc);
    EndPaint(hwnd, &ps);
    0
}

/// Draws the whole client area into `hdc`, used for both WM_PAINT and WM_PRINTCLIENT.
unsafe fn paint_client(hwnd: HWND, hdc: *mut core::ffi::c_void) {
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc);
    let w = rc.right.max(1);
    let h = rc.bottom.max(1);

    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp);
    FillRect(mem, &rc, GetSysColorBrush(COLOR_BTNFACE));
    SetBkMode(mem, TRANSPARENT as i32);

    let data = with_app(|a| {
        let m = metrics(a.dpi, a.vol_states.len(), h);
        let active = !a.pattern.trim().is_empty();
        let hits = if active {
            a.response
                .as_ref()
                .map(|r| {
                    if r.hits.is_empty() && r.error.is_none() {
                        i18n::no_matches().to_owned()
                    } else {
                        i18n::hits_label(r.hits.len(), r.truncated, r.ms)
                    }
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        let hint = if active {
            String::new()
        } else {
            i18n::empty_hint().to_owned()
        };
        let scope = if active {
            a.response
                .as_ref()
                .map(|r| i18n::scope_label(&r.dir_label))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let lines: Vec<(String, bool)> = a
            .vol_states
            .iter()
            .map(|v| match v.status {
                VolStatus::Scanning => (i18n::indexing_label(v.drive), false),
                VolStatus::Done => {
                    let (f, d, b, skipped) = v.stats.unwrap_or_default();
                    let skipped = (skipped > 0).then(|| format_count(skipped));
                    (
                        i18n::volume_line(
                            v.drive,
                            &format_count(f),
                            &format_count(d),
                            &format_size(b),
                            v.ms,
                            skipped.as_deref(),
                        ),
                        false,
                    )
                }
                VolStatus::Failed => (
                    format!("{}: {}", v.drive, v.err.clone().unwrap_or_default()),
                    true,
                ),
            })
            .collect();
        let scan = a
            .vol_states
            .iter()
            .find(|v| v.status == VolStatus::Scanning)
            .map(|v| (v.done, v.total));
        (a.font, a.dpi, m, hits, hint, scope, lines, scan)
    })
    .unwrap_or((
        std::ptr::null_mut(),
        96,
        metrics(96, 0, h),
        String::new(),
        String::new(),
        String::new(),
        Vec::new(),
        None,
    ));

    let (font, dpi, m, hits, hint, scope, lines, scan) = data;
    if !font.is_null() {
        SelectObject(mem, font);
    }
    SetTextColor(mem, GetSysColor(COLOR_BTNTEXT));

    draw_text(
        mem,
        i18n::tr("文件名", "Name"),
        m.pad,
        m.y1,
        m.label_w,
        m.row_h,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    draw_text(
        mem,
        i18n::tr("目录", "Folder"),
        m.pad,
        m.y2,
        m.label_w,
        m.row_h,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );

    if !hits.is_empty() {
        let x = m.pad + scale(dpi, 190);
        draw_text(
            mem,
            &hits,
            x,
            m.btn_y,
            w - x - m.pad,
            m.btn_h,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        if !scope.is_empty() {
            let hx = x + text_width(mem, &hits) + scale(dpi, 18);
            draw_text(
                mem,
                &scope,
                hx,
                m.btn_y,
                (w - hx - m.pad).max(0),
                m.btn_h,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
    }

    if hits.is_empty() && !hint.is_empty() {
        let x = m.pad + scale(dpi, 190);
        SetTextColor(mem, rgb(120, 120, 120));
        draw_text(
            mem,
            &hint,
            x,
            m.btn_y,
            (w - x - m.pad).max(0),
            m.btn_h,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        SetTextColor(mem, GetSysColor(COLOR_BTNTEXT));
    }

    for (i, (text, failed)) in lines.iter().enumerate() {
        SetTextColor(
            mem,
            if *failed {
                rgb(200, 60, 60)
            } else {
                GetSysColor(COLOR_BTNTEXT)
            },
        );
        draw_text(
            mem,
            text,
            m.pad,
            m.info_y + i as i32 * m.line_h,
            (w - 2 * m.pad).max(0),
            m.line_h,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    if let Some((done, total)) = scan {
        SetTextColor(mem, GetSysColor(COLOR_BTNTEXT));
        let text = format!("{} / {}", format_size(done), format_size(total));
        draw_text(
            mem,
            &text,
            m.pad + scale(dpi, 420),
            m.info_y,
            (w - m.pad - scale(dpi, 420)).max(0),
            m.line_h,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
}

unsafe fn draw_text(
    hdc: *mut core::ffi::c_void,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: u32,
) {
    let buf: Vec<u16> = text.encode_utf16().collect();
    let mut rc = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    DrawTextW(hdc, buf.as_ptr(), buf.len() as i32, &mut rc, flags);
}

unsafe fn text_width(hdc: *mut core::ffi::c_void, text: &str) -> i32 {
    let buf: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    GetTextExtentPoint32W(hdc, buf.as_ptr(), buf.len() as i32, &mut size);
    size.cx
}

// --------------------------------------------------------------------- actions

unsafe fn apply_update(hwnd: HWND, update: Update) {
    if let Some(count) = update.list_count {
        set_list_count(count);
    }
    if update.relayout {
        layout(hwnd);
    }
    match update.progress {
        Some((pos, max)) => {
            if let Some(h) = with_app(|a| a.h_progress) {
                SendMessageW(h, PBM_SETRANGE32, 0, max as isize);
                SendMessageW(h, PBM_SETPOS, pos as usize, 0);
            }
            place_progress(hwnd);
        }
        None => {
            if let Some(h) = with_app(|a| a.h_progress) {
                ShowWindow(h, SW_HIDE);
            }
        }
    }
    if update.status {
        refresh_status_bar();
    }
    if update.hint || update.list_count.is_some() || update.relayout {
        refresh_hint();
    }
    if update.status || update.hint || update.list_count.is_some() || update.relayout {
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

unsafe fn set_list_count(count: usize) {
    let Some(h) = with_app(|a| a.h_list) else {
        return;
    };
    SendMessageW(
        h,
        LVM_SETITEMCOUNT,
        count,
        (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as isize,
    );
    InvalidateRect(h, std::ptr::null(), 0);
    clear_list_selection();
}

unsafe fn clear_list_selection() {
    let Some(h) = with_app(|a| a.h_list) else {
        return;
    };
    let mut item = LVITEMW {
        state: 0,
        stateMask: LVIS_SELECTED | LVIS_FOCUSED,
        ..Default::default()
    };
    SendMessageW(
        h,
        LVM_SETITEMSTATE,
        usize::MAX,
        &mut item as *mut _ as isize,
    );
}

unsafe fn select_item(index: usize) {
    let Some(h) = with_app(|a| a.h_list) else {
        return;
    };
    let mut item = LVITEMW {
        state: LVIS_SELECTED | LVIS_FOCUSED,
        stateMask: LVIS_SELECTED | LVIS_FOCUSED,
        ..Default::default()
    };
    SendMessageW(h, LVM_SETITEMSTATE, index, &mut item as *mut _ as isize);
    SetFocus(h);
}

unsafe fn refresh_status_bar() {
    let Some((h, left, right)) =
        with_app(|a| (a.h_status, a.total_text(), a.status_text()))
    else {
        return;
    };
    let l = wide(&left);
    SendMessageW(h, SB_SETTEXT, 0, l.as_ptr() as isize);
    let r = wide(&right);
    SendMessageW(h, SB_SETTEXT, 1, r.as_ptr() as isize);
}

unsafe fn refresh_hint() {
    // The hint is drawn by `paint`; just make sure that row gets refreshed.
    if let Some(h) = with_app(|a| a.hwnd) {
        InvalidateRect(h, std::ptr::null(), 0);
    }
}

#[derive(Clone, Copy)]
enum CopyKind {
    File,
    Name,
    Path,
}

unsafe fn copy_selected(kind: CopyKind) {
    let Some((path, name, _folder)) = with_app(|a| a.selected_info()).flatten() else {
        return;
    };
    let (ok_text, result) = match kind {
        CopyKind::File => (
            i18n::copy_status(&name),
            clipboard_files(std::slice::from_ref(&path), false),
        ),
        CopyKind::Name => (
            i18n::copied_name_status().to_owned(),
            clipboard_text(&name),
        ),
        CopyKind::Path => (
            i18n::copied_path_status().to_owned(),
            clipboard_text(&path),
        ),
    };
    let text = match result {
        Ok(()) => ok_text,
        Err(e) => format!("{e}"),
    };
    with_app_mut(|a| a.status = text);
    refresh_status_bar();
}

unsafe fn open_selected() {
    if let Some((_path, _name, folder)) = with_app(|a| a.selected_info()).flatten() {
        open_folder(&folder);
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let file = wide(i18n::tr("复制文件", "Copy file"));
    let name = wide(i18n::tr("复制文件名", "Copy file name"));
    let path = wide(i18n::tr("复制路径", "Copy path"));

    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    AppendMenuW(menu, MF_STRING, IDM_COPY_FILE as usize, file.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, IDM_COPY_NAME as usize, name.as_ptr());
    AppendMenuW(menu, MF_STRING, IDM_COPY_PATH as usize, path.as_ptr());
    SetMenuDefaultItem(menu, IDM_COPY_FILE, 0);

    let mut pt = POINT::default();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    DestroyMenu(menu);
    PostMessageW(hwnd, WM_NULL, 0, 0);

    match cmd as u32 {
        IDM_COPY_FILE => copy_selected(CopyKind::File),
        IDM_COPY_NAME => copy_selected(CopyKind::Name),
        IDM_COPY_PATH => copy_selected(CopyKind::Path),
        _ => {}
    }
}

// ----------------------------------------------------------------------- misc

unsafe fn set_icon(hwnd: HWND) {
    let inst = GetModuleHandleW(std::ptr::null());
    for (which, cx, cy) in [
        (ICON_BIG, SM_CXICON, SM_CYICON),
        (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
    ] {
        let icon = LoadImageW(
            inst,
            1 as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(cx),
            GetSystemMetrics(cy),
            LR_DEFAULTCOLOR,
        );
        if !icon.is_null() {
            SendMessageW(hwnd, WM_SETICON, which as usize, icon as isize);
        }
    }
}

unsafe fn make_font(dpi: u32) -> *mut core::ffi::c_void {
    let mut ncm: windows_sys::Win32::UI::WindowsAndMessaging::NONCLIENTMETRICSW =
        std::mem::zeroed();
    ncm.cbSize = std::mem::size_of_val(&ncm) as u32;
    SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        &mut ncm as *mut _ as *mut core::ffi::c_void,
        0,
    );
    let mut lf: LOGFONTW = ncm.lfMessageFont;
    lf.lfHeight = -((9 * dpi as i32 + 36) / 72);
    lf.lfWeight = FW_NORMAL as i32;
    CreateFontIndirectW(&lf)
}

unsafe fn apply_font(font: *mut core::ffi::c_void) {
    if font.is_null() {
        return;
    }
    if let Some(list) = with_app(|a| {
        [
            a.h_pattern,
            a.h_dir,
            a.h_reindex,
            a.h_clear,
            a.h_list,
            a.h_status,
        ]
    }) {
        for h in list {
            if !h.is_null() {
                SendMessageW(h, WM_SETFONT, font as usize, 1);
            }
        }
    }
}

fn get_text(h: HWND) -> String {
    if h.is_null() {
        return String::new();
    }
    unsafe {
        let len = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; len.max(0) as usize + 1];
        let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

unsafe fn set_edit_text(h: HWND, text: &str) {
    let buf = wide(text);
    SUPPRESS_CHANGE.with(|s| s.set(true));
    SetWindowTextW(h, buf.as_ptr());
    SUPPRESS_CHANGE.with(|s| s.set(false));
}

unsafe extern "system" fn edit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    is_dir: usize,
) -> LRESULT {
    match msg {
        WM_KEYDOWN if wparam == VK_RETURN as usize => {
            PostMessageW(GetParent(hwnd), WM_SEARCH_NOW, 0, 0);
            return 0;
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            PostMessageW(GetParent(hwnd), WM_CLEAR_ALL, 0, 0);
            return 0;
        }
        WM_CHAR if wparam == VK_RETURN as usize => return 0,
        WM_CHAR if wparam == VK_ESCAPE as usize => return 0,
        WM_LBUTTONDBLCLK if is_dir != 0 => {
            PostMessageW(GetParent(hwnd), WM_PICK_DIR, 0, 0);
            return 0;
        }
        WM_NCDESTROY => {
            RemoveWindowSubclass(hwnd, Some(edit_subclass), _id);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

// ---------------------------------------------------------------- app helpers

/// Lifts the window above other windows so the debug screenshots see it.
unsafe fn bring_to_front(hwnd: HWND) {
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    SetForegroundWindow(hwnd);
    BringWindowToTop(hwnd);
    RedrawWindow(
        hwnd,
        std::ptr::null(),
        std::ptr::null_mut(),
        RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW | RDW_ALLCHILDREN,
    );
    UpdateWindow(hwnd);
}

fn with_app<R>(f: impl FnOnce(&App) -> R) -> Option<R> {
    APP.with(|c| c.borrow().as_ref().map(f))
}

fn with_app_mut<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.borrow_mut().as_mut().map(f))
}

fn try_with_app_mut<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

fn entry_of<'a>(
    vols: &'a [Arc<VolumeIndex>],
    hit: (char, u32),
) -> Option<(&'a VolumeIndex, &'a crate::index::Entry)> {
    let v = vols.iter().find(|v| v.drive == hit.0)?;
    let e = v.entries.get(hit.1 as usize)?;
    Some((v, e))
}

// ------------------------------------------------------------------- indexing

fn start_indexer(tx: Sender<IndexEvent>) {
    std::thread::Builder::new()
        .name("fastfind-index".to_owned())
        .spawn(move || {
            let drives = volume::fixed_drives();
            if drives.is_empty() {
                let _ = tx.send(IndexEvent::Failed {
                    drive: '?',
                    msg: i18n::no_fixed_drives().to_owned(),
                });
            }
            for d in drives {
                if !d.fs.eq_ignore_ascii_case("NTFS") {
                    let _ = tx.send(IndexEvent::Failed {
                        drive: d.letter,
                        msg: i18n::fs_unsupported(&d.fs),
                    });
                    continue;
                }
                let vol = match Volume::open(d.letter) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(IndexEvent::Failed {
                            drive: d.letter,
                            msg: format!("{e:#}"),
                        });
                        continue;
                    }
                };
                let total = vol.info.mft_valid_bytes;
                let _ = tx.send(IndexEvent::Start {
                    drive: d.letter,
                    total,
                });
                let started = Instant::now();
                let last = Cell::new(Instant::now());
                let result = mft::scan(&vol, &|done| {
                    if last.get().elapsed() >= Duration::from_millis(120) {
                        last.set(Instant::now());
                        let _ = tx.send(IndexEvent::Progress {
                            drive: d.letter,
                            done,
                        });
                    }
                });
                match result {
                    Ok(scan) => {
                        let ms = started.elapsed().as_millis() as u64;
                        let index = Arc::new(VolumeIndex::build(d.letter, scan, ms));
                        flog!(
                            "volume {} indexed: {} files, {} dirs, {} ms",
                            d.letter,
                            index.file_count,
                            index.dir_count,
                            ms
                        );
                        let _ = tx.send(IndexEvent::Done {
                            drive: d.letter,
                            index,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(IndexEvent::Failed {
                            drive: d.letter,
                            msg: format!("{e:#}"),
                        });
                    }
                }
            }
            let _ = tx.send(IndexEvent::AllDone);
        })
        .expect("spawn indexer");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_fit_small_windows() {
        let m = metrics(96, 4, 760);
        assert!(m.list_y > m.info_y);
        assert!(m.list_h > 0);
        // A tiny window must still leave a usable list area.
        let m = metrics(96, 4, 300);
        assert_eq!(m.list_h, 80);
    }

    #[test]
    fn scaling_rounds() {
        assert_eq!(scale(96, 24), 24);
        assert_eq!(scale(144, 24), 36);
        assert_eq!(scale(120, 10), 13);
    }
}
