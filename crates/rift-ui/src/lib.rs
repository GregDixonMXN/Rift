//! Rift graphical review surface (eframe/egui).
//! Minimal, dense, keyboard-first: Overview / Queue / Files / Diff.

use rift_core::{
    Category, ChangeSet, ChangeStats, DiffLineKind, FileChange, ReviewItem, Severity, SymbolChange,
};
use std::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Overview,
    Queue,
    Files,
    Detail,
    Diff,
}

pub struct RiftApp {
    cs: ChangeSet,
    view: View,
    selected: usize,
    file_selected: usize,
    search: String,
    show_mechanical: bool,
    /// Per-file analysis progress (progressive mode). All true when done.
    analyzed: Vec<bool>,
    done: bool,
    rx: Option<mpsc::Receiver<UiMsg>>,
    /// Flattened raw-diff rows for virtualized rendering.
    diff_rows: Vec<DiffRow>,
}

/// Messages from the background analysis worker (see [`RiftApp::pending`]).
#[derive(Debug)]
pub enum UiMsg {
    /// One file's symbols are done (index into `cs.files`).
    Progress { index: usize },
    /// Grouping + stats finished.
    Finished {
        items: Vec<ReviewItem>,
        stats: ChangeStats,
        syms: Vec<SymbolChange>,
    },
}

/// One virtualized row of the raw-diff view.
#[derive(Debug, Clone, Copy)]
enum DiffRow {
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
}

fn flatten_diff_rows(files: &[FileChange]) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        rows.push(DiffRow::File(fi));
        for (hi, h) in f.hunks.iter().enumerate() {
            rows.push(DiffRow::Hunk(fi, hi));
            for (li, _) in h.lines.iter().enumerate() {
                rows.push(DiffRow::Line(fi, hi, li));
            }
        }
    }
    rows
}

impl RiftApp {
    pub fn new(cs: ChangeSet) -> Self {
        let n = cs.files.len();
        let diff_rows = flatten_diff_rows(&cs.files);
        Self {
            cs,
            view: View::Overview,
            selected: 0,
            file_selected: 0,
            search: String::new(),
            show_mechanical: false,
            analyzed: vec![true; n],
            done: true,
            rx: None,
            diff_rows,
        }
    }

    /// App with files known but analysis still running. The worker streams
    /// progress; the window paints instantly and fills in as results land.
    pub fn pending(
        repo_root: String,
        base_ref: String,
        head_ref: String,
        files: Vec<FileChange>,
        rx: mpsc::Receiver<UiMsg>,
    ) -> Self {
        let n = files.len();
        let diff_rows = flatten_diff_rows(&files);
        let stats = ChangeStats {
            files_changed: n,
            ..Default::default()
        };
        let cs = ChangeSet {
            repo_root,
            base_ref,
            head_ref,
            files,
            symbol_changes: Vec::new(),
            review_items: Vec::new(),
            stats,
        };
        Self {
            cs,
            view: View::Overview,
            selected: 0,
            file_selected: 0,
            search: String::new(),
            show_mechanical: false,
            analyzed: vec![false; n],
            done: false,
            rx: Some(rx),
            diff_rows,
        }
    }

    /// Drain worker messages (bounded per frame). Returns true if anything landed.
    fn drain(&mut self) -> bool {
        let mut touched = false;
        for _ in 0..64 {
            let msg = match self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                Some(m) => m,
                None => break,
            };
            touched = true;
            match msg {
                UiMsg::Progress { index } => {
                    if let Some(slot) = self.analyzed.get_mut(index) {
                        *slot = true;
                    }
                }
                UiMsg::Finished { items, stats, syms } => {
                    self.cs.review_items = items;
                    self.cs.stats = stats;
                    self.cs.symbol_changes = syms;
                    self.done = true;
                    self.analyzed.fill(true);
                }
            }
        }
        touched
    }

    fn analyzed_count(&self) -> usize {
        self.analyzed.iter().filter(|&&b| b).count()
    }

    fn visible_items(&self) -> Vec<usize> {
        let q = self.search.to_lowercase();
        self.cs
            .review_items
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                if !self.show_mechanical && matches!(r.category, Category::Mechanical) {
                    return false;
                }
                if q.is_empty() {
                    return true;
                }
                r.title.to_lowercase().contains(&q)
                    || r.files.iter().any(|f| f.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_item(&self) -> Option<usize> {
        self.visible_items().get(self.selected).copied()
    }
}

pub fn run_native(cs: ChangeSet) -> eframe::Result<()> {
    let title = format!(
        "Rift — {} ({} files, +{} −{})",
        cs.base_ref, cs.stats.files_changed, cs.stats.added_lines, cs.stats.deleted_lines
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_title(&title),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(RiftApp::new(cs)))
        }),
    )
}

