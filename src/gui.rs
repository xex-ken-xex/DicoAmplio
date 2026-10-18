//! 画面。見た目は vw と同じ `vw-ui`(配色・メニュー・パネル見出し)で、
//! パネルの並びも vw に合わせる: メニュー → ログ(下)→ 設定(左)→ プレビュー(右)→ タグの規則(中央)。

use crate::charset::Charset;
use crate::config::{self, Config, CsvEncoding, Level, RuleSpec};
use crate::engine::{vars_for, Engine, Idx, PreviewRow, Progress, DEFAULT_RULES};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText, ThemePreference};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

/// 「タグを選んで足す」に出す、そのレベルの典型的なタグ。辞書にあるどの名前でも書ける。
fn typical_tags(level: Level) -> &'static [&'static str] {
    match level {
        Level::Patient => &["PatientName", "PatientID", "PatientSex", "PatientBirthDate", "PatientAge", "PatientWeight", "PatientSize", "OtherPatientIDs", "PatientComments"],
        Level::Study => &["StudyDescription", "StudyDate", "StudyTime", "StudyID", "AccessionNumber", "ReferringPhysicianName", "InstitutionName", "PatientAge", "PatientWeight", "PatientSize"],
        Level::Series => &["SeriesDescription", "SeriesNumber", "SeriesDate", "SeriesTime", "Modality", "BodyPartExamined", "ProtocolName", "OperatorsName", "StationName", "Manufacturer"],
        Level::Image => &["InstanceNumber", "ImageComments", "AcquisitionDate", "AcquisitionTime", "ContentDate", "ContentTime"],
    }
}

const WARN: Color32 = Color32::from_rgb(0xe0, 0xa0, 0x40);
const ERROR: Color32 = Color32::from_rgb(0xe0, 0x60, 0x60);

