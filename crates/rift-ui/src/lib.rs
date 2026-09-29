//! Rift graphical review surface (eframe/egui).
//! Minimal, dense, keyboard-first: Overview / Queue / Files / Diff.

use rift_core::{Category, ChangeSet, DiffLineKind, Severity};

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
}

impl RiftApp {
    pub fn new(cs: ChangeSet) -> Self {
        Self {
            cs,
            view: View::Overview,
            selected: 0,
            file_selected: 0,
            search: String::new(),
            show_mechanical: false,
        }
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

impl eframe::App for RiftApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, f) in app.cs.files.iter().enumerate() {
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
                    if ui.selectable_label(app.file_selected == i, label).clicked() {
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
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Raw diff");
        egui::ScrollArea::vertical().show(ui, |ui| {
            for f in &app.cs.files {
                ui.separator();
                ui.monospace(format!(
                    "--- {} ({:?}, +{} −{})",
                    f.display_path(),
                    f.status,
                    f.added_lines,
                    f.deleted_lines
                ));
                render_hunks(ui, f, true);
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
