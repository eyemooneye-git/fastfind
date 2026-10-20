//! The egui front-end plus the background indexing pipeline.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::index::{Entry, VolumeIndex};
use crate::mft;
use crate::search::{self, Request, Response, VolumeSet};
use crate::volume::{self, Volume};
use crate::winutil::{
    clipboard_files, clipboard_text, format_count, format_filetime, format_size, open_folder,
    pick_folder,
};
use crate::{elevate, flog};

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
    Start {
        drive: char,
        total: u64,
    },
    Progress {
        drive: char,
        done: u64,
    },
    Done {
        drive: char,
        index: Arc<VolumeIndex>,
    },
    Failed {
        drive: char,
        msg: String,
    },
    AllDone,
}

#[derive(Clone, Copy)]
enum RowAction {
    CopyFile,
    CopyPath,
    CopyName,
}

pub struct FastFindApp {
    pattern: String,
    dir: String,
    gen: u64,
    req_tx: Sender<Request>,
    res_rx: Receiver<Response>,
    response: Option<Response>,
    pending: bool,

    volumes: VolumeSet,
    vol_states: Vec<VolState>,
    index_tx: Sender<IndexEvent>,
    index_rx: Receiver<IndexEvent>,
    indexing: bool,

    selected: Option<usize>,
    elevated: bool,
    status: String,
    first_frame: bool,
    icon_set: bool,

    /// Text that the last request was built from, to detect edits.
    last_pattern: String,
    last_dir: String,
    /// Debounce deadline for live search.
    search_due: Option<Instant>,

    /// `--screenshot` debug mode.
    shot: Option<std::path::PathBuf>,
    /// `--window-shot` debug mode: captures the decorated window (title bar).
    win_shot: Option<std::path::PathBuf>,
    /// `--copy-test` debug mode: pushes text to the clipboard once.
    copy_test: Option<String>,
    copy_sent: bool,
    shot_pattern: String,
    shot_dir: String,
    frames: u32,
    shot_requested: bool,
}