/// Open the window immediately with the file list; parse + score on a worker
/// thread and stream results in. First paint never waits for analysis.
pub fn run_native_progressive(
    repo_root: String,
    base_ref: String,
    head_ref: String,
    files: Vec<FileChange>,
) -> eframe::Result<()> {
    use std::sync::Arc;
    let mut files = files;
    // Generated flags up front so file_symbols can skip cheaply per file.
    // (analyze() re-marks idempotently; flags are plain booleans.)
    rift_analysis::mark_generated(&mut files);
    let files = Arc::new(files);
    let (tx, rx) = mpsc::channel();
    let title = format!("Rift — {} ({} files)", base_ref, files.len());

    // Analysis worker: per-file symbols (progress) then grouping + stats.
    {
        let wfiles = Arc::clone(&files);
        std::thread::spawn(move || {
            let mut syms_all = Vec::new();
            for (i, f) in wfiles.iter().enumerate() {
                syms_all.extend(rift_analysis::file_symbols(f));
                if tx.send(UiMsg::Progress { index: i }).is_err() {
                    return; // UI closed.
                }
            }
            let items = rift_analysis::group_items(&wfiles, &syms_all);
            let stats = rift_analysis::compute_stats(&wfiles, &syms_all, &items);
            let _ = tx.send(UiMsg::Finished {
                items,
                stats,
                syms: syms_all,
            });
        });
    }

    let app_files: Vec<FileChange> = (*files).clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_title(&title),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(RiftApp::pending(
                repo_root, base_ref, head_ref, app_files, rx,
            )))
        }),
    )
}

impl eframe::App for RiftApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Stream worker results; keep repainting until analysis lands.
        if self.rx.is_some() {
            self.drain();
            if !self.done {
                ctx.request_repaint();
            }
        }
        // Keyboard first.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::J)) {
            let n = self.visible_items().len();
            if n > 0 {
                self.selected = (self.selected + 1).min(n - 1);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::K)) {
            self.selected = self.selected.saturating_sub(1);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::O)) {
            self.view = View::Overview;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::D)) {
            self.view = View::Diff;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F)) {
            self.view = View::Files;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::N)) {
            // Next meaningful (non-test, non-mechanical) change.
            let vis = self.visible_items();
            let mut i = self.selected + 1;
            while i < vis.len() {
                let r = &self.cs.review_items[vis[i]];
                if !matches!(r.category, Category::Mechanical | Category::Test) {
                    self.selected = i;
                    break;
                }
                i += 1;
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            self.view = View::Detail;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.view = View::Overview;
        }

        egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("◈ Rift");
                ui.label(format!(
                    "{}  ·  {} files  ·  +{} −{}  ·  {} meaningful",
                    self.cs.base_ref,
                    self.cs.stats.files_changed,
                    self.cs.stats.added_lines,
                    self.cs.stats.deleted_lines,
                    self.cs.stats.meaningful_changes,
                ));
                if !self.done {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.show_mechanical, "mechanical");
                    let tabs = [
                        ("Overview (o)", View::Overview),
                        ("Queue", View::Queue),
                        ("Files (f)", View::Files),
                        ("Diff (d)", View::Diff),
                    ];
                    for (label, v) in tabs {
                        if ui.selectable_label(self.view == v, label).clicked() {
                            self.view = v;
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("search ( / ):");
                let resp = ui.text_edit_singleline(&mut self.search);
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Slash)) {
                    resp.request_focus();
                }
                ui.label("j/k move · enter inspect · esc back · n next meaningful");
            });
        });

        match self.view {
            View::Overview => show_overview(self, ctx),
            View::Queue => show_queue(self, ctx),
            View::Files => show_files(self, ctx),
            View::Detail => show_detail(self, ctx),
            View::Diff => show_diff(self, ctx),
        }
    }
}

