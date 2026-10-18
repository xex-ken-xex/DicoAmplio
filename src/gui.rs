//! 画面。見た目は vw と同じ `vw-ui`(配色・メニュー・パネル見出し)で、
//! パネルの並びも vw に合わせる: メニュー → 道具の帯 → ログ(下)→ 設定(左)→ プレビュー(右)→ タグの規則(中央)。

use crate::charset::Charset;
use crate::config::{self, Config, CsvEncoding, Level, RuleSpec};
use crate::engine::{tag_of, vars_for, Engine, Idx, PreviewRow, Progress, TableCache, DEFAULT_RULES};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText, ThemePreference};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
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

/// レベルの説明(規則の節の見出しの下)。
fn level_note(level: Level) -> &'static str {
    match level {
        Level::Patient => "患者ごとに 1 つの値。同じ患者の全画像で同じになる",
        Level::Study => "検査ごとに 1 つの値。検査日・検査の説明・受付番号など",
        Level::Series => "シリーズごとに 1 つの値。シリーズの説明・部位・プロトコルなど",
        Level::Image => "画像ごとに 1 つの値。画像番号など",
    }
}

const WARN: Color32 = Color32::from_rgb(0xe0, 0xa0, 0x40);
const ERROR: Color32 = Color32::from_rgb(0xe0, 0x60, 0x60);
const OK: Color32 = Color32::from_rgb(0x6a, 0xc0, 0x7a);

/// 規則の行のタグの欄の幅。既定の規則の行とそろえるため、1 か所で決める。
const TAG_W: f32 = 200.0;