impl FastFindApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shot: Option<std::path::PathBuf>,
        win_shot: Option<std::path::PathBuf>,
        copy_test: Option<String>,
        shot_pattern: String,
        shot_dir: String,
    ) -> Self {
        install_fonts(&cc.egui_ctx);
        let volumes: VolumeSet = Arc::new(RwLock::new(Vec::new()));
        let (req_tx, res_rx) = search::spawn_worker(volumes.clone());
        let (index_tx, index_rx) = channel();
        let elevated = elevate::is_elevated();
        if elevated {
            start_indexer(index_tx.clone());
        }
        Self {
            pattern: String::new(),
            dir: String::new(),
            gen: 0,
            req_tx,
            res_rx,
            response: None,
            pending: false,
            volumes,
            vol_states: Vec::new(),
            index_tx,
            index_rx,
            indexing: elevated,
            selected: None,
            elevated,
            status: String::new(),
            first_frame: true,
            icon_set: false,
            last_pattern: String::new(),
            last_dir: String::new(),
            search_due: None,
            shot,
            win_shot,
            copy_test,
            copy_sent: false,
            shot_pattern,
            shot_dir,
            frames: 0,
            shot_requested: false,
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

    /// Run the current query now (unless nothing has been indexed yet).
    fn search_now(&mut self) {
        self.search_due = None;
        if self.snapshot_volumes().is_empty() {
            return;
        }
        self.trigger_search();
    }

    /// Live search: schedule a request 130 ms after the last edit.
    fn poll_input(&mut self, ctx: &egui::Context) {
        if self.pattern != self.last_pattern || self.dir != self.last_dir {
            self.last_pattern = self.pattern.clone();
            self.last_dir = self.dir.clone();
            if self.pattern.trim().is_empty() {
                self.gen += 1;
                self.response = None;
                self.pending = false;
                self.selected = None;
                self.status.clear();
                self.search_due = None;
            } else {
                self.search_due = Some(Instant::now() + Duration::from_millis(130));
            }
        }
        if let Some(due) = self.search_due {
            let now = Instant::now();
            if now >= due {
                self.search_now();
            } else {
                ctx.request_repaint_after(due - now);
            }
        }
    }

    fn poll(&mut self) {
        while let Ok(ev) = self.index_rx.try_recv() {
            match ev {
                IndexEvent::Start { drive, total } => {
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
                    self.trigger_search();
                }
                IndexEvent::Failed { drive, msg } => {
                    flog!("盘 {} 索引失败: {}", drive, msg);
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
                    }
                    self.vol_states.sort_by_key(|v| v.drive);
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
                self.response = Some(resp);
                self.pending = false;
            }
        }
    }
}

impl eframe::App for FastFindApp {
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        // Results are rows, not selectable text: keep the normal arrow cursor.
        root.style_mut().interaction.selectable_labels = false;
        if !self.icon_set {
            self.icon_set = crate::icon::install_window_icon(frame);
        }
        self.poll();
        self.poll_input(&ctx);
        if self.indexing || self.pending || self.search_due.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        }

        if self.shot.is_some() || self.win_shot.is_some() || self.copy_test.is_some() {
            self.frames = self.frames.saturating_add(1);
            ctx.request_repaint_after(Duration::from_millis(16));
            if let Some(text) = &self.copy_test {
                if !self.copy_sent && self.frames == 30 {
                    self.copy_sent = true;
                    ctx.copy_text(text.clone());
                    flog!("copy-test: ctx.copy_text({} chars)", text.chars().count());
                }
                if self.frames == 240 {
                    std::process::exit(0);
                }
            }
            if self.win_shot.is_some() && self.frames == 50 {
                if let Some(hwnd) = crate::icon::app_hwnd(frame) {
                    if let Some(path) = &self.win_shot {
                        if let Err(e) = crate::winutil::capture_window(hwnd, path) {
                            flog!("窗口截图失败: {e:#}");
                        }
                    }
                }
                std::process::exit(0);
            }
        }
        if self.shot.is_some() {
            if self.frames == 20 {
                // Set the fields and let the normal live-search path run, which is
                // exactly what a user typing would trigger.
                self.pattern = self.shot_pattern.clone();
                self.dir = self.shot_dir.clone();
            }
            if !self.shot_requested && self.frames > 40 && !self.indexing && !self.pending {
                self.shot_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
                if let Some(path) = self.shot.clone() {
                    ctx.request_screenshot(move |image| {
                        let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
                        for p in &image.pixels {
                            rgba.extend_from_slice(&p.to_array());
                        }
                        let _ = crate::png::write_rgba(
                            &path,
                            image.width(),
                            image.height(),
                            &rgba,
                        );
                        std::process::exit(0);
                    });
                }
            }
        }

        let mut reindex = false;
        let mut search_clicked = false;
        let mut pick_dir = false;
        egui::Panel::top("inputs").show(root, |ui| {
            ui.add_space(6.0);
            egui::Grid::new("input_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .min_col_width(60.0)
                .show(ui, |ui| {
                    ui.label(crate::i18n::tr("文件名", "Name"));
                    let w = (ui.available_width() - 4.0).max(220.0);
                    let pat = ui.add_sized(
                        [w, 24.0],
                        egui::TextEdit::singleline(&mut self.pattern).hint_text(
                            crate::i18n::tr(
                                "输入即搜索；支持 * 和 ?，例如 *.log、report_???.docx",
                                "Search as you type; * and ? are supported, e.g. *.log, report_???.docx",
                            ),
                        ),
                    );
                    if self.first_frame {
                        pat.request_focus();
                        self.first_frame = false;
                    }
                    if pat.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        search_clicked = true;
                    }
                    ui.end_row();

                    ui.label(crate::i18n::tr("目录", "Folder"));
                    let w = (ui.available_width() - 4.0).max(220.0);
                    let dir_edit = ui.add_sized(
                        [w, 24.0],
                        egui::TextEdit::singleline(&mut self.dir).hint_text(crate::i18n::tr(
                            "留空 = 全部磁盘；双击选择文件夹",
                            "Empty = all drives; double-click to pick a folder",
                        )),
                    );
                    if dir_edit.double_clicked() {
                        pick_dir = true;
                    }
                    if dir_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        search_clicked = true;
                    }
                    ui.end_row();
                });

            ui.horizontal(|ui| {
                if ui.button(crate::i18n::tr("重新索引", "Reindex")).clicked() {
                    reindex = true;
                }
                if ui.button(crate::i18n::tr("清空", "Clear")).clicked() {
                    self.pattern.clear();
                    self.dir.clear();
                    self.selected = None;
                    self.response = None;
                    self.pending = false;
                    self.gen += 1;
                    self.status.clear();
                    self.last_pattern.clear();
                    self.last_dir.clear();
                    self.search_due = None;
                }
                let hits = self.response.as_ref().map(|r| r.hits.len()).unwrap_or(0);
                let truncated = self
                    .response
                    .as_ref()
                    .map(|r| r.truncated)
                    .unwrap_or(false);
                let ms = self.response.as_ref().map(|r| r.ms).unwrap_or(0);
                let text = crate::i18n::hits_label(hits, truncated, ms);
                ui.label(text);
                if let Some(r) = &self.response {
                    ui.separator();
                    ui.label(crate::i18n::scope_label(&r.dir_label));
                }
            });

            for v in &self.vol_states {
                match v.status {
                    VolStatus::Scanning => {
                        let frac = if v.total > 0 {
                            (v.done as f32 / v.total as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::indexing_label(v.drive));
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .desired_width(300.0)
                                    .show_percentage(),
                            );
                            ui.label(format!(
                                "{} / {}",
                                format_size(v.done),
                                format_size(v.total)
                            ));
                        });
                    }
                    VolStatus::Done => {
                        let (f, d, b, skipped) = v.stats.unwrap_or_default();
                        let skipped = (skipped > 0).then(|| format_count(skipped));
                        ui.label(crate::i18n::volume_line(
                            v.drive,
                            &format_count(f),
                            &format_count(d),
                            &format_size(b),
                            v.ms,
                            skipped.as_deref(),
                        ));
                    }
                    VolStatus::Failed => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 90, 90),
                            format!("{}: {}", v.drive, v.err.clone().unwrap_or_default()),
                        );
                    }
                }
            }
            ui.add_space(4.0);
        });
        if search_clicked {
            self.search_now();
        }
        if pick_dir {
            if let Some(path) = pick_folder(crate::i18n::pick_folder_title()) {
                self.dir = path;
                ctx.request_repaint();
            }
        }

        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal(|ui| {
                let (files, dirs, bytes) = self.total_stats();
                ui.label(crate::i18n::total_line(
                    &format_count(files),
                    &format_count(dirs),
                    &format_size(bytes),
                ));
                ui.separator();
                if !self.status.is_empty() {
                    ui.colored_label(egui::Color32::from_rgb(220, 160, 60), self.status.clone());
                } else if let Some(path) = self.selected_path() {
                    ui.add(egui::Label::new(path).truncate());
                } else {
                    ui.label(crate::i18n::tr(
                        "双击或 Enter 打开所在文件夹 · 右键菜单 · Ctrl+C 复制文件",
                        "Double-click or Enter opens the folder · right-click for menu · Ctrl+C copies the file",
                    ));
                }
            });
        });

        let mut elevation_error: Option<String> = None;
        egui::CentralPanel::default().show(root, |ui| {
            if !self.elevated {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.heading(crate::i18n::tr("需要管理员权限", "Administrator rights required"));
                    ui.add_space(6.0);
                    ui.label(crate::i18n::tr(
                        "读取 NTFS 的 $MFT 需要管理员权限，请以管理员身份重新启动。",
                        "Reading the NTFS $MFT requires administrator rights. Please restart as administrator.",
                    ));
                    ui.add_space(12.0);
                    if ui
                        .button(crate::i18n::tr(
                            "以管理员身份重新启动",
                            "Restart as administrator",
                        ))
                        .clicked()
                    {
                        if let Err(e) = elevate::relaunch_elevated() {
                            elevation_error = Some(format!("{e}"));
                        } else {
                            std::process::exit(0);
                        }
                    }
                });
                return;
            }
            if self.pattern.trim().is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(crate::i18n::tr(
                        "输入文件名通配符开始搜索\n\n例如：*.log      report_???.docx      *.mp4",
                        "Type a file name wildcard to start\n\ne.g. *.log      report_???.docx      *.mp4",
                    ));
                });
                return;
            }
            self.draw_results(ui);
        });
        if let Some(e) = elevation_error {
            self.status = e;
        }

        // Keyboard shortcuts only when a text field isn't capturing the keys.
        let editing = ctx.memory(|m| m.focused().is_some());
        if !editing && self.selected.is_some() {
            // Enter pressed inside an input field only triggers a search.
            if !search_clicked && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Some(dir) = self.selected_folder() {
                    open_folder(&dir);
                }
            }
            // egui-winit turns Ctrl+C into `Event::Copy` before the key ever
            // reaches `key_pressed`, so listen for that event (and keep the key
            // check for layouts/shortcuts that still produce one).
            let copy_pressed = ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Copy))
                    || (i.modifiers.command && i.key_pressed(egui::Key::C))
            });
            if copy_pressed {
                let hit = self.selected.and_then(|sel| {
                    self.response
                        .as_ref()
                        .and_then(|r| r.hits.get(sel).copied())
                });
                if let Some(hit) = hit {
                    let vols = self.snapshot_volumes();
                    if let Some(path) = path_of(&vols, hit) {
                        let name = entry_of(&vols, hit)
                            .map(|(v, e)| v.name(e).to_owned())
                            .unwrap_or_else(|| path.clone());
                        match clipboard_files(&[path], false) {
                            Ok(()) => self.status = crate::i18n::copy_status(&name),
                            Err(e) => self.status = format!("{e}"),
                        }
                    }
                }
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.pattern.clear();
            self.dir.clear();
            self.selected = None;
        }

        if reindex {
            if let Ok(mut g) = self.volumes.write() {
                g.clear();
            }
            self.vol_states.clear();
            self.selected = None;
            self.response = None;
            self.indexing = true;
            start_indexer(self.index_tx.clone());
        }
    }
}