fn sev_color(s: Severity) -> egui::Color32 {
    match s {
        Severity::Low => egui::Color32::GRAY,
        Severity::Medium => egui::Color32::from_rgb(220, 180, 80),
        Severity::High => egui::Color32::from_rgb(230, 120, 80),
        Severity::Critical => egui::Color32::from_rgb(240, 70, 70),
    }
}

fn show_overview(app: &mut RiftApp, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading(format!("{} → {}", app.cs.base_ref, app.cs.head_ref));
        ui.label(format!(
            "{} files changed · +{} −{} · {} meaningful · {} mechanical collapsed",
            app.cs.stats.files_changed,
            app.cs.stats.added_lines,
            app.cs.stats.deleted_lines,
            app.cs.stats.meaningful_changes,
            app.cs.stats.mechanical_files,
        ));
        if !app.done {
            ui.label(egui::RichText::new(format!(
                "Analyzing… {}/{} files — queue fills in as results land",
                app.analyzed_count(),
                app.cs.files.len()
            )));
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            let vis = app.visible_items();
            for (pos, idx) in vis.iter().enumerate() {
                let r = &app.cs.review_items[*idx];
                ui.horizontal(|ui| {
                    ui.colored_label(sev_color(r.severity), format!("{:?}", r.severity));
                    let label = format!("{}. {}", pos + 1, r.title);
                    if ui.selectable_label(app.selected == pos, label).clicked()
                        || ui.button("inspect ⏎").clicked()
                    {
                        app.selected = pos;
                        app.view = View::Detail;
                    }
                });
                if !r.why.is_empty() {
                    ui.label(egui::RichText::new(&r.why).small().weak());
                }
            }
            if vis.is_empty() {
                ui.label("No changes match. Working tree is clean or everything is filtered.");
            }
        });
    });
}

fn show_queue(app: &mut RiftApp, ctx: &egui::Context) {
    egui::SidePanel::left("queue")
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.heading("Review queue");
            egui::ScrollArea::vertical().show(ui, |ui| {
                let vis = app.visible_items();
                for (pos, idx) in vis.iter().enumerate() {
                    let r = &app.cs.review_items[*idx];
                    let resp = ui.selectable_label(
                        app.selected == pos,
                        format!("{}. [{}] {}", pos + 1, sev_text(r.severity), r.title),
                    );
                    if resp.clicked() {
                        app.selected = pos;
                        app.view = View::Detail;
                    }
                    if app.selected == pos {
                        resp.scroll_to_me(Some(egui::Align::Center));
                    }
                }
            });
        });
    egui::CentralPanel::default().show(ctx, |ui| {
        show_item_detail(app, ui);
    });
}

fn sev_text(s: Severity) -> &'static str {
    match s {
        Severity::Low => "LOW",
        Severity::Medium => "MED",
        Severity::High => "HIGH",
        Severity::Critical => "CRIT",
    }
}

fn show_detail(app: &mut RiftApp, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            show_item_detail(app, ui);
        });
    });
}