pub fn run(dir: PathBuf, initial: Option<Config>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("DicoAmplio").with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DicoAmplio",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, dir, initial)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

struct Job {
    progress: Arc<Progress>,
    done: Receiver<Result<u64, String>>,
    logged: usize,
}

struct App {
    cfg: Config,
    preset_dir: PathBuf,
    presets: Vec<String>,
    current_preset: Option<String>,
    save_dialog: Option<String>,
    theme: ThemePreference,

    engine: Result<Engine, String>,
    engine_cfg: Config,
    sources: Option<usize>,
    counted_input: String,
    sample: Idx,
    /// CSV の欄を開いている規則(cfg.rules の番号)
    csv_open: HashSet<usize>,

    job: Option<Job>,
    log: Vec<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, preset_dir: PathBuf, initial: Option<Config>) -> Self {
        crate::fonts::load_cjk_in_background(&cc.egui_ctx);
        let theme = ThemePreference::System;
        vw_ui::theme::apply(&cc.egui_ctx, theme, true, 1.0);
        let last = config::preset_path(&preset_dir, config::LAST_NAME).ok().and_then(|p| Config::load(&p).ok());
        let cfg = initial.or(last).unwrap_or_default();
        let mut app = App {
            engine: Engine::new(&cfg).map_err(|e| format!("{e:#}")),
            engine_cfg: cfg.clone(),
            cfg,
            presets: config::list_presets(&preset_dir),
            preset_dir,
            current_preset: None,
            save_dialog: None,
            theme,
            sources: None,
            counted_input: String::new(),
            sample: Idx { p: 0, s: 0, r: 0, i: 0 },
            csv_open: HashSet::new(),
            job: None,
            log: vec!["起動した".into()],
        };
        app.count_sources();
        app
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.log.push(msg.into());
    }

    fn refresh_engine(&mut self) {
        if self.cfg != self.engine_cfg {
            self.engine = Engine::new(&self.cfg).map_err(|e| format!("{e:#}"));
            self.engine_cfg = self.cfg.clone();
        }
    }

    /// 入力欄を打っている間に歩き回らないよう、確定したときにだけ数える。
    fn count_sources(&mut self) {
        self.counted_input = self.cfg.input.clone();
        self.sources = self.engine.as_ref().ok().and_then(|e| e.sources().ok()).map(|s| s.len());
    }

    fn save_last(&self) {
        if let Ok(p) = config::preset_path(&self.preset_dir, config::LAST_NAME) {
            let _ = self.cfg.save(&p);
        }
    }

    fn load_preset(&mut self, name: &str) {
        let result = config::preset_path(&self.preset_dir, name).and_then(|p| Config::load(&p));
        match result {
            Ok(c) => {
                self.cfg = c;
                self.current_preset = Some(name.to_string());
                self.refresh_engine();
                self.count_sources();
                self.note(format!("プリセット「{name}」を読んだ"));
            }
            Err(e) => self.note(format!("エラー: {e:#}")),
        }
    }

    fn save_preset(&mut self, name: &str) {
        let result = config::preset_path(&self.preset_dir, name).and_then(|p| self.cfg.save(&p).map(|_| p));
        match result {
            Ok(p) => {
                self.current_preset = Some(name.trim().to_string());
                self.presets = config::list_presets(&self.preset_dir);
                self.note(format!("プリセットを保存した: {}", p.display()));
            }
            Err(e) => self.note(format!("エラー: {e:#}")),
        }
    }

    fn delete_preset(&mut self, name: &str) {
        let result = config::preset_path(&self.preset_dir, name).and_then(|p| Ok(std::fs::remove_file(p)?));
        match result {
            Ok(()) => {
                if self.current_preset.as_deref() == Some(name) {
                    self.current_preset = None;
                }
                self.presets = config::list_presets(&self.preset_dir);
                self.note(format!("プリセット「{name}」を消した"));
            }
            Err(e) => self.note(format!("エラー: {e:#}")),
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        self.refresh_engine();
        let engine = match Engine::new(&self.cfg) {
            Ok(e) => e,
            Err(e) => {
                self.note(format!("エラー: {e:#}"));
                return;
            }
        };
        self.save_last();
        let progress = Arc::new(Progress::default());
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = engine.run(&p).map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.note("実行を始めた");
        self.job = Some(Job { progress, done: rx, logged: 0 });
    }

    /// 走っている実行の進み具合とログを取り込む。終わっていれば片づける。
    fn poll_job(&mut self) {
        let Some(job) = &mut self.job else { return };
        let lines: Vec<String> = job.progress.log.lock().unwrap()[job.logged..].to_vec();
        job.logged += lines.len();
        let finished = job.done.try_recv().ok();
        self.log.extend(lines);
        if let Some(r) = finished {
            match r {
                Ok(n) => self.log.push(format!("書き出し {n} 個")),
                Err(e) => self.log.push(format!("エラー: {e}")),
            }
            self.job = None;
        }
    }

    fn browse_dir(current: &str) -> Option<String> {
        let mut d = rfd::FileDialog::new();
        if Path::new(current).is_dir() {
            d = d.set_directory(current);
        }
        d.pick_folder().map(|p| p.display().to_string())
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut load = None;
        let mut delete = None;
        let mut start = false;
        vw_ui::menu::bar(ui, |ui| {
            vw_ui::menu::bar_menu(ui, "ファイル", |ui| {
                if ui.add(vw_ui::menu::item(false, "設定ファイルを開く…", "")).clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new().add_filter("設定", &["toml"]).pick_file() {
                        match Config::load(&p) {
                            Ok(c) => {
                                self.cfg = c;
                                self.current_preset = None;
                                self.note(format!("設定を読んだ: {}", p.display()));
                            }
                            Err(e) => self.note(format!("エラー: {e:#}")),
                        }
                    }
                }
                if ui.add(vw_ui::menu::item(false, "設定ファイルに保存…", "")).clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new().add_filter("設定", &["toml"]).set_file_name("preset.toml").save_file() {
                        match self.cfg.save(&p) {
                            Ok(()) => self.note(format!("設定を保存した: {}", p.display())),
                            Err(e) => self.note(format!("エラー: {e:#}")),
                        }
                    }
                }
                ui.separator();
                if ui.add(vw_ui::menu::item(false, "終了", "")).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            vw_ui::menu::bar_menu(ui, "プリセット", |ui| {
                if self.presets.is_empty() {
                    ui.add_enabled(false, egui::Button::new("(保存したプリセットはない)"));
                }
                for name in &self.presets {
                    let current = self.current_preset.as_deref() == Some(name.as_str());
                    if ui.add(vw_ui::menu::item(current, name.as_str(), "")).clicked() {
                        load = Some(name.clone());
                        ui.close();
                    }
                }
                ui.separator();
                if ui.add(vw_ui::menu::item(false, "いまの設定を保存…", "")).clicked() {
                    self.save_dialog = Some(self.current_preset.clone().unwrap_or_default());
                    ui.close();
                }
                if !self.presets.is_empty() {
                    vw_ui::menu::submenu(ui, "消す", |ui| {
                        for name in &self.presets {
                            if ui.add(vw_ui::menu::item(false, name.as_str(), "")).clicked() {
                                delete = Some(name.clone());
                                ui.close();
                            }
                        }
                    });
                }
                ui.separator();
                ui.weak(RichText::new(self.preset_dir.display().to_string()).small().monospace());
            });
            vw_ui::menu::bar_menu(ui, "実行", |ui| {
                let idle = self.job.is_none();
                if ui.add_enabled(idle, vw_ui::menu::item(false, "水増しを実行", "Ctrl+R")).clicked() {
                    start = true;
                    ui.close();
                }
                let cancel = vw_ui::menu::item(false, "中断", "");
                if ui.add_enabled(!idle, cancel).clicked() {
                    if let Some(j) = &self.job {
                        j.progress.cancel.store(true, Ordering::Relaxed);
                    }
                    ui.close();
                }
            });
            vw_ui::menu::bar_menu(ui, "表示", |ui| {
                for (pref, label) in [
                    (ThemePreference::System, "テーマ: システムに合わせる"),
                    (ThemePreference::Dark, "テーマ: ダーク"),
                    (ThemePreference::Light, "テーマ: ライト"),
                ] {
                    if ui.add(vw_ui::menu::item(self.theme == pref, label, "")).clicked() {
                        self.theme = pref;
                        vw_ui::theme::apply(&ctx, pref, true, 1.0);
                        ui.close();
                    }
                }
            });
        });
        if let Some(n) = load {
            self.load_preset(&n);
        }
        if let Some(n) = delete {
            self.delete_preset(&n);
        }
        if start || ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::R)) {
            self.start(&ctx);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        vw_ui::panel::header(ui, "設定");
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.strong("入出力");
            let mut recount = false;
            for (label, is_input) in [("元のディレクトリ", true), ("出力先", false)] {
                ui.label(label);
                ui.horizontal(|ui| {
                    let field = if is_input { &mut self.cfg.input } else { &mut self.cfg.output };
                    let browse_w = 48.0;
                    let r = ui.add(egui::TextEdit::singleline(field).desired_width(ui.available_width() - browse_w));
                    let mut browsed = false;
                    if ui.button("参照").clicked() {
                        if let Some(p) = Self::browse_dir(field) {
                            *field = p;
                            browsed = true;
                        }
                    }
                    recount |= is_input && (r.lost_focus() || browsed);
                });
            }
            if recount || self.counted_input != self.cfg.input && !ui.memory(|m| m.focused().is_some()) {
                self.refresh_engine();
                self.count_sources();
            }

            ui.add_space(6.0);
            ui.strong("件数(元を 1 組とした倍数)");
            egui::Grid::new("counts").num_columns(2).show(ui, |ui| {
                for (label, v) in [
                    ("患者", &mut self.cfg.patients),
                    ("患者あたりの検査", &mut self.cfg.studies),
                    ("検査あたりのシリーズ", &mut self.cfg.series),
                    ("シリーズあたりの画像", &mut self.cfg.images),
                ] {
                    ui.label(label);
                    ui.add(egui::DragValue::new(v).range(1..=1_000_000).speed(0.2));
                    ui.end_row();
                }
            });
            let c = &self.cfg;
            match self.sources {
                Some(n) => {
                    let total = n as u64 * c.patients as u64 * c.studies as u64 * c.series as u64 * c.images as u64;
                    ui.label(format!("元 {n} 個 → 出力 {total} 個"));
                }
                None => {
                    ui.weak("元のディレクトリを指定すると出力数が出る");
                }
            }

            ui.add_space(6.0);
            ui.strong("文字コード(日本語)");
            vw_ui::menu::combo("charset")
                .selected_text(charset_label(self.cfg.charset))
                .width(ui.available_width() - 8.0)
                .show_ui(ui, |ui| {
                    for cs in [Charset::Utf8, Charset::Iso2022jp, Charset::Keep] {
                        ui.selectable_value(&mut self.cfg.charset, cs, charset_label(cs));
                    }
                });
            ui.weak(charset_note(self.cfg.charset));

            ui.add_space(6.0);
            ui.strong("CSV の差し込み");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.csv).hint_text("(使わない)").desired_width(ui.available_width() - 100.0));
                if ui.button("参照").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt"]).pick_file() {
                        self.cfg.csv = p.display().to_string();
                    }
                }
                if ui.button("消す").clicked() {
                    self.cfg.csv.clear();
                }
            });
            egui::Grid::new("csv").num_columns(2).show(ui, |ui| {
                ui.label("CSV の文字コード");
                vw_ui::menu::combo("csv_enc").selected_text(self.cfg.csv_encoding.label()).show_ui(ui, |ui| {
                    for e in CsvEncoding::ALL {
                        ui.selectable_value(&mut self.cfg.csv_encoding, e, e.label());
                    }
                });
                ui.end_row();
            });
            if let Ok(e) = &self.engine {
                if !e.csv_columns.is_empty() {
                    ui.label(format!("{} 行 / 列: {}", e.csv_rows, e.csv_columns.join(", ")));
                }
            }

            ui.add_space(6.0);
            ui.strong("UID の接頭辞");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.uid_root).desired_width(ui.available_width() - 8.0));
        });
    }

    fn rules_ui(&mut self, ui: &mut egui::Ui) {
        vw_ui::panel::header(ui, "タグの規則");
        ui.add_space(2.0);
        ui.weak("元の 1 組を n 倍にするとき、タグの値がどの単位で変わるかで 4 つに分ける。CSV の行も、そのレベルの単位で進む。");
        ui.weak("値は printf 風: EVAL%(p1)05d、%(weight).1f、%(name)-12s。同じタグを複数書いたときは下のものが勝つ。");
        ui.add_space(4.0);

        let mut remove = None;
        let mut add: Option<RuleSpec> = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for level in Level::ALL {
                ui.add_space(6.0);
                ui.strong(format!("{}レベルのタグ({}ごとに 1 つの値)", level.label(), level.label()));
                let mut vars = vars_for(level).join(" ");
                if let Ok(e) = &self.engine {
                    if !e.csv_columns.is_empty() {
                        vars += &format!("  + 全体の CSV の列: {}", e.csv_columns.join(" "));
                    }
                }
                ui.weak(RichText::new(format!("使える変数: {vars}")).small());

                // 既定の規則。同じタグを足すと上書きされるので、ここに見えているだけにする
                for (tag, l, value) in DEFAULT_RULES {
                    let overridden = self.cfg.rules.iter().any(|r| r.tag.trim().eq_ignore_ascii_case(tag));
                    if *l == level && !overridden {
                        ui.weak(RichText::new(format!("既定  {tag} = {value}")).small().monospace());
                    }
                }

                for i in 0..self.cfg.rules.len() {
                    if self.cfg.rules[i].level != level {
                        continue;
                    }
                    self.rule_ui(ui, i, &mut remove);
                }
                ui.horizontal(|ui| {
                    if ui.button("＋ 規則を足す").clicked() {
                        add = Some(RuleSpec::new("", level, ""));
                    }
                    vw_ui::menu::button(ui, "タグを選んで足す ▼", |ui| {
                        for name in typical_tags(level) {
                            if ui.button(*name).clicked() {
                                add = Some(RuleSpec::new(name, level, ""));
                                ui.close();
                            }
                        }
                    });
                });
            }
            if let Err(e) = &self.engine {
                ui.add_space(8.0);
                ui.colored_label(ERROR, e);
            }
        });
        if let Some(i) = remove {
            self.cfg.rules.remove(i);
            self.csv_open.clear();
        }
        if let Some(r) = add {
            self.cfg.rules.push(r);
        }
    }

    /// 規則 1 本。タグ・値・CSV の開閉・削除の行と、開いていれば、そのタグ専用の CSV の行。
    fn rule_ui(&mut self, ui: &mut egui::Ui, i: usize, remove: &mut Option<usize>) {
        let open = self.csv_open.contains(&i) || !self.cfg.rules[i].csv.is_empty();
        let mut toggle = false;
        ui.horizontal(|ui| {
            let r = &mut self.cfg.rules[i];
            ui.add(egui::TextEdit::singleline(&mut r.tag).hint_text("PatientName").desired_width(190.0));
            let value_width = (ui.available_width() - 90.0).max(160.0);
            ui.add(egui::TextEdit::singleline(&mut r.value).hint_text("%(name)s").desired_width(value_width));
            toggle = ui.selectable_label(!r.csv.is_empty(), "CSV").on_hover_text("このタグだけが読む CSV を指定する").clicked();
            if ui.button("×").on_hover_text("この規則を消す").clicked() {
                *remove = Some(i);
            }
        });
        if toggle {
            if !self.csv_open.insert(i) {
                self.csv_open.remove(&i);
            }
        }
        if !open {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(24.0);
            let r = &mut self.cfg.rules[i];
            ui.label("CSV");
            ui.add(egui::TextEdit::singleline(&mut r.csv).hint_text("(全体の CSV を使う)").desired_width((ui.available_width() - 250.0).max(120.0)));
            if ui.button("参照").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt"]).pick_file() {
                    r.csv = p.display().to_string();
                }
            }
            if ui.button("消す").clicked() {
                r.csv.clear();
            }
            let label = r.csv_encoding.map(CsvEncoding::label).unwrap_or("文字コード: 全体に従う");
            vw_ui::menu::combo(("rule_csv_enc", i)).selected_text(label).show_ui(ui, |ui| {
                ui.selectable_value(&mut r.csv_encoding, None, "全体に従う");
                for e in CsvEncoding::ALL {
                    ui.selectable_value(&mut r.csv_encoding, Some(e), e.label());
                }
            });
        });
        let r = &self.cfg.rules[i];
        if !r.csv.is_empty() {
            let enc = r.csv_encoding.unwrap_or(self.cfg.csv_encoding);
            if let Some((cols, rows)) = self.engine.as_ref().ok().and_then(|e| e.table_info(&r.csv, enc)) {
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    ui.weak(RichText::new(format!("{rows} 行 / 列: {}", cols.join(" "))).small());
                });
            }
        }
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        vw_ui::panel::header(ui, "プレビュー");
        let c = &self.cfg;
        self.sample.p = self.sample.p.min(c.patients - 1);
        self.sample.s = self.sample.s.min(c.studies - 1);
        self.sample.r = self.sample.r.min(c.series - 1);
        self.sample.i = self.sample.i.min(c.images - 1);
        ui.horizontal(|ui| {
            for (label, v, max) in [
                ("患者", &mut self.sample.p, c.patients),
                ("検査", &mut self.sample.s, c.studies),
                ("シリーズ", &mut self.sample.r, c.series),
                ("画像", &mut self.sample.i, c.images),
            ] {
                ui.label(label);
                ui.add(egui::DragValue::new(v).range(0..=max - 1).speed(0.2));
            }
        });
        ui.weak("何番目の複製に当たるか(0 始まり)の見本。元ファイルは読まない。");
        ui.add_space(4.0);
        let rows: Vec<PreviewRow> = self.engine.as_ref().map(|e| e.preview(self.sample)).unwrap_or_default();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for r in &rows {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&r.name).strong());
                    ui.weak(format!("{} {} / {}レベル", r.tag, r.vr, r.level.label()));
                });
                match &r.value {
                    Ok(v) => {
                        ui.label(RichText::new(v).monospace());
                    }
                    Err(e) => {
                        ui.colored_label(ERROR, e);
                    }
                }
                if let Some(b) = &r.encoded {
                    let hex: String = b.iter().map(|x| format!("{x:02X} ")).collect();
                    ui.weak(RichText::new(hex).small().monospace());
                }
                for w in &r.warnings {
                    ui.colored_label(WARN, format!("⚠ {w}"));
                }
                ui.add_space(4.0);
            }
        });
    }

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        let mut cancel = false;
        let mut start = false;
        ui.horizontal(|ui| {
            vw_ui::panel::header(ui, "ログ");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &self.job {
                Some(j) => {
                    cancel = ui.button("中断").clicked();
                    let total = j.progress.total.load(Ordering::Relaxed);
                    let done = j.progress.done.load(Ordering::Relaxed);
                    let f = if total == 0 { 0.0 } else { done as f32 / total as f32 };
                    ui.add(egui::ProgressBar::new(f).desired_width(240.0).text(format!("{done} / {total}")));
                }
                None => start = ui.button("水増しを実行").clicked(),
            });
        });
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
            for l in &self.log {
                ui.label(RichText::new(l).monospace());
            }
        });
        if cancel {
            if let Some(j) = &self.job {
                j.progress.cancel.store(true, Ordering::Relaxed);
            }
        }
        if start {
            let ctx = ui.ctx().clone();
            self.start(&ctx);
        }
    }

    fn save_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(name) = &mut self.save_dialog else { return };
        let mut action = None;
        egui::Window::new("プリセットを保存").collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label("名前(同じ名前は上書き)");
            let r = ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
            r.request_focus();
            let ok = !name.trim().is_empty();
            ui.horizontal(|ui| {
                if ui.add_enabled(ok, egui::Button::new("保存")).clicked() || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    action = Some(true);
                }
                if ui.button("やめる").clicked() {
                    action = Some(false);
                }
            });
        });
        match action {
            Some(true) => {
                let n = self.save_dialog.take().unwrap_or_default();
                self.save_preset(&n);
            }
            Some(false) => self.save_dialog = None,
            None => {}
        }
    }
}