impl FastFindApp {
    fn selected_path(&self) -> Option<String> {
        let sel = self.selected?;
        let hit = self.response.as_ref()?.hits.get(sel).copied()?;
        path_of(&self.snapshot_volumes(), hit)
    }

    fn selected_folder(&self) -> Option<String> {
        let sel = self.selected?;
        let hit = self.response.as_ref()?.hits.get(sel).copied()?;
        folder_of(&self.snapshot_volumes(), hit)
    }

    fn draw_results(&mut self, ui: &mut egui::Ui) {
        let vols = self.snapshot_volumes();
        let hits = match &self.response {
            Some(r) => r.hits.clone(),
            None => Arc::new(Vec::new()),
        };
        let mut clicked: Option<usize> = None;
        let mut dbl: Option<usize> = None;
        let mut action: Option<(usize, RowAction)> = None;
        let selected = self.selected;

        let table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(300.0).at_least(120.0).clip(true))
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(90.0).at_least(70.0).clip(true))
            .column(Column::initial(130.0).at_least(110.0).clip(true))
            .min_scrolled_height(0.0)
            .auto_shrink([false, false]);

        table
            .header(24.0, |mut header| {
                header.col(|ui| {
                    ui.strong(crate::i18n::tr("名称", "Name"));
                });
                header.col(|ui| {
                    ui.strong(crate::i18n::tr("所在目录", "Folder"));
                });
                header.col(|ui| {
                    ui.strong(crate::i18n::tr("大小", "Size"));
                });
                header.col(|ui| {
                    ui.strong(crate::i18n::tr("修改时间", "Modified"));
                });
            })
            .body(|body| {
                body.rows(22.0, hits.len(), |mut row| {
                    let i = row.index();
                    let hit = hits[i];
                    let found = vols
                        .iter()
                        .find(|v| v.drive == hit.0)
                        .and_then(|v| v.entries.get(hit.1 as usize).map(|e| (v, e)));
                    row.set_selected(selected == Some(i));
                    match found {
                        Some((v, e)) => {
                            let name = v.name(e);
                            row.col(|ui| {
                                if e.is_dir {
                                    ui.colored_label(egui::Color32::from_rgb(120, 190, 255), name);
                                } else {
                                    ui.add(egui::Label::new(name).truncate());
                                }
                            });
                            row.col(|ui| {
                                ui.add(egui::Label::new(v.prefix(e)).truncate());
                            });
                            row.col(|ui| {
                                if !e.is_dir {
                                    ui.label(format_size(e.size));
                                }
                            });
                            row.col(|ui| {
                                ui.label(format_filetime(e.mtime));
                            });
                        }
                        None => {
                            row.col(|_ui| {});
                            row.col(|_ui| {});
                            row.col(|_ui| {});
                            row.col(|_ui| {});
                        }
                    }
                    let resp = row.response();
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                    if resp.double_clicked() {
                        dbl = Some(i);
                    }
                    resp.context_menu(|ui| {
                        if ui.button(crate::i18n::tr("复制文件", "Copy file")).clicked() {
                            action = Some((i, RowAction::CopyFile));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(crate::i18n::tr("复制文件名", "Copy file name")).clicked() {
                            action = Some((i, RowAction::CopyName));
                            ui.close();
                        }
                        if ui.button(crate::i18n::tr("复制路径", "Copy path")).clicked() {
                            action = Some((i, RowAction::CopyPath));
                            ui.close();
                        }
                    });
                });
            });

        if let Some(i) = clicked.or(dbl) {
            self.selected = Some(i);
        }
        if let Some(i) = dbl {
            if let Some(dir) = folder_of(&vols, hits[i]) {
                open_folder(&dir);
            }
        }
        if let Some((i, act)) = action {
            self.selected = Some(i);
            let hit = hits[i];
            match act {
                RowAction::CopyFile => {
                    if let Some(path) = path_of(&vols, hit) {
                        let name = entry_of(&vols, hit)
                            .map(|(v, e)| v.name(e).to_owned())
                            .unwrap_or_else(|| path.clone());
                        match clipboard_files(&[path], false) {
                            Ok(()) => self.status = crate::i18n::copy_status(&name),
                            Err(e) => self.status = format!("{e}"),
                        }
                    }
                }
                RowAction::CopyPath => {
                    if let Some(path) = path_of(&vols, hit) {
                        match clipboard_text(&path) {
                            Ok(()) => {
                                self.status = crate::i18n::copied_path_status().to_owned()
                            }
                            Err(e) => self.status = format!("{e}"),
                        }
                    }
                }
                RowAction::CopyName => {
                    if let Some(name) = entry_of(&vols, hit).map(|(v, e)| v.name(e).to_owned()) {
                        match clipboard_text(&name) {
                            Ok(()) => {
                                self.status = crate::i18n::copied_name_status().to_owned()
                            }
                            Err(e) => self.status = format!("{e}"),
                        }
                    }
                }
            }
        }
    }
}