fn show_item_detail(app: &RiftApp, ui: &mut egui::Ui) {
    let Some(idx) = app.selected_item() else {
        ui.label("Nothing selected.");
        return;
    };
    let r = &app.cs.review_items[idx];
    ui.horizontal(|ui| {
        ui.colored_label(sev_color(r.severity), format!("{:?}", r.severity));
        ui.heading(&r.title);
    });
    ui.label(format!(
        "category: {:?} · priority {} · confidence {:.0}% · {} file(s)",
        r.category,
        r.priority,
        r.confidence * 100.0,
        r.files.len()
    ));
    ui.separator();
    ui.strong("Why this matters");
    ui.label(&r.why);
    if !r.symbols.is_empty() {
        ui.separator();
        ui.strong("Symbols");
        for s in &r.symbols {
            ui.monospace(s);
        }
    }
    if !r.evidence.is_empty() {
        ui.separator();
        ui.strong("Evidence");
        for e in &r.evidence {
            ui.horizontal(|ui| {
                ui.label(format!("[{}]", e.kind));
                ui.label(&e.summary);
            });
            if let Some(o) = &e.old_value {
                ui.monospace(format!("- {o}"));
            }
            if let Some(n) = &e.new_value {
                ui.monospace(format!("+ {n}"));
            }
        }
    }
    ui.separator();
    ui.strong("Files");
    for f in &r.files {
        ui.monospace(f);
        if let Some(fc) = app.cs.files.iter().find(|x| x.display_path() == f) {
            render_hunks(ui, fc, false);
        }
    }
}

fn show_files(app: &mut RiftApp, ctx: &egui::Context) {
    egui::SidePanel::left("files")
        .default_width(360.0)
        .show(ctx, |ui| {
            ui.heading("Files");
            // Virtualized: only visible rows get widgets. Fixed row height
            // by construction (exact-size click rect + non-wrapping label).
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
            let n = app.cs.files.len();
            egui::ScrollArea::vertical().show_rows(ui, row_h, n, |ui, range| {
                for i in range {
                    let f = &app.cs.files[i];
                    let mut label = format!(
                        "{:?} {} +{} −{}",
                        f.status,
                        f.display_path(),
                        f.added_lines,
                        f.deleted_lines
                    );
                    if f.is_generated {
                        label.push_str(" [gen]");
                    }
                    if !app.analyzed.get(i).copied().unwrap_or(true) {
                        label.push_str(" …");
                    }
                    let selected = app.file_selected == i;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), row_h),
                        egui::Sense::click(),
                    );
                    if selected {
                        ui.painter()
                            .rect_filled(rect, 4.0, ui.visuals().selection.bg_fill);
                    }
                    ui.put(rect, egui::Label::new(label).truncate());
                    if resp.clicked() {
                        app.file_selected = i;
                    }
                }
            });
        });
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some(f) = app.cs.files.get(app.file_selected) {
                ui.heading(f.display_path());
                ui.label(format!(
                    "{:?} · {:?} · +{} −{}",
                    f.status, f.language, f.added_lines, f.deleted_lines
                ));
                render_hunks(ui, f, true);
            }
        });
    });
}

fn show_diff(app: &RiftApp, ctx: &egui::Context) {
    use egui::RichText;
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Raw diff");
        // Virtualized over flattened rows: 100k-line diffs scroll smoothly
        // because only the visible window is laid out. Uniform row height
        // (monospace, never wrapped — horizontal scroll for long lines).
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        let total = app.diff_rows.len();
        egui::ScrollArea::both().show_rows(ui, row_h, total, |ui, range| {
            // Rows must never wrap: fixed stride assumes uniform height.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            for idx in range {
                match app.diff_rows[idx] {
                    DiffRow::File(fi) => {
                        let f = &app.cs.files[fi];
                        ui.label(
                            RichText::new(format!(
                                "--- {} ({:?}, +{} −{})",
                                f.display_path(),
                                f.status,
                                f.added_lines,
                                f.deleted_lines
                            ))
                            .monospace()
                            .strong(),
                        );
                    }
                    DiffRow::Hunk(fi, hi) => {
                        let h = &app.cs.files[fi].hunks[hi];
                        ui.label(
                            RichText::new(format!("@@ {} @@", h.header))
                                .monospace()
                                .weak(),
                        );
                    }
                    DiffRow::Line(fi, hi, li) => {
                        let l = &app.cs.files[fi].hunks[hi].lines[li];
                        match l.kind {
                            DiffLineKind::Addition => {
                                ui.label(
                                    RichText::new(format!("+{}", l.text))
                                        .monospace()
                                        .color(egui::Color32::from_rgb(140, 220, 140)),
                                );
                            }
                            DiffLineKind::Deletion => {
                                ui.label(
                                    RichText::new(format!("-{}", l.text))
                                        .monospace()
                                        .color(egui::Color32::from_rgb(230, 140, 140)),
                                );
                            }
                            DiffLineKind::Context => {
                                ui.label(RichText::new(format!(" {}", l.text)).monospace().weak());
                            }
                        }
                    }
                }
            }
        });
    });
}

