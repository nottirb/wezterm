use crate::domain::DomainId;
use crate::pane::{CloseReason, Pane, PaneId, Pattern, SearchResult};
use crate::renderable::*;
use crate::tmux::{TmuxDomain, TmuxDomainState};
use crate::{Domain, Mux, MuxNotification};
use anyhow::Error;
use async_trait::async_trait;
use config::keyassignment::ScrollbackEraseMode;
use config::{configuration, ExitBehavior};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use rangeset::RangeSet;
use serde::{Deserialize, Serialize};
use smol::channel::{bounded, Receiver, TryRecvError};
use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::io::Result as IoResult;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::escape::DeviceControlMode;
use termwiz::surface::{Line, SequenceNo, SEQ_ZERO};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, AlertHandler, CellAttributes, Clipboard, KeyCode, KeyModifiers, MouseEvent,
    SemanticZone, StableRowIndex, Terminal, TerminalConfiguration,
};

#[derive(Debug)]
enum ProcessState {
    Running {
        child_waiter: Receiver<IoResult<ExitStatus>>,
        pid: Option<u32>,
        signaller: Box<dyn ChildKiller>,
        // Whether we've explicitly killed the child
        killed: bool,
    },
    DeadPendingClose {
        killed: bool,
    },
    Dead,
}

pub struct LocalPane {
    pane_id: PaneId,
    terminal: RefCell<Terminal>,
    process: RefCell<ProcessState>,
    pty: RefCell<Box<dyn MasterPty>>,
    domain_id: DomainId,
    tmux_domain: RefCell<Option<Arc<TmuxDomainState>>>,
}