fn charset_label(cs: Charset) -> &'static str {
    match cs {
        Charset::Utf8 => "UTF-8(ISO_IR 192)",
        Charset::Iso2022jp => "ISO 2022 IR 13 / IR 87(JIS)",
        Charset::Keep => "触れない(元ファイルのまま)",
    }
}

fn charset_note(cs: Charset) -> &'static str {
    match cs {
        Charset::Utf8 => "(0008,0005) = ISO_IR 192。新しい装置・ビューアなら読める。",
        Charset::Iso2022jp => "(0008,0005) = ISO 2022 IR 13\\ISO 2022 IR 87。JAHIS など日本の運用向け。JIS X 0208 外の字は書けない。",
        Charset::Keep => "元ファイルが日本語を表せる文字コードのときだけ使う。",
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_last();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job();
        self.refresh_engine();
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // メニューは窓の地の色、パネルはその一段上にして、枠・パネルが線に頼らず見分けられるようにする(vw と同じ)
        let chrome = ui.visuals().extreme_bg_color;
        egui::Panel::top("menu")
            .frame(egui::Frame::side_top_panel(ui.style()).fill(chrome))
            .show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("log").resizable(true).default_size(170.0).show(ui, |ui| self.log_ui(ui));
        egui::Panel::left("settings").resizable(true).default_size(330.0).show(ui, |ui| self.settings_ui(ui));
        egui::Panel::right("preview").resizable(true).default_size(380.0).show(ui, |ui| self.preview_ui(ui));
        egui::CentralPanel::default().show(ui, |ui| self.rules_ui(ui));
        self.save_dialog_ui(&ctx);
    }
}