pub fn run(dir: PathBuf, initial: Option<Config>, preset: Option<String>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("DicoAmplio").with_inner_size([1360.0, 860.0]).with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DicoAmplio",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, dir, initial, preset)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

struct Job {
    progress: Arc<Progress>,
    done: Receiver<Result<u64, String>>,
    logged: usize,
    started: std::time::Instant,
}

/// 保存の確認。同じ名前があるときは、もう一度押してもらう。
struct SaveDialog {
    name: String,
}

struct App {
    cfg: Config,
    preset_dir: PathBuf,
    presets: Vec<String>,
    current_preset: Option<String>,
    save_dialog: Option<SaveDialog>,
    theme: ThemePreference,

    /// Arc なのは、実行のスレッドに渡すときに作り直さずに済ませるため
    engine: Result<Arc<Engine>, String>,
    /// engine を作ったときの設定(入出力先など、engine に関わらない欄は空にしてある)
    engine_cfg: Config,
    table_cache: TableCache,
    sources: Option<usize>,
    counted_input: String,
    sample: Idx,
    /// CSV の欄を開いている規則(cfg.rules の番号)
    csv_open: HashSet<usize>,

    job: Option<Job>,
    log: Vec<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, preset_dir: PathBuf, initial: Option<Config>, preset: Option<String>) -> Self {
        crate::fonts::load_cjk_in_background(&cc.egui_ctx);
        let theme = ThemePreference::System;
        vw_ui::theme::apply(&cc.egui_ctx, theme, true, 1.0);
        let last = config::preset_path(&preset_dir, config::LAST_NAME).ok().and_then(|p| Config::load(&p).ok());
        let mut cfg = initial.or(last).unwrap_or_default();
        for n in [&mut cfg.patients, &mut cfg.studies, &mut cfg.series, &mut cfg.images] {
            *n = (*n).max(1);
        }
        let mut table_cache = TableCache::new();
        let mut app = App {
            engine: Engine::with_cache(&cfg, &mut table_cache).map_err(|e| format!("{e:#}")).map(Arc::new),
            engine_cfg: Self::engine_key(&cfg),
            table_cache,
            cfg,
            presets: config::list_presets(&preset_dir),
            preset_dir,
            current_preset: preset,
            save_dialog: None,
            theme,
            sources: None,
            counted_input: String::new(),
            sample: Idx { p: 0, s: 0, r: 0, i: 0 },
            csv_open: HashSet::new(),
            job: None,
            log: vec!["起動した。元のディレクトリは、窓にフォルダをドロップしても指定できる".into()],
        };
        app.count_sources();
        app
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.log.push(msg.into());
    }

    /// engine の中身を変える欄だけを残した設定。出力先や UID の接頭辞を打つたびに作り直さないため。
    fn engine_key(cfg: &Config) -> Config {
        Config { input: String::new(), output: String::new(), uid_root: String::new(), ..cfg.clone() }
    }

    fn refresh_engine(&mut self) {
        let key = Self::engine_key(&self.cfg);
        if key != self.engine_cfg {
            self.engine = Engine::with_cache(&self.cfg, &mut self.table_cache).map_err(|e| format!("{e:#}")).map(Arc::new);
            self.engine_cfg = key;
        }
    }

    /// 入力欄を打っている間に歩き回らないよう、確定したときにだけ数える。
    fn count_sources(&mut self) {
        self.counted_input = self.cfg.input.clone();
        self.sources = self.engine.as_ref().ok().and_then(|e| e.sources().ok()).map(|s| s.len());
    }

    fn total(&self) -> Option<u64> {
        let c = &self.cfg;
        self.sources.map(|n| n as u64 * c.patients as u64 * c.studies as u64 * c.series as u64 * c.images as u64)
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

    /// 実行の前に分かる問題。実行ボタンを押せなくする理由にもなる。
    fn blocking_problem(&self) -> Option<String> {
        if self.cfg.input.trim().is_empty() {
            return Some("元のディレクトリを指定してほしい".into());
        }
        if !Path::new(self.cfg.input.trim()).is_dir() {
            return Some("元のディレクトリがない".into());
        }
        if self.cfg.output.trim().is_empty() {
            return Some("出力先を指定してほしい".into());
        }
        if Path::new(self.cfg.output.trim()) == Path::new(self.cfg.input.trim()) {
            return Some("出力先が元のディレクトリと同じ".into());
        }
        if let Err(e) = &self.engine {
            return Some(e.clone());
        }
        None
    }

    fn start(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        self.refresh_engine();
        if let Some(p) = self.blocking_problem() {
            self.note(format!("エラー: {p}"));
            return;
        }
        // 元のディレクトリと出力先は engine_key に入れていないので、ここで最新の値で作り直す
        let engine = match Engine::with_cache(&self.cfg, &mut self.table_cache) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                self.note(format!("エラー: {e:#}"));
                return;
            }
        };
        self.engine = Ok(engine.clone());
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
        self.job = Some(Job { progress, done: rx, logged: 0, started: std::time::Instant::now() });
    }

    fn cancel(&self) {
        if let Some(j) = &self.job {
            j.progress.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 走っている実行の進み具合とログを取り込む。終わっていれば片づける。
    fn poll_job(&mut self) {
        let Some(job) = &mut self.job else { return };
        let lines: Vec<String> = job.progress.log.lock().unwrap()[job.logged..].to_vec();
        job.logged += lines.len();
        // スレッドがパニックで消えると送り手も消えるので、Disconnected は「失敗して終わった」として扱う
        let finished = match job.done.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("実行のスレッドが途中で止まった".to_string())),
        };
        let elapsed = job.started.elapsed();
        self.log.extend(lines);
        if let Some(r) = finished {
            match r {
                Ok(n) => self.log.push(format!("書き出し {n} 個({:.1} 秒)", elapsed.as_secs_f64())),
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

    /// 窓にドロップされたフォルダ(ファイルならその親)を、元のディレクトリにする。
    fn take_dropped(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        let Some(p) = dropped.into_iter().next() else { return };
        let dir = if p.is_dir() { p } else { p.parent().map(Path::to_path_buf).unwrap_or(p) };
        self.cfg.input = dir.display().to_string();
        self.refresh_engine();
        self.count_sources();
        self.note(format!("元のディレクトリ: {}", dir.display()));
    }

    // ---- メニュー --------------------------------------------------------------------------

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
                if ui.add(vw_ui::menu::item(false, "いまの設定を保存…", "Ctrl+S")).clicked() {
                    self.save_dialog = Some(SaveDialog { name: self.current_preset.clone().unwrap_or_default() });
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
                if ui.add_enabled(!idle, vw_ui::menu::item(false, "中断", "")).clicked() {
                    self.cancel();
                    ui.close();
                }
                ui.separator();
                if ui.add(vw_ui::menu::item(false, "ログを消す", "")).clicked() {
                    self.log.clear();
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
        let (run_key, save_key) = ctx.input(|i| {
            (i.modifiers.command && i.key_pressed(egui::Key::R), i.modifiers.command && i.key_pressed(egui::Key::S))
        });
        if start || run_key {
            self.start(&ctx);
        }
        if save_key && self.save_dialog.is_none() {
            self.save_dialog = Some(SaveDialog { name: self.current_preset.clone().unwrap_or_default() });
        }
    }

    /// メニューの下の帯。実行のボタンと、いま何が起きるかの一行。
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut start = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            match &self.job {
                Some(j) => {
                    cancel = ui.button("■ 中断").clicked();
                    let total = j.progress.total.load(Ordering::Relaxed);
                    let done = j.progress.done.load(Ordering::Relaxed);
                    let f = if total == 0 { 0.0 } else { done as f32 / total as f32 };
                    ui.add(egui::ProgressBar::new(f).desired_width(260.0).text(format!("{done} / {total}")));
                    let secs = j.started.elapsed().as_secs_f64();
                    if done > 0 && secs > 0.5 {
                        let left = (total.saturating_sub(done)) as f64 / (done as f64 / secs);
                        ui.weak(format!("{:.0} 個/秒、残り {:.0} 秒", done as f64 / secs, left));
                    }
                }
                None => {
                    let problem = self.blocking_problem();
                    let button = egui::Button::new(RichText::new("▶ 水増しを実行").strong());
                    let r = ui.add_enabled(problem.is_none(), button);
                    start = r.clicked();
                    match (problem, self.total()) {
                        (Some(p), _) => {
                            ui.colored_label(WARN, format!("⚠ {p}"));
                        }
                        (None, Some(total)) => {
                            ui.label(format!("元 {} 個 → 出力 {total} 個", self.sources.unwrap_or(0)));
                            ui.weak(format!("→ {}", self.cfg.output.trim()));
                        }
                        (None, None) => {}
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                match &self.current_preset {
                    Some(n) => {
                        ui.label(RichText::new(n).strong());
                        ui.weak("プリセット:");
                    }
                    None => {
                        ui.weak("プリセット: (なし)");
                    }
                }
            });
        });
        if cancel {
            self.cancel();
        }
        if start {
            self.start(&ctx);
        }
    }

    // ---- 設定 ----------------------------------------------------------------------------

    /// 設定の節の見出し。中身の字より少し大きい字と、下の細線で区切る。
    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(10.0);
        ui.label(RichText::new(title).strong());
        ui.add_space(2.0);
        ui.separator();
        ui.add_space(4.0);
    }

    /// パスの欄。ラベル、入力欄(残りの幅いっぱい)と「参照」。入力欄から離れたら true。
    fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, browse: impl FnOnce(&str) -> Option<String>) -> bool {
        ui.label(label);
        let mut committed = false;
        ui.horizontal(|ui| {
            let browse_w = 52.0;
            let r = ui.add(
                egui::TextEdit::singleline(value)
                    .desired_width(ui.available_width() - browse_w)
                    .hint_text("パス"),
            );
            committed |= r.lost_focus();
            if ui.button("参照").clicked() {
                if let Some(p) = browse(value) {
                    *value = p;
                    committed = true;
                }
            }
        });
        committed
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        vw_ui::panel::header(ui, "設定");
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            Self::section(ui, "入出力");
            let mut recount = Self::path_row(ui, "元のディレクトリ(再帰的に探す)", &mut self.cfg.input, Self::browse_dir);
            match self.sources {
                Some(n) => {
                    ui.colored_label(OK, format!("✓ ファイル {n} 個"));
                }
                None if !self.cfg.input.trim().is_empty() => {
                    ui.colored_label(WARN, "⚠ ディレクトリがない");
                }
                None => {
                    ui.weak("フォルダを窓にドロップしても指定できる");
                }
            }
            ui.add_space(6.0);
            Self::path_row(ui, "出力先", &mut self.cfg.output, Self::browse_dir);
            ui.weak("出力は 出力先/P患者/S検査/Rシリーズ/SOPInstanceUID.dcm");
            if recount || self.counted_input != self.cfg.input && !ui.memory(|m| m.focused().is_some()) {
                self.refresh_engine();
                self.count_sources();
                recount = false;
            }
            let _ = recount;

            Self::section(ui, "件数(元の 1 組を何倍にするか)");
            egui::Grid::new("counts").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
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
            let k = c.patients as u64 * c.studies as u64 * c.series as u64 * c.images as u64;
            match self.total() {
                Some(total) => {
                    ui.label(RichText::new(format!("{k} 倍 → 出力 {total} 個")).strong());
                }
                None => {
                    ui.weak(format!("{k} 倍(元のディレクトリを指定すると出力数が出る)"));
                }
            }

            Self::section(ui, "文字コード(日本語)");
            vw_ui::menu::combo("charset")
                .selected_text(charset_label(self.cfg.charset))
                .width(ui.available_width() - 8.0)
                .show_ui(ui, |ui| {
                    for cs in [Charset::Utf8, Charset::Iso2022jp, Charset::Keep] {
                        ui.selectable_value(&mut self.cfg.charset, cs, charset_label(cs));
                    }
                });
            ui.weak(charset_note(self.cfg.charset));

            Self::section(ui, "CSV の差し込み(全体)");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.csv).hint_text("(使わない)").desired_width(ui.available_width() - 104.0));
                if ui.button("参照").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt"]).pick_file() {
                        self.cfg.csv = p.display().to_string();
                    }
                }
                if ui.button("消す").clicked() {
                    self.cfg.csv.clear();
                }
            });
            ui.horizontal(|ui| {
                ui.label("文字コード");
                vw_ui::menu::combo("csv_enc").selected_text(self.cfg.csv_encoding.label()).show_ui(ui, |ui| {
                    for e in CsvEncoding::ALL {
                        ui.selectable_value(&mut self.cfg.csv_encoding, e, e.label());
                    }
                });
            });
            match &self.engine {
                Ok(e) if !e.csv_columns.is_empty() => {
                    ui.colored_label(OK, format!("✓ {} 行", e.csv_rows));
                    ui.horizontal_wrapped(|ui| {
                        ui.weak("列:");
                        for c in &e.csv_columns {
                            ui.label(RichText::new(c).monospace());
                        }
                    });
                }
                Err(e) if !self.cfg.csv.trim().is_empty() && e.contains("CSV") => {
                    ui.colored_label(ERROR, format!("✗ {e}"));
                }
                _ => {
                    ui.weak("1 行目が列名。列名がそのまま変数になり、タグのレベルの単位で 1 行ずつ進む");
                }
            }

            Self::section(ui, "UID の接頭辞");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.uid_root).desired_width(ui.available_width() - 8.0));
            ui.weak("UID は 接頭辞.<元の UID と複製番号から決まる数> になる");
            ui.add_space(8.0);
        });
    }

    // ---- タグの規則 ----------------------------------------------------------------------

    fn rules_ui(&mut self, ui: &mut egui::Ui) {
        vw_ui::panel::header(ui, "タグの規則");
        ui.add_space(2.0);
        ui.weak("値は printf 風(EVAL%(p1)05d、%(weight).1f、%(name)-12s)。タグは辞書の名前か (gggg,eeee)。同じタグは下のものが勝つ。");
        ui.add_space(4.0);

        let mut remove = None;
        let mut add: Option<RuleSpec> = None;
        let csv_columns = self.engine.as_ref().ok().map(|e| e.csv_columns.clone()).unwrap_or_default();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for level in Level::ALL {
                let frame = egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).inner_margin(8.0);
                frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}レベル", level.label())).strong());
                        ui.weak(level_note(level));
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(RichText::new("変数:").small());
                        for v in vars_for(level).iter().copied().chain(csv_columns.iter().map(String::as_str)) {
                            ui.label(RichText::new(v).small().monospace());
                        }
                    });
                    ui.add_space(4.0);

                    // 既定の規則。同じタグを足すと上書きされるので、見えているだけにする
                    for (tag, l, value) in DEFAULT_RULES {
                        // 規則は (0010,0010) や /VR つきでも書けるので、文字ではなくタグで比べる
                        let default_tag = tag_of(tag);
                        let overridden = self.cfg.rules.iter().any(|r| tag_of(&r.tag) == default_tag);
                        if *l == level && !overridden {
                            ui.horizontal(|ui| {
                                // add_sized は中身を中央に置くので、幅だけ決めて左から並べる
                                ui.allocate_ui_with_layout(
                                    egui::vec2(TAG_W, 18.0),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.set_min_width(TAG_W);
                                        ui.add_space(4.0);
                                        ui.label(RichText::new(*tag).weak());
                                    },
                                );
                                ui.add(egui::Label::new(RichText::new(*value).weak().monospace()));
                                ui.weak(RichText::new("既定").small());
                            });
                        }
                    }
                    for i in 0..self.cfg.rules.len() {
                        if self.cfg.rules[i].level == level {
                            self.rule_ui(ui, i, &mut remove);
                        }
                    }
                    ui.add_space(4.0);
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
                });
                ui.add_space(6.0);
            }
            if let Err(e) = &self.engine {
                ui.add_space(4.0);
                ui.colored_label(ERROR, format!("✗ {e}"));
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
            ui.add(egui::TextEdit::singleline(&mut r.tag).hint_text("PatientName").desired_width(TAG_W - 8.0));
            let value_width = (ui.available_width() - 96.0).max(160.0);
            ui.add(egui::TextEdit::singleline(&mut r.value).hint_text("%(name)s").desired_width(value_width).font(egui::TextStyle::Monospace));
            toggle = ui.selectable_label(!r.csv.is_empty(), "CSV").on_hover_text("このタグだけが読む CSV を指定する").clicked();
            if ui.button("×").on_hover_text("この規則を消す").clicked() {
                *remove = Some(i);
            }
        });
        if toggle && !self.csv_open.insert(i) {
            self.csv_open.remove(&i);
        }
        if !open {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(TAG_W);
            let r = &mut self.cfg.rules[i];
            ui.label("CSV");
            ui.add(egui::TextEdit::singleline(&mut r.csv).hint_text("(全体の CSV を使う)").desired_width((ui.available_width() - 260.0).max(120.0)));
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
                    ui.add_space(TAG_W);
                    ui.weak(RichText::new(format!("{rows} 行 / 列: {}", cols.join(" "))).small());
                });
            }
        }
    }

    // ---- プレビュー ----------------------------------------------------------------------

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        vw_ui::panel::header(ui, "プレビュー");
        let c = &self.cfg;
        self.sample.p = self.sample.p.min(c.patients.max(1) - 1);
        self.sample.s = self.sample.s.min(c.studies.max(1) - 1);
        self.sample.r = self.sample.r.min(c.series.max(1) - 1);
        self.sample.i = self.sample.i.min(c.images.max(1) - 1);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            for (label, v, max) in [
                ("患者", &mut self.sample.p, c.patients),
                ("検査", &mut self.sample.s, c.studies),
                ("シリーズ", &mut self.sample.r, c.series),
                ("画像", &mut self.sample.i, c.images),
            ] {
                ui.label(label);
                ui.add(egui::DragValue::new(v).range(0..=max.max(1) - 1).speed(0.2));
            }
        });
        ui.weak("何番目の複製か(0 始まり)。元ファイルは読まず、規則だけを当てた値");
        ui.add_space(4.0);
        let rows: Vec<PreviewRow> = self.engine.as_ref().map(|e| e.preview(self.sample)).unwrap_or_default();
        let field_bg = ui.visuals().extreme_bg_color;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for r in &rows {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&r.name).strong());
                    ui.weak(RichText::new(format!("{} {}", r.tag, r.vr)).small());
                    ui.weak(RichText::new(format!("{}レベル", r.level.label())).small());
                });
                egui::Frame::NONE.fill(field_bg).inner_margin(egui::Margin::symmetric(6, 3)).corner_radius(2).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    match &r.value {
                        Ok(v) if v.is_empty() => {
                            ui.weak(RichText::new("(空)").monospace());
                        }
                        Ok(v) => {
                            ui.label(RichText::new(v).monospace());
                        }
                        Err(e) => {
                            ui.colored_label(ERROR, format!("✗ {e}"));
                        }
                    }
                    if let Some(b) = &r.encoded {
                        let hex: String = b.iter().map(|x| format!("{x:02X} ")).collect();
                        ui.weak(RichText::new(hex).small().monospace());
                    }
                });
                for w in &r.warnings {
                    ui.colored_label(WARN, format!("⚠ {w}"));
                }
                ui.add_space(6.0);
            }
        });
    }

    // ---- ログ ----------------------------------------------------------------------------

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            vw_ui::panel::header(ui, "ログ");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("消す").clicked() {
                    self.log.clear();
                }
            });
        });
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
            for l in &self.log {
                let text = RichText::new(l).monospace();
                if l.starts_with("エラー") {
                    ui.colored_label(ERROR, text);
                } else {
                    ui.label(text);
                }
            }
        });
    }

    // ---- プリセットの保存 ----------------------------------------------------------------

    fn save_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.save_dialog else { return };
        let mut action = None;
        let exists = self.presets.iter().any(|p| p == dialog.name.trim());
        let is_current = self.current_preset.as_deref() == Some(dialog.name.trim());
        egui::Window::new("プリセットを保存").collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label("名前");
            let r = ui.add(egui::TextEdit::singleline(&mut dialog.name).desired_width(280.0));
            if !r.has_focus() && !ui.memory(|m| m.focused().is_some()) {
                r.request_focus();
            }
            let ok = !dialog.name.trim().is_empty();
            if exists && !is_current {
                ui.colored_label(WARN, "⚠ 同じ名前のプリセットがある。保存すると上書きする");
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let label = if exists { "上書き保存" } else { "保存" };
                let enter = ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(ok, egui::Button::new(label)).clicked() || enter {
                    action = Some(true);
                }
                if ui.button("やめる").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = Some(false);
                }
            });
        });
        match action {
            Some(true) => {
                let n = self.save_dialog.take().map(|d| d.name).unwrap_or_default();
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
        Charset::Keep => "元ファイルの文字セットのまま。その文字セットで書けない値はエラーで止まる。",
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_last();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job();
        self.take_dropped(&ctx);
        self.refresh_engine();
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // メニューと道具の帯は窓の地の色、パネルはその一段上にして、枠・パネルが線に頼らず見分けられるようにする(vw と同じ)
        let chrome = ui.visuals().extreme_bg_color;
        egui::Panel::top("menu")
            .frame(egui::Frame::side_top_panel(ui.style()).fill(chrome))
            .show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar")
            .frame(egui::Frame::side_top_panel(ui.style()).fill(chrome).inner_margin(egui::Margin::symmetric(6, 6)))
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("log").resizable(true).default_size(150.0).show(ui, |ui| self.log_ui(ui));
        egui::Panel::left("settings").resizable(true).default_size(340.0).show(ui, |ui| self.settings_ui(ui));
        egui::Panel::right("preview").resizable(true).default_size(380.0).show(ui, |ui| self.preview_ui(ui));
        egui::CentralPanel::default().show(ui, |ui| self.rules_ui(ui));
        self.save_dialog_ui(&ctx);
    }
}