#[async_trait(?Send)]
impl Pane for LocalPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let mut cursor = terminal_get_cursor_position(&mut self.terminal.borrow_mut());
        if self.tmux_domain.borrow().is_some() {
            cursor.visibility = termwiz::surface::CursorVisibility::Hidden;
        }
        cursor
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.terminal.borrow().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.terminal.borrow_mut(), lines, seqno)
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let (first, mut lines) = terminal_get_lines(&mut self.terminal.borrow_mut(), lines);

        if self.tmux_domain.borrow().is_some() {
            let cursor = terminal_get_cursor_position(&mut self.terminal.borrow_mut());
            let idx = cursor.y as isize - first as isize;
            if idx > 0 {
                if let Some(line) = lines.get_mut(idx as usize) {
                    line.overlay_text_with_attribute(
                        0,
                        "This pane is running tmux control mode. Press q to detach.",
                        CellAttributes::default(),
                        SEQ_ZERO,
                    );
                }
            }
        }

        (first, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.terminal.borrow_mut())
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.terminal.borrow().user_vars().clone()
    }

    fn kill(&self) {
        let mut proc = self.process.borrow_mut();
        log::debug!(
            "killing process in pane {}, state is {:?}",
            self.pane_id,
            proc
        );
        match &mut *proc {
            ProcessState::Running {
                signaller, killed, ..
            } => {
                let _ = signaller.kill();
                *killed = true;
            }
            ProcessState::DeadPendingClose { killed } => {
                *killed = true;
            }
            _ => {}
        }
    }

    fn is_dead(&self) -> bool {
        let mut proc = self.process.borrow_mut();
        let mut notify = None;

        const EXIT_BEHAVIOR: &str = "\x1b]8;;https://wezfurlong.org/wezterm/\
                                     config/lua/config/exit_behavior.html\
                                     \x1b\\exit_behavior\x1b]8;;\x1b\\";

        match &mut *proc {
            ProcessState::Running {
                child_waiter,
                killed,
                ..
            } => {
                let status = match child_waiter.try_recv() {
                    Ok(Ok(s)) => Some(s),
                    Err(TryRecvError::Empty) => None,
                    _ => Some(ExitStatus::with_exit_code(1)),
                };
                if let Some(status) = status {
                    match (configuration().exit_behavior, status.success(), killed) {
                        (ExitBehavior::Close, _, _) => *proc = ProcessState::Dead,
                        (ExitBehavior::CloseOnCleanExit, false, false) => {
                            notify = Some(format!(
                                "\r\n[Process didn't exit cleanly. ({}=\"CloseOnCleanExit\")]\r\n",
                                EXIT_BEHAVIOR
                            ));
                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::CloseOnCleanExit, ..) => *proc = ProcessState::Dead,
                        (ExitBehavior::Hold, success, false) => {
                            if success {
                                notify = Some(format!(
                                    "\r\n[Process completed. ({}=\"Hold\")]\r\n",
                                    EXIT_BEHAVIOR
                                ));
                            } else {
                                notify = Some(format!(
                                    "\r\n[Process didn't exit cleanly. ({}=\"Hold\")]\r\n",
                                    EXIT_BEHAVIOR
                                ));
                            }
                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::Hold, _, true) => *proc = ProcessState::Dead,
                    }
                    log::debug!("child terminated, new state is {:?}", proc);
                }
            }
            ProcessState::DeadPendingClose { killed } => {
                if *killed {
                    *proc = ProcessState::Dead;
                    log::debug!("child state -> {:?}", proc);
                }
            }
            ProcessState::Dead => {}
        }

        if let Some(notify) = notify {
            let pane_id = self.pane_id;
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get().unwrap();
                if let Some(pane) = mux.get_pane(pane_id) {
                    let mut parser = termwiz::escape::parser::Parser::new();
                    let mut actions = vec![];
                    parser.parse(notify.as_bytes(), |action| actions.push(action));
                    pane.perform_actions(actions);
                    mux.notify(MuxNotification::PaneOutput(pane_id));
                }
            })
            .detach();
        }

        match &*proc {
            ProcessState::Running { .. } => false,
            ProcessState::DeadPendingClose { .. } => false,
            ProcessState::Dead => true,
        }
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.terminal.borrow_mut().set_clipboard(clipboard);
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.terminal.borrow_mut().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.borrow().get_config())
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.terminal.borrow_mut().perform_actions(actions)
    }

    fn mouse_event(&self, event: MouseEvent) -> Result<(), Error> {
        self.terminal.borrow_mut().mouse_event(event)
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        if self.tmux_domain.borrow().is_some() {
            log::error!("key: {:?}", key);
            if key == KeyCode::Char('q') {
                self.terminal.borrow_mut().send_paste("detach\n")?;
            }
            return Ok(());
        } else {
            self.terminal.borrow_mut().key_down(key, mods)
        }
    }

    fn resize(&self, size: PtySize) -> Result<(), Error> {
        self.pty.borrow_mut().resize(size)?;
        self.terminal.borrow_mut().resize(
            size.rows as usize,
            size.cols as usize,
            size.pixel_width as usize,
            size.pixel_height as usize,
        );
        Ok(())
    }

    fn writer(&self) -> RefMut<dyn std::io::Write> {
        self.pty.borrow_mut()
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(Some(self.pty.borrow_mut().try_clone_reader()?))
    }

    fn send_paste(&self, text: &str) -> Result<(), Error> {
        if self.tmux_domain.borrow().is_some() {
            Ok(())
        } else {
            self.terminal.borrow_mut().send_paste(text)
        }
    }

    fn get_title(&self) -> String {
        self.terminal.borrow_mut().get_title().to_string()
    }

    fn palette(&self) -> ColorPalette {
        self.terminal.borrow().palette()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        match erase_mode {
            ScrollbackEraseMode::ScrollbackOnly => {
                self.terminal.borrow_mut().erase_scrollback();
            }
            ScrollbackEraseMode::ScrollbackAndViewport => {
                self.terminal.borrow_mut().erase_scrollback_and_viewport();
            }
        }
    }

    fn focus_changed(&self, focused: bool) {
        self.terminal.borrow_mut().focus_changed(focused);
    }

    fn is_mouse_grabbed(&self) -> bool {
        if self.tmux_domain.borrow().is_some() {
            false
        } else {
            self.terminal.borrow().is_mouse_grabbed()
        }
    }

    fn is_alt_screen_active(&self) -> bool {
        if self.tmux_domain.borrow().is_some() {
            false
        } else {
            self.terminal.borrow().is_alt_screen_active()
        }
    }

    fn get_current_working_dir(&self) -> Option<Url> {
        self.terminal
            .borrow()
            .get_current_dir()
            .cloned()
            .or_else(|| self.divine_current_working_dir())
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        if let Some(proc_list) = self.divine_process_list() {
            log::trace!(
                "can_close_without_prompting? procs in pane {:#?}",
                proc_list
            );

            let hook_result = config::run_immediate_with_lua_config(|lua| {
                let lua = match lua {
                    Some(lua) => lua,
                    None => return Ok(None),
                };
                let v = config::lua::emit_sync_callback(
                    &*lua,
                    ("mux-is-process-stateful".to_string(), (proc_list.clone())),
                )?;
                match v {
                    mlua::Value::Nil => Ok(None),
                    mlua::Value::Boolean(v) => Ok(Some(v)),
                    _ => Ok(None),
                }
            });

            fn default_stateful_check(proc_list: &LocalProcessInfo) -> bool {
                let names = proc_list.flatten_to_exe_names();

                let skip = configuration()
                    .skip_close_confirmation_for_processes_named
                    .iter()
                    .cloned()
                    .collect::<HashSet<_>>();

                if !names.is_subset(&skip) {
                    // There are other processes running than are listed,
                    // so we consider this to be stateful
                    return true;
                }
                false
            }

            let is_stateful = match hook_result {
                Ok(None) => default_stateful_check(&proc_list),
                Ok(Some(s)) => s,
                Err(err) => {
                    log::error!(
                        "Error while running mux-is-process-stateful \
                         hook: {:#}, falling back to default behavior",
                        err
                    );
                    default_stateful_check(&proc_list)
                }
            };

            !is_stateful
        } else {
            #[cfg(unix)]
            {
                // If the process is dead but exit_behavior is holding the
                // window, we don't need to prompt to confirm closing.
                // That is detectable as no longer having a process group leader.
                if self.pty.borrow().process_group_leader().is_none() {
                    return true;
                }
            }

            false
        }
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        let term = self.terminal.borrow();
        term.get_semantic_zones()
    }

    async fn search(&self, mut pattern: Pattern) -> anyhow::Result<Vec<SearchResult>> {
        let term = self.terminal.borrow();
        let screen = term.screen();

        if let Pattern::CaseInSensitiveString(s) = &mut pattern {
            // normalize the case so we match everything lowercase
            *s = s.to_lowercase()
        }

        let mut results = vec![];
        let mut haystack = String::new();
        let mut coords = vec![];
        let mut uniq_matches: HashMap<String, usize> = HashMap::new();

        #[derive(Copy, Clone)]
        struct Coord {
            byte_idx: usize,
            grapheme_idx: usize,
            stable_row: StableRowIndex,
        }

        fn haystack_idx_to_coord(idx: usize, coords: &[Coord]) -> (usize, StableRowIndex) {
            let c = coords
                .binary_search_by(|ele| ele.byte_idx.cmp(&idx))
                .or_else(|i| -> Result<usize, usize> { Ok(i) })
                .unwrap();
            let coord = coords.get(c).or_else(|| coords.last()).unwrap();
            (coord.grapheme_idx, coord.stable_row)
        }

        fn collect_matches(
            results: &mut Vec<SearchResult>,
            pattern: &Pattern,
            haystack: &str,
            coords: &[Coord],
            uniq_matches: &mut HashMap<String, usize>,
        ) {
            if haystack.is_empty() {
                return;
            }
            match pattern {
                // Rust only provides a case sensitive match_indices function, so
                // we have to pre-arrange to lowercase both the pattern and the
                // haystack strings
                Pattern::CaseInSensitiveString(s) | Pattern::CaseSensitiveString(s) => {
                    for (idx, s) in haystack.match_indices(s) {
                        let match_id = match uniq_matches.get(s).copied() {
                            Some(id) => id,
                            None => {
                                let id = uniq_matches.len();
                                uniq_matches.insert(s.to_owned(), id);
                                id
                            }
                        };
                        let (start_x, start_y) = haystack_idx_to_coord(idx, coords);
                        let (end_x, end_y) = haystack_idx_to_coord(idx + s.len(), coords);
                        results.push(SearchResult {
                            start_x,
                            start_y,
                            end_x,
                            end_y,
                            match_id,
                        });
                    }
                }
                Pattern::Regex(r) => {
                    if let Ok(re) = regex::Regex::new(r) {
                        // Allow for the regex to contain captures
                        for c in re.captures_iter(haystack) {
                            // Look for the captures in reverse order, as index==0 is
                            // the whole matched string.  We can't just call
                            // `c.iter().rev()` as the capture iterator isn't double-ended.
                            for idx in (0..c.len()).rev() {
                                if let Some(m) = c.get(idx) {
                                    let s = m.as_str();
                                    let match_id = match uniq_matches.get(s).copied() {
                                        Some(id) => id,
                                        None => {
                                            let id = uniq_matches.len();
                                            uniq_matches.insert(s.to_owned(), id);
                                            id
                                        }
                                    };

                                    let (start_x, start_y) =
                                        haystack_idx_to_coord(m.start(), coords);
                                    let (end_x, end_y) = haystack_idx_to_coord(m.end(), coords);
                                    results.push(SearchResult {
                                        start_x,
                                        start_y,
                                        end_x,
                                        end_y,
                                        match_id,
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        for (idx, line) in screen.lines.iter().enumerate() {
            let stable_row = screen.phys_to_stable_row_index(idx);

            let mut wrapped = false;
            for (grapheme_idx, cell) in line.visible_cells() {
                coords.push(Coord {
                    byte_idx: haystack.len(),
                    grapheme_idx,
                    stable_row,
                });

                let s = cell.str();
                if let Pattern::CaseInSensitiveString(_) = &pattern {
                    // normalize the case so we match everything lowercase
                    haystack.push_str(&s.to_lowercase());
                } else {
                    haystack.push_str(cell.str());
                }
                wrapped = cell.attrs().wrapped();
            }

            if !wrapped {
                if let Pattern::Regex(_) = &pattern {
                    if let Some(coord) = coords.last().copied() {
                        coords.push(Coord {
                            byte_idx: haystack.len(),
                            grapheme_idx: coord.grapheme_idx + 1,
                            ..coord
                        });
                        haystack.push('\n');
                    }
                } else {
                    collect_matches(
                        &mut results,
                        &pattern,
                        &haystack,
                        &coords,
                        &mut uniq_matches,
                    );
                    haystack.clear();
                    coords.clear();
                }
            }
        }

        collect_matches(
            &mut results,
            &pattern,
            &haystack,
            &coords,
            &mut uniq_matches,
        );
        Ok(results)
    }
}

struct LocalPaneDCSHandler {
    pane_id: PaneId,
    tmux_domain: Option<Arc<TmuxDomainState>>,
}

impl wezterm_term::DeviceControlHandler for LocalPaneDCSHandler {
    fn handle_device_control(&mut self, control: termwiz::escape::DeviceControlMode) {
        match control {
            DeviceControlMode::Enter(mode) => {
                if !mode.ignored_extra_intermediates
                    && mode.params.len() == 1
                    && mode.params[0] == 1000
                    && mode.intermediates.is_empty()
                {
                    log::info!("tmux -CC mode requested");

                    // Create a new domain to host these tmux tabs
                    let domain = TmuxDomain::new(self.pane_id);
                    let tmux_domain = Arc::clone(&domain.inner);

                    let domain: Arc<dyn Domain> = Arc::new(domain);
                    let mux = Mux::get().expect("to be called on main thread");
                    mux.add_domain(&domain);

                    if let Some(pane) = mux.get_pane(self.pane_id) {
                        let pane = pane.downcast_ref::<LocalPane>().unwrap();
                        pane.tmux_domain
                            .borrow_mut()
                            .replace(Arc::clone(&tmux_domain));
                    }

                    self.tmux_domain.replace(tmux_domain);

                // TODO: do we need to proactively list available tabs here?
                // if so we should arrange to call domain.attach() and make
                // it do the right thing.
                } else {
                    log::warn!("unknown DeviceControlMode::Enter {:?}", mode,);
                }
            }
            DeviceControlMode::Exit => {
                if let Some(tmux) = self.tmux_domain.take() {
                    let mux = Mux::get().expect("to be called on main thread");
                    if let Some(pane) = mux.get_pane(self.pane_id) {
                        let pane = pane.downcast_ref::<LocalPane>().unwrap();
                        pane.tmux_domain.borrow_mut().take();
                    }
                    mux.domain_was_detached(tmux.domain_id);
                }
            }
            DeviceControlMode::Data(c) => {
                if let Some(tmux) = self.tmux_domain.as_ref() {
                    tmux.advance(c);
                } else {
                    log::warn!(
                        "unhandled DeviceControlMode::Data {:x} {}",
                        c,
                        (c as char).escape_debug()
                    );
                }
            }
            _ => {
                log::warn!("unhandled: {:?}", control);
            }
        }
    }
}

struct LocalPaneNotifHandler {
    pane_id: PaneId,
}

impl AlertHandler for LocalPaneNotifHandler {
    fn alert(&mut self, alert: Alert) {
        if let Some(mux) = Mux::get() {
            mux.notify(MuxNotification::Alert {
                pane_id: self.pane_id,
                alert,
            });
        }
    }
}

/// This is a little gross; on some systems, our pipe reader will continue
/// to be blocked in read even after the child process has died.
/// We need to wake up and notice that the child terminated in order
/// for our state to wind down.
/// This block schedules a background thread to wait for the child
/// to terminate, and then nudge the muxer to check for dead processes.
/// Without this, typing `exit` in `cmd.exe` would keep the pane around
/// until something else triggered the mux to prune dead processes.
fn split_child(
    mut process: Box<dyn Child + Send>,
) -> (
    Receiver<IoResult<ExitStatus>>,
    Box<dyn ChildKiller>,
    Option<u32>,
) {
    let pid = process.process_id();
    let signaller = process.clone_killer();

    let (tx, rx) = bounded(1);

    std::thread::spawn(move || {
        let status = process.wait();
        tx.try_send(status).ok();
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get().unwrap();
            mux.prune_dead_windows();
        })
        .detach();
    });

    (rx, signaller, pid)
}

impl LocalPane {
    pub fn new(
        pane_id: PaneId,
        mut terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        domain_id: DomainId,
    ) -> Self {
        let (process, signaller, pid) = split_child(process);

        terminal.set_device_control_handler(Box::new(LocalPaneDCSHandler {
            pane_id,
            tmux_domain: None,
        }));
        terminal.set_notification_handler(Box::new(LocalPaneNotifHandler { pane_id }));
        Self {
            pane_id,
            terminal: RefCell::new(terminal),
            process: RefCell::new(ProcessState::Running {
                child_waiter: process,
                pid,
                signaller,
                killed: false,
            }),
            pty: RefCell::new(pty),
            domain_id,
            tmux_domain: RefCell::new(None),
        }
    }

    #[cfg(target_os = "macos")]
    fn divine_current_working_dir_macos(&self) -> Option<Url> {
        if let Some(pid) = self.pty.borrow().process_group_leader() {
            extern "C" {
                fn proc_pidinfo(
                    pid: libc::pid_t,
                    flavor: libc::c_int,
                    arg: u64,
                    buffer: *mut proc_vnodepathinfo,
                    buffersize: libc::c_int,
                ) -> libc::c_int;
            }
            const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
            #[repr(C)]
            struct vinfo_stat {
                vst_dev: u32,
                vst_mode: u16,
                vst_nlink: u16,
                vst_ino: u64,
                vst_uid: libc::uid_t,
                vst_gid: libc::gid_t,
                vst_atime: i64,
                vst_atimensec: i64,
                vst_mtime: i64,
                vst_mtimensec: i64,
                vst_ctime: i64,
                vst_ctimensec: i64,
                vst_birthtime: i64,
                vst_birthtimensec: i64,
                vst_size: libc::off_t,
                vst_blocks: i64,
                vst_blksize: i32,
                vst_flags: u32,
                vst_gen: u32,
                vst_rdev: u32,
                vst_qspare_1: i64,
                vst_qspare_2: i64,
            }
            #[repr(C)]
            struct vnode_info {
                vi_stat: vinfo_stat,
                vi_type: libc::c_int,
                vi_pad: libc::c_int,
                vi_fsid: libc::fsid_t,
            }

            const MAXPATHLEN: usize = 1024;
            #[repr(C)]
            struct vnode_info_path {
                vip_vi: vnode_info,
                vip_path: [i8; MAXPATHLEN],
            }

            #[repr(C)]
            struct proc_vnodepathinfo {
                pvi_cdir: vnode_info_path,
                pvi_rdir: vnode_info_path,
            }

            let mut pathinfo: proc_vnodepathinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of_val(&pathinfo) as libc::c_int;
            let ret = unsafe { proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &mut pathinfo, size) };
            if ret == size {
                let path = unsafe { std::ffi::CStr::from_ptr(pathinfo.pvi_cdir.vip_path.as_ptr()) };
                if let Ok(s) = path.to_str() {
                    return Url::parse(&format!("file://localhost{}", s)).ok();
                }
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    fn divine_current_working_dir_linux(&self) -> Option<Url> {
        if let Some(pid) = self.pty.borrow().process_group_leader() {
            if let Ok(path) = std::fs::read_link(format!("/proc/{}/cwd", pid)) {
                return Url::parse(&format!("file://localhost{}", path.display())).ok();
            }
        }
        None
    }

    fn divine_current_working_dir(&self) -> Option<Url> {
        #[cfg(target_os = "linux")]
        {
            return self.divine_current_working_dir_linux();
        }

        #[cfg(target_os = "macos")]
        {
            return self.divine_current_working_dir_macos();
        }

        #[allow(unreachable_code)]
        None
    }

    fn divine_process_list(&self) -> Option<LocalProcessInfo> {
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        if let ProcessState::Running { pid: Some(pid), .. } = &*self.process.borrow() {
            return LocalProcessInfo::with_root_pid(*pid);
        }

        None
    }
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub enum LocalProcessStatus {
    Idle,
    Run,
    Sleep,
    Stop,
    Zombie,
    Tracing,
    Dead,
    Wakekill,
    Waking,
    Parked,
    LockBlocked,
    Unknown,
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
impl LocalProcessStatus {
    fn from_process_status(status: sysinfo::ProcessStatus) -> Self {
        match status {
            sysinfo::ProcessStatus::Idle => Self::Idle,
            sysinfo::ProcessStatus::Run => Self::Run,
            sysinfo::ProcessStatus::Sleep => Self::Sleep,
            sysinfo::ProcessStatus::Stop => Self::Stop,
            sysinfo::ProcessStatus::Zombie => Self::Zombie,
            sysinfo::ProcessStatus::Tracing => Self::Tracing,
            sysinfo::ProcessStatus::Dead => Self::Dead,
            sysinfo::ProcessStatus::Wakekill => Self::Wakekill,
            sysinfo::ProcessStatus::Waking => Self::Waking,
            sysinfo::ProcessStatus::Parked => Self::Parked,
            sysinfo::ProcessStatus::LockBlocked => Self::LockBlocked,
            sysinfo::ProcessStatus::Unknown(_) => Self::Unknown,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LocalProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub executable: PathBuf,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub status: LocalProcessStatus,
    pub children: HashMap<u32, LocalProcessInfo>,
}
luahelper::impl_lua_conversion!(LocalProcessInfo);

impl LocalProcessInfo {
    fn flatten_to_exe_names(&self) -> HashSet<String> {
        let mut names = HashSet::new();

        fn flatten(item: &LocalProcessInfo, names: &mut HashSet<String>) {
            if let Some(exe) = item.executable.file_name() {
                names.insert(exe.to_string_lossy().into_owned());
            }
            for proc in item.children.values() {
                flatten(proc, names);
            }
        }

        flatten(self, &mut names);
        names
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
impl LocalProcessInfo {
    fn with_root_pid(pid: u32) -> Option<Self> {
        use sysinfo::{
            AsU32, Pid, Process, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt,
        };
        let system = System::new_with_specifics(
            RefreshKind::new().with_processes(ProcessRefreshKind::new()),
        );

        fn build_proc(proc: &Process, processes: &HashMap<Pid, Process>) -> LocalProcessInfo {
            // Process has a `tasks` field but it does not correspond to child processes,
            // so we need to repeatedly walk the full process list and establish that
            // linkage for ourselves here
            let mut children = HashMap::new();
            let pid = proc.pid();
            for (child_pid, child_proc) in processes {
                if child_proc.parent() == Some(pid) {
                    children.insert(child_pid.as_u32(), build_proc(child_proc, processes));
                }
            }

            LocalProcessInfo {
                pid: proc.pid().as_u32(),
                ppid: proc.parent().map(|pid| pid.as_u32()).unwrap_or(1),
                name: proc.name().to_string(),
                executable: proc.exe().to_path_buf(),
                cwd: proc.cwd().to_path_buf(),
                argv: proc.cmd().to_vec(),
                status: LocalProcessStatus::from_process_status(proc.status()),
                children,
            }
        }

        let proc = system.process(pid as Pid)?;
        let procs = system.processes();

        Some(build_proc(&proc, &procs))
    }
}

impl Drop for LocalPane {
    fn drop(&mut self) {
        // Avoid lingering zombies if we can, but don't block forever.
        // <https://github.com/wez/wezterm/issues/558>
        if let ProcessState::Running { signaller, .. } = &mut *self.process.borrow_mut() {
            let _ = signaller.kill();
        }
    }
}