fn render_hunks(ui: &mut egui::Ui, f: &rift_core::FileChange, show_context: bool) {
    use egui::RichText;
    if f.is_binary {
        ui.label("[binary file — no text diff]");
        return;
    }
    for h in &f.hunks {
        ui.label(RichText::new(format!("@@ {} @@", h.header)).small().weak());
        for l in &h.lines {
            match l.kind {
                DiffLineKind::Addition => {
                    ui.label(
                        RichText::new(format!("+{}", l.text))
                            .monospace()
                            .color(egui::Color32::from_rgb(140, 220, 140)),
                    );
                }
                DiffLineKind::Deletion => {
                    ui.label(
                        RichText::new(format!("-{}", l.text))
                            .monospace()
                            .color(egui::Color32::from_rgb(230, 140, 140)),
                    );
                }
                DiffLineKind::Context => {
                    if show_context {
                        ui.label(RichText::new(format!(" {}", l.text)).monospace().weak());
                    }
                }
            }
        }
    }
}

/// Plain-text overview for `--overview` and headless fallback.
pub fn render_text_overview(cs: &ChangeSet) -> String {
    let mut s = String::new();
    s.push_str(&format!("{} → {}\n", cs.base_ref, cs.head_ref));
    s.push_str(&format!(
        "{} files · +{} −{} · {} meaningful · {} mechanical\n\n",
        cs.stats.files_changed,
        cs.stats.added_lines,
        cs.stats.deleted_lines,
        cs.stats.meaningful_changes,
        cs.stats.mechanical_files
    ));
    for (i, r) in cs.sorted_review_items().iter().enumerate() {
        if matches!(r.category, Category::Mechanical) {
            continue;
        }
        s.push_str(&format!(
            "{}. [{:?}] {} (priority {}, {:.0}% confidence)\n",
            i + 1,
            r.severity,
            r.title,
            r.priority,
            r.confidence * 100.0
        ));
        if !r.why.is_empty() {
            s.push_str(&format!("    {}\n", r.why));
        }
    }
    let mech: Vec<_> = cs
        .review_items
        .iter()
        .filter(|r| matches!(r.category, Category::Mechanical))
        .collect();
    if !mech.is_empty() {
        s.push_str(&format!(
            "\nMechanical (collapsed): {} item(s)\n",
            mech.len()
        ));
        for m in mech {
            s.push_str(&format!("  - {}\n", m.title));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{Category, ChangeStats, ReviewItem, Severity};

    fn item(id: &str, title: &str, category: Category) -> ReviewItem {
        ReviewItem {
            id: id.into(),
            title: title.into(),
            category,
            severity: Severity::Medium,
            priority: 50,
            confidence: 0.8,
            files: vec!["a.rs".into()],
            symbols: vec![],
            evidence: vec![],
            why: "because".into(),
        }
    }

    fn cs_with(items: Vec<ReviewItem>) -> ChangeSet {
        ChangeSet {
            repo_root: ".".into(),
            base_ref: "HEAD".into(),
            head_ref: "worktree".into(),
            files: vec![],
            symbol_changes: vec![],
            review_items: items,
            stats: ChangeStats {
                files_changed: 2,
                added_lines: 10,
                deleted_lines: 4,
                meaningful_changes: 1,
                mechanical_files: 1,
                ..Default::default()
            },
        }
    }

    #[test]
    fn mechanical_hidden_by_default() {
        let cs = cs_with(vec![
            item("a", "auth timeout", Category::Auth),
            item("m", "lockfile", Category::Mechanical),
        ]);
        let app = RiftApp::new(cs);
        assert_eq!(app.visible_items(), vec![0]);
    }

    #[test]
    fn mechanical_toggle_reveals() {
        let cs = cs_with(vec![
            item("a", "auth timeout", Category::Auth),
            item("m", "lockfile", Category::Mechanical),
        ]);
        let mut app = RiftApp::new(cs);
        app.show_mechanical = true;
        assert_eq!(app.visible_items().len(), 2);
    }

    #[test]
    fn search_filters_title_and_path() {
        let mut real = item("a", "auth timeout", Category::Auth);
        real.files = vec!["src/session.rs".into()];
        let cs = cs_with(vec![real, item("b", "readme tweak", Category::Docs)]);
        let mut app = RiftApp::new(cs);
        app.search = "session".into();
        assert_eq!(app.visible_items(), vec![0]);
        app.search = "nothing-matches-xyz".into();
        assert!(app.visible_items().is_empty());
    }

    #[test]
    fn text_overview_separates_mechanical() {
        let cs = cs_with(vec![
            item("a", "auth timeout", Category::Auth),
            item("m", "lockfile", Category::Mechanical),
        ]);
        let out = render_text_overview(&cs);
        assert!(out.contains("auth timeout"), "{out}");
        assert!(out.contains("Mechanical (collapsed)"), "{out}");
        assert!(out.contains("2 files"), "{out}");
    }

    fn file_with_hunks(path: &str, hunks: usize, lines: usize) -> FileChange {
        use rift_core::{DiffLine, DiffLineKind, FileStatus, Hunk, Language};
        FileChange {
            old_path: String::new(),
            new_path: path.into(),
            status: FileStatus::Modified,
            language: Language::Rust,
            is_binary: false,
            is_generated: false,
            is_test_file: false,
            added_lines: lines,
            deleted_lines: 0,
            old_content: None,
            new_content: None,
            hunks: (0..hunks)
                .map(|h| Hunk {
                    old_start: 1,
                    old_lines: lines as u32,
                    new_start: 1,
                    new_lines: lines as u32,
                    header: format!("{h}"),
                    lines: (0..lines)
                        .map(|_| DiffLine {
                            kind: DiffLineKind::Context,
                            text: "x".into(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn flatten_counts_rows() {
        // 2 files × (1 file row + 2 hunks × (1 + 3 lines)) = 2 + 2*2*(1+3) = 18.
        let files = vec![file_with_hunks("a.rs", 2, 3), file_with_hunks("b.rs", 2, 3)];
        let rows = flatten_diff_rows(&files);
        assert_eq!(rows.len(), 18);
        assert!(matches!(rows[0], DiffRow::File(0)));
        assert!(matches!(rows[1], DiffRow::Hunk(0, 0)));
        assert!(matches!(rows[2], DiffRow::Line(0, 0, 0)));
    }

    #[test]
    fn pending_streams_to_finished() {
        use rift_core::ChangeStats;
        let (tx, rx) = mpsc::channel();
        let files = vec![file_with_hunks("a.rs", 1, 1)];
        let mut app = RiftApp::pending(".".into(), "HEAD".into(), "worktree".into(), files, rx);
        assert!(!app.done);
        assert_eq!(app.analyzed_count(), 0);

        tx.send(UiMsg::Progress { index: 0 }).unwrap();
        assert!(app.drain());
        assert_eq!(app.analyzed_count(), 1);
        assert!(!app.done);

        tx.send(UiMsg::Finished {
            items: vec![item("a", "auth timeout", Category::Auth)],
            stats: ChangeStats {
                files_changed: 1,
                ..Default::default()
            },
            syms: vec![],
        })
        .unwrap();
        assert!(app.drain());
        assert!(app.done);
        assert_eq!(app.visible_items(), vec![0]);
        assert_eq!(app.analyzed_count(), 1);
    }
}