fn entry_of<'a>(
    vols: &'a [Arc<VolumeIndex>],
    hit: (char, u32),
) -> Option<(&'a Arc<VolumeIndex>, &'a Entry)> {
    let v = vols.iter().find(|v| v.drive == hit.0)?;
    let e = v.entries.get(hit.1 as usize)?;
    Some((v, e))
}

fn path_of(vols: &[Arc<VolumeIndex>], hit: (char, u32)) -> Option<String> {
    entry_of(vols, hit).map(|(v, e)| v.full_path(e))
}

/// The folder shown when a row is opened: the entry's parent directory.
fn folder_of(vols: &[Arc<VolumeIndex>], hit: (char, u32)) -> Option<String> {
    entry_of(vols, hit).map(|(v, e)| v.prefix(e).to_owned())
}

fn start_indexer(tx: Sender<IndexEvent>) {
    std::thread::Builder::new()
        .name("fastfind-index".to_owned())
        .spawn(move || {
            let drives = volume::fixed_drives();
            if drives.is_empty() {
                let _ = tx.send(IndexEvent::Failed {
                    drive: '?',
                    msg: crate::i18n::no_fixed_drives().to_owned(),
                });
            }
            for d in drives {
                if !d.fs.eq_ignore_ascii_case("NTFS") {
                    let _ = tx.send(IndexEvent::Failed {
                        drive: d.letter,
                        msg: crate::i18n::fs_unsupported(&d.fs),
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
                let last = std::cell::Cell::new(Instant::now());
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
                        let index =
                            Arc::new(VolumeIndex::build(d.letter, scan, ms));
                        flog!(
                            "盘 {} 索引完成: {} 文件, {} 目录, {} ms",
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

fn install_fonts(ctx: &egui::Context) {
    // egui's bundled fonts have no CJK glyphs; borrow one from Windows.
    const CANDIDATES: &[&str] = &[
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyhl.ttc",
        "C:\\Windows\\Fonts\\msyhbd.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "C:\\Windows\\Fonts\\Deng.ttf",
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if !looks_like_font(&bytes) {
            continue;
        }
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        flog!("已加载中文字体 {}", path);
        return;
    }
    flog!("未找到可用的中文字体，中文可能显示为方块");
}

fn looks_like_font(b: &[u8]) -> bool {
    if b.len() < 12 {
        return false;
    }
    let tag = &b[0..4];
    tag == [0x00, 0x01, 0x00, 0x00] || tag == b"OTTO" || tag == b"true" || tag == b"ttcf"
}
