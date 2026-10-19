//! 水増し本体。元の各ファイルを 1 回だけ読み込み、(患者, 検査, シリーズ, 画像) の組ごとに
//! 複製して UID と識別タグを振り直し、rayon で並列に書き出す。

use crate::charset::{is_multi_iso2022, scs_values, Charset, Iso2022};
use crate::config::{Config, CsvEncoding, Level, RuleSpec};
use crate::store;
use crate::template::{Template, Vars};
use crate::tree::{self, Place};
use anyhow::{bail, Context, Result};
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry, VirtualVr};
use dicom_core::value::Value;
use dicom_core::header::Header;
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, StandardDataDictionary};
use dicom_encoding::text::{SpecificCharacterSet, TextCodec};
use dicom_object::{open_file, InMemDicomObject};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use walkdir::WalkDir;

type Row = HashMap<String, String>;

pub struct Table {
    columns: Vec<String>,
    rows: Vec<Row>,
}

/// 読んだ CSV の置き場。画面は設定を変えるたびに Engine を作り直すので、CSV を読み直さずに済むよう
/// 画面が持ち、更新日時が変わったときだけ読み直す。
pub type TableCache = HashMap<(String, CsvEncoding), (Option<std::time::SystemTime>, Arc<Table>)>;

#[derive(Clone, Copy)]
enum UidLevel {
    Study,
    Series,
    Instance,
    Frame,
}

/// 置き換える UID のタグ(シーケンス内も含む)と、その階層。
const UID_TAGS: &[(Tag, UidLevel)] = &[
    (tags::STUDY_INSTANCE_UID, UidLevel::Study),
    (tags::SERIES_INSTANCE_UID, UidLevel::Series),
    (tags::SOP_INSTANCE_UID, UidLevel::Instance),
    (tags::REFERENCED_SOP_INSTANCE_UID, UidLevel::Instance),
    (tags::FRAME_OF_REFERENCE_UID, UidLevel::Frame),
    (Tag(0x3006, 0x0024), UidLevel::Frame), // ReferencedFrameOfReferenceUID
    (Tag(0x0020, 0x0200), UidLevel::Frame), // SynchronizationFrameOfReferenceUID
];

/// 何も指定しないときの規則。同じタグを指定すると上書きされる。
/// 患者の ID には元の患者の番号 `P` も入れる。複製の番号 `p` だけでは、元の患者が複数いるときに
/// 別の患者が同じ ID に潰れるため。
pub const DEFAULT_RULES: &[(&str, Level, &str)] = &[
    ("PatientID", Level::Patient, "EVAL%(P)03d-%(p)04d"),
    ("PatientName", Level::Patient, "EVAL^%(P)03d-%(p)04d"),
    ("StudyID", Level::Study, "%(s1)d"),
    ("AccessionNumber", Level::Study, "ACC%(P)03d%(p)04d%(s)02d"),
    ("SeriesNumber", Level::Series, "%(r1)d"),
    ("InstanceNumber", Level::Image, "%(inst)d"),
];

/// 規則の値に使える整数の変数。小文字は複製の番号、大文字は元の木での番号。
pub const BUILTIN_VARS: [&str; 14] = ["p", "p1", "P", "s", "s1", "S", "r", "r1", "R", "i", "i1", "I", "n", "inst"];

/// 元ファイルの値をそのまま使う文字列の変数(レベルの粗い順)。
pub const SRC_VARS: [&str; 7] = [
    "src_patient_id",
    "src_patient_name",
    "src_study_id",
    "src_study_description",
    "src_series_number",
    "src_series_description",
    "src_modality",
];

/// レベルごとに使える整数の変数。細かいレベルの番号は、粗いレベルのタグには使えない。
pub fn vars_for(level: Level) -> Vec<&'static str> {
    let upto = match level {
        Level::Patient => 3,
        Level::Study => 6,
        Level::Series => 9,
        Level::Image => 14,
    };
    // BUILTIN_VARS は患者 → 検査 → シリーズ → 画像の順に並べてある
    BUILTIN_VARS[..upto].to_vec()
}

/// レベルごとに使える、元ファイルの値の変数。
pub fn src_vars_for(level: Level) -> Vec<&'static str> {
    let upto = match level {
        Level::Patient => 2,
        Level::Study => 4,
        Level::Series | Level::Image => 7,
    };
    SRC_VARS[..upto].to_vec()
}

/// DICOM の標準でのタグのレベル(規則のレベルを決める根拠ではなく、食い違いを警告するために使う)。
/// 患者の属性のうち年齢・身長・体重は、標準では検査ごとの属性。
pub fn standard_level(tag: Tag) -> Level {
    const STUDY: &[Tag] = &[
        Tag(0x0008, 0x0020), // StudyDate
        Tag(0x0008, 0x0030), // StudyTime
        Tag(0x0008, 0x0050), // AccessionNumber
        Tag(0x0008, 0x0080), // InstitutionName
        Tag(0x0008, 0x0090), // ReferringPhysicianName
        Tag(0x0008, 0x1030), // StudyDescription
        Tag(0x0008, 0x1048), // PhysiciansOfRecord
        Tag(0x0008, 0x1060), // NameOfPhysiciansReadingStudy
        Tag(0x0020, 0x0010), // StudyID
    ];
    const SERIES: &[Tag] = &[
        Tag(0x0008, 0x0021), // SeriesDate
        Tag(0x0008, 0x0031), // SeriesTime
        Tag(0x0008, 0x0060), // Modality
        Tag(0x0008, 0x0070), // Manufacturer
        Tag(0x0008, 0x1010), // StationName
        Tag(0x0008, 0x103E), // SeriesDescription
        Tag(0x0008, 0x1070), // OperatorsName
        Tag(0x0008, 0x1090), // ManufacturerModelName
        Tag(0x0018, 0x0015), // BodyPartExamined
        Tag(0x0018, 0x1030), // ProtocolName
        Tag(0x0020, 0x0011), // SeriesNumber
    ];
    if matches!(tag, Tag(0x0010, 0x1010) | Tag(0x0010, 0x1020) | Tag(0x0010, 0x1030)) {
        Level::Study
    } else if tag.group() == 0x0010 {
        Level::Patient
    } else if STUDY.contains(&tag) {
        Level::Study
    } else if SERIES.contains(&tag) {
        Level::Series
    } else {
        Level::Image
    }
}

/// 日本語などを文字コードで符号化する文字列の VR。AE・CS などは ASCII 限りなので含めない。
fn is_text_vr(vr: VR) -> bool {
    matches!(vr, VR::PN | VR::LO | VR::SH | VR::LT | VR::ST | VR::UT | VR::UC)
}

pub const CANCELLED: &str = "中断した";

struct Rule {
    tag: Tag,
    name: String,
    /// 辞書で決まらないタグで、元ファイルにもないときは None
    vr: Option<VR>,
    tpl: Template,
    /// テンプレートの元の文字列(既定のままかを画面が見るため)
    value: String,
    /// このタグが読む CSV(タグごとの指定がなければ全体の CSV)と、1 行を進める単位
    table: Option<Arc<Table>>,
    level: Level,
}

/// 読んで、複製ごとに同じ結果になる書き換えを済ませた元ファイル。
struct Prepared {
    src: dicom_object::DefaultDicomObject,
    /// 日本語などを自前で符号化するときの文字セットの組
    flavor: Option<Iso2022>,
    /// 触れない(Keep)ときに、新しい値が元ファイルの文字セットで書けるかを調べるための符号化器
    keep_check: Option<(String, Option<SpecificCharacterSet>)>,
    src_inst_no: i64,
    path: PathBuf,
}

/// 1 つのアソシエーションで送るまとまり。
struct Group {
    label: String,
    /// 提案する (SOP Class UID, 転送構文の候補)
    contexts: Vec<(String, Vec<String>)>,
    /// (元ファイル, 木での位置, 送る複製の番号)
    items: Vec<(PathBuf, Place, Vec<Idx>)>,
    count: u64,
}

/// 複製の番号。UID はこれと元の UID から決定的に作る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Idx {
    pub p: u32,
    pub s: u32,
    pub r: u32,
    pub i: u32,
}

/// 送信の接続 1 本の、いまの状態。画面の「接続の状態」用。
#[derive(Clone, Debug, Default)]
pub struct WorkerStatus {
    /// いま送っているまとまり。何もしていなければ None
    pub group: Option<String>,
    pub done: u64,
    pub total: u64,
    pub started: Option<std::time::Instant>,
}

/// 送り終えたまとまり 1 つの結果。
#[derive(Clone, Debug)]
pub struct GroupResult {
    pub label: String,
    pub count: u64,
    pub sent: u64,
    pub rejected: u64,
    pub secs: f64,
    pub error: Option<String>,
}

/// 実行の進み具合。別スレッドの画面から読む。
#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub cancel: AtomicBool,
    pub log: Mutex<Vec<String>>,
    /// 標準エラーにも出す(コマンドライン用)
    pub echo: bool,
    /// 送信の接続ごとの状態(接続の数だけ)
    pub workers: Mutex<Vec<WorkerStatus>>,
    /// 送り終えたまとまりの結果
    pub results: Mutex<Vec<GroupResult>>,
    /// 接続ごとの端末風のログ(`[  12.345] …`。実行の始まりからの秒)
    pub worker_logs: Mutex<Vec<Vec<String>>>,
    pub started: Mutex<Option<std::time::Instant>>,
}

/// 接続のログに残す行数の上限。1 万件を送ると 1 万行になるので、古いものから捨てる
const WORKER_LOG_LINES: usize = 3000;

impl Progress {
    /// 接続 `k` の端末風のログに 1 行足す。
    pub fn wlog(&self, k: usize, msg: impl Into<String>) {
        let secs = self.started.lock().unwrap().map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let line = format!("[{secs:8.3}] {}", msg.into());
        let mut logs = self.worker_logs.lock().unwrap();
        if let Some(l) = logs.get_mut(k) {
            if l.len() >= WORKER_LOG_LINES {
                l.drain(..WORKER_LOG_LINES / 10);
            }
            l.push(line);
        }
    }

    pub fn say(&self, msg: impl Into<String>) {
        let msg = msg.into();
        if self.echo {
            eprintln!("{msg}");
        }
        self.log.lock().unwrap().push(msg);
    }
}

pub struct Engine {
    cfg: Config,
    ns: Uuid,
    rules: Vec<Rule>,
    /// 全体の CSV の列と行数(画面の表示用)
    pub csv_columns: Vec<String>,
    pub csv_rows: usize,
    tables: HashMap<(String, CsvEncoding), Arc<Table>>,
}

/// 画面に出す、規則 1 本ぶんの見本。
pub struct PreviewRow {
    pub tag: String,
    pub name: String,
    pub level: Level,
    pub vr: String,
    pub value: Result<String, String>,
    /// 文字コードで符号化したバイト列(日本語を ISO 2022 にしたときだけ)
    pub encoded: Option<Vec<u8>>,
    pub warnings: Vec<String>,
    /// CSV を使う規則なら、この複製で読む行(列名の行を 1 とした行番号)
    pub csv_line: Option<usize>,
}

impl Engine {
    pub fn new(cfg: &Config) -> Result<Engine> {
        Engine::with_cache(cfg, &mut TableCache::new())
    }

    pub fn with_cache(cfg: &Config, cache: &mut TableCache) -> Result<Engine> {
        if [cfg.patients, cfg.studies, cfg.series, cfg.images].contains(&0) {
            bail!("患者・検査・シリーズ・画像の数は 1 以上にしてほしい");
        }
        let mut tables = HashMap::new();
        let mut table = |path: &str, enc: CsvEncoding| -> Result<Arc<Table>> {
            let t = cached_table(cache, path, enc)?;
            tables.insert((path.to_string(), enc), t.clone());
            Ok(t)
        };
        let global = match cfg.csv.trim() {
            "" => None,
            path => Some(table(path, cfg.csv_encoding)?),
        };

        // 既定 → 設定の順に積み、同じタグは後のものが勝つ
        let mut specs: Vec<RuleSpec> = DEFAULT_RULES
            .iter()
            .map(|(t, l, v)| RuleSpec::new(t, *l, v))
            .collect();
        specs.extend(cfg.rules.iter().filter(|r| !(r.tag.trim().is_empty() && r.value.is_empty())).cloned());
        let mut rules: Vec<Rule> = Vec::new();
        for s in &specs {
            let own = match s.csv.trim() {
                "" => None,
                path => Some(table(path, s.csv_encoding.unwrap_or(cfg.csv_encoding)).with_context(|| s.tag.clone())?),
            };
            let r = parse_rule(s, own.or_else(|| global.clone()))?;
            rules.retain(|x| x.tag != r.tag);
            rules.push(r);
        }
        let (csv_columns, csv_rows) = global.map(|t| (t.columns.clone(), t.rows.len())).unwrap_or_default();
        Ok(Engine { cfg: cfg.clone(), ns: Uuid::NAMESPACE_OID, rules, csv_columns, csv_rows, tables })
    }

    /// タグごとの CSV の列と行数(画面の表示用)。読み込めていなければ None。
    pub fn table_info(&self, path: &str, enc: CsvEncoding) -> Option<(Vec<String>, usize)> {
        self.tables.get(&(path.trim().to_string(), enc)).map(|t| (t.columns.clone(), t.rows.len()))
    }

    fn uid(&self, orig: &str, level: UidLevel, ix: Idx) -> String {
        let key = match level {
            UidLevel::Study | UidLevel::Frame => format!("{orig}|{}|{}", ix.p, ix.s),
            UidLevel::Series => format!("{orig}|{}|{}|{}", ix.p, ix.s, ix.r),
            UidLevel::Instance => format!("{orig}|{}|{}|{}|{}", ix.p, ix.s, ix.r, ix.i),
        };
        format!("{}.{}", self.cfg.uid_root.trim(), Uuid::new_v5(&self.ns, key.as_bytes()).as_u128())
    }

    /// 通し番号(0 始まり)。元の画像の通し番号 → 患者 → 検査 → シリーズ → 画像の複製の順で、全出力で一意。
    fn serial(&self, ix: Idx, ordinal: u64) -> u64 {
        let c = &self.cfg;
        (((ordinal * c.patients as u64 + ix.p as u64) * c.studies as u64 + ix.s as u64) * c.series as u64 + ix.r as u64) * c.images as u64 + ix.i as u64
    }

    fn csv_row<'a>(&self, rule: &'a Rule, ix: Idx, place: &Place) -> Option<&'a Row> {
        let t = rule.table.as_deref()?;
        Some(&t.rows[self.csv_index(rule, ix, place) % t.rows.len()])
    }

    fn ints(&self, ix: Idx, place: &Place, src_inst_no: i64) -> Vec<(&'static str, i64)> {
        vec![
            ("p", ix.p as i64),
            ("p1", ix.p as i64 + 1),
            ("P", place.patient as i64),
            ("s", ix.s as i64),
            ("s1", ix.s as i64 + 1),
            ("S", place.study as i64),
            ("r", ix.r as i64),
            ("r1", ix.r as i64 + 1),
            ("R", place.series as i64),
            ("i", ix.i as i64),
            ("i1", ix.i as i64 + 1),
            ("I", place.image as i64),
            ("n", self.serial(ix, place.ordinal) as i64 + 1),
            ("inst", src_inst_no + ix.i as i64 * 10000),
        ]
    }

    fn strs(place: &Place) -> [(&'static str, &str); 7] {
        [
            ("src_patient_id", &place.patient_id),
            ("src_patient_name", &place.patient_name),
            ("src_study_id", &place.study_id),
            ("src_study_description", &place.study_description),
            ("src_series_number", &place.series_number),
            ("src_series_description", &place.series_description),
            ("src_modality", &place.modality),
        ]
    }

    /// 元ファイルを読まずに、規則を `ix` の複製(元の木では `place`)に当てた値を見せる。
    pub fn preview(&self, ix: Idx, place: &Place) -> Vec<PreviewRow> {
        let ints = self.ints(ix, place, 1);
        let strs = Self::strs(place);
        let japanese = Iso2022::japanese();
        self.rules
            .iter()
            .map(|r| {
                let vars = Vars { ints: &ints, strs: &strs, csv: self.csv_row(r, ix, place) };
                let value = r.tpl.render(&vars).map_err(|e| format!("{e:#}"));
                let vr = r.vr;
                let mut warnings = Vec::new();
                if r.level > standard_level(r.tag) {
                    warnings.push(format!(
                        "標準では{}レベルのタグ。{}レベルにすると、同じ{}の中で値がばらつく",
                        standard_level(r.tag).label(),
                        r.level.label(),
                        standard_level(r.tag).label()
                    ));
                }
                let mut encoded = None;
                if let (Ok(v), Some(vr)) = (&value, vr) {
                    warnings.extend(check_value(vr, v));
                    if self.cfg.charset == Charset::Iso2022jp && is_text_vr(vr) && !v.is_ascii() {
                        match japanese.encode(v) {
                            Ok(b) => encoded = Some(b),
                            Err(e) => warnings.push(format!("{e:#}")),
                        }
                    }
                }
                PreviewRow {
                    tag: r.tag.to_string(),
                    name: r.name.clone(),
                    level: r.level,
                    vr: vr.map(|v| v.to_string().to_string()).unwrap_or_else(|| "?".into()),
                    value,
                    encoded,
                    warnings,
                    csv_line: self.csv_line(r, ix, place),
                }
            })
            .collect()
    }

    pub fn sources(&self) -> Result<Vec<PathBuf>> {
        list_sources(&self.cfg.input)
    }

    /// タグに規則があれば、`ix` の複製(元の木では `place`)での値。なければ None(元ファイルの値のまま)。
    /// 画面が複製後の木のラベル(患者 ID・名前・検査の説明など)を出すのに使う。
    pub fn render_rule(&self, tag: Tag, ix: Idx, place: &Place) -> Option<Result<String, String>> {
        let r = self.rules.iter().find(|r| r.tag == tag)?;
        let ints = self.ints(ix, place, 1);
        let strs = Self::strs(place);
        let vars = Vars { ints: &ints, strs: &strs, csv: self.csv_row(r, ix, place) };
        Some(r.tpl.render(&vars).map_err(|e| format!("{e:#}")))
    }

    /// InstanceNumber の規則が既定(`元の番号 + 複製番号 × 10000`)のままか。
    /// そのときだけ、画面が複製後の画像の番号の範囲を元の範囲から計算できる。
    pub fn instance_number_is_default(&self) -> bool {
        self.rules.iter().find(|r| r.tag == tags::INSTANCE_NUMBER).is_some_and(|r| r.value == "%(inst)d")
    }

    /// 規則が読む CSV の、`ix` の複製で使う行(1 行目の列名を含めた行番号)。
    fn csv_line(&self, rule: &Rule, ix: Idx, place: &Place) -> Option<usize> {
        let t = rule.table.as_deref()?;
        if !rule.tpl.uses_csv(&t.columns) {
            return None;
        }
        Some(self.csv_index(rule, ix, place) % t.rows.len() + 2)
    }

    /// CSV の行の番号(0 始まり、折り返す前)。そのレベルの単位で、隙間なく 1 つずつ進む。
    /// 元の木での通し番号(患者・検査・シリーズ・画像)を織り込むので、元が違えば必ず別の行になり、
    /// 同じ元の複製どうしも別の行になる。
    fn csv_index(&self, rule: &Rule, ix: Idx, place: &Place) -> usize {
        let c = &self.cfg;
        let (np, ns, nr) = (c.patients as u64, c.studies as u64, c.series as u64);
        let k = match rule.level {
            Level::Patient => place.patient as u64 * np + ix.p as u64,
            Level::Study => (place.study_ordinal * np + ix.p as u64) * ns + ix.s as u64,
            Level::Series => ((place.series_ordinal * np + ix.p as u64) * ns + ix.s as u64) * nr + ix.r as u64,
            Level::Image => self.serial(ix, place.ordinal),
        };
        k as usize
    }

    /// 通し番号(1 始まり)。画面の表示用。
    pub fn serial_number(&self, ix: Idx, place: &Place) -> u64 {
        self.serial(ix, place.ordinal) + 1
    }

    pub fn planned(&self, sources: usize) -> u64 {
        let c = &self.cfg;
        sources as u64 * c.patients as u64 * c.studies as u64 * c.series as u64 * c.images as u64
    }

    /// ディスクに書くか(送るだけのときは書かない)。
    fn writes_files(&self) -> bool {
        !self.cfg.store.enabled || self.cfg.store.also_write
    }

    /// 書き出した(送った)数を返す。中断したときも、そこまでの数を返す。
    pub fn run(&self, progress: &Progress) -> Result<u64> {
        if self.writes_files() && self.cfg.output.trim().is_empty() {
            bail!("出力先を指定してほしい");
        }
        self.preflight()?;
        let files = self.sources()?;
        // 元の木を先に読む。元の患者の番号などを規則の変数にし、送信では提示コンテキストを集めるため
        let tree = tree::scan(&files, &AtomicBool::new(false), &AtomicU64::new(0));
        let c = tree.counts();
        progress.total.store(self.planned(tree.files), Ordering::Relaxed);
        progress.say(format!(
            "元: {} 患者 / {} 検査 / {} シリーズ / {} 画像({} ファイル) → {} {} 個",
            c.patients,
            c.studies,
            c.series,
            c.images,
            tree.files,
            if self.cfg.store.enabled { "送信" } else { "出力" },
            progress.total.load(Ordering::Relaxed)
        ));
        tree::log_skipped(&tree.skipped, |m| progress.say(m));
        if self.cfg.store.enabled {
            return self.run_store(&tree, &files, progress);
        }
        let result: Result<Vec<u64>> = files
            .par_iter()
            .filter_map(|f| tree.places.get(f).map(|place| (f, place)))
            .map(|(f, place)| self.process_file(f, place, progress))
            .collect();
        match result {
            Ok(v) => Ok(v.iter().sum()),
            Err(e) if e.to_string() == CANCELLED => {
                progress.say(CANCELLED);
                Ok(progress.done.load(Ordering::Relaxed))
            }
            Err(e) => Err(e),
        }
    }

    /// 1 つのアソシエーションで送るまとまり。
    fn groups(&self, tree: &tree::SourceTree, files: &[PathBuf]) -> Vec<Group> {
        use crate::config::AssocUnit;
        let c = &self.cfg;
        let n = c.store.connections.max(1) as u64;
        let mut listed: Vec<(&PathBuf, &Place)> = files.iter().filter_map(|f| tree.places.get(f).map(|p| (f, p))).collect();
        listed.sort_by_key(|(_, p)| p.ordinal);
        let mut map: std::collections::BTreeMap<String, Group> = std::collections::BTreeMap::new();
        for (f, place) in listed {
            let mut per_key: std::collections::BTreeMap<String, Vec<Idx>> = std::collections::BTreeMap::new();
            for ix in self.combos() {
                let key = match c.store.unit {
                    AssocUnit::Modality => format!("モダリティ {}", if place.modality.is_empty() { "(なし)" } else { &place.modality }),
                    AssocUnit::Patient => format!("患者 P{} p{}", place.patient, ix.p),
                    AssocUnit::Study => format!("検査 P{} p{} S{} s{}", place.patient, ix.p, place.study, ix.s),
                    AssocUnit::Series => format!("シリーズ P{} p{} S{} s{} R{} r{}", place.patient, ix.p, place.study, ix.s, place.series, ix.r),
                    AssocUnit::All => format!("組 {}", self.serial(ix, place.ordinal) % n + 1),
                };
                per_key.entry(key).or_default().push(ix);
            }
            for (key, ixs) in per_key {
                let g = map.entry(key.clone()).or_insert_with(|| Group { label: key, contexts: vec![], items: vec![], count: 0 });
                let ctx = (place.sop_class.clone(), store::proposable(&place.transfer_syntax));
                if !g.contexts.contains(&ctx) {
                    g.contexts.push(ctx);
                }
                g.count += ixs.len() as u64;
                g.items.push((f.clone(), place.clone(), ixs));
            }
        }
        map.into_values().collect()
    }

    /// C-STORE で送る。接続の数だけスレッドを立て、各スレッドがグループを 1 つずつ受け持って、
    /// アソシエーションを張り、複製を作りながら送り、送り切ったら解放する。
    fn run_store(&self, tree: &tree::SourceTree, files: &[PathBuf], progress: &Progress) -> Result<u64> {
        let groups = self.groups(tree, files);
        self.send_groups(groups, progress)
    }

    /// 水増しも書き換えもせず、ディレクトリの DICOM をそのまま送る(storescu と同じ)。
    pub fn run_send_as_is(&self, progress: &Progress) -> Result<u64> {
        let dir = self.cfg.store.as_is_dir.trim();
        let files = list_sources(dir).with_context(|| "そのまま送るディレクトリ")?;
        let tree = tree::scan(&files, &AtomicBool::new(false), &AtomicU64::new(0));
        let c = tree.counts();
        progress.total.store(tree.files as u64, Ordering::Relaxed);
        progress.say(format!("そのまま送る: {} 患者 / {} 検査 / {} シリーズ / {} 画像({} ファイル)", c.patients, c.studies, c.series, c.images, tree.files));
        tree::log_skipped(&tree.skipped, |m| progress.say(m));
        // 複製の番号は使わないので、全部 0 のまま。単位の分け方(モダリティ・検査…)はそのまま効く
        let one = Engine { cfg: Config { patients: 1, studies: 1, series: 1, images: 1, ..self.cfg.clone() }, ..self.shallow() };
        let groups = one.groups(&tree, &files);
        one.send_groups_with(groups, progress, true)
    }

    /// 設定だけを引き継いだ Engine(規則・CSV は空)。そのまま送るときに、件数を 1 にしたものを作るため。
    fn shallow(&self) -> Engine {
        Engine { cfg: self.cfg.clone(), ns: self.ns, rules: vec![], csv_columns: vec![], csv_rows: 0, tables: HashMap::new() }
    }

    fn send_groups(&self, groups: Vec<Group>, progress: &Progress) -> Result<u64> {
        self.send_groups_with(groups, progress, false)
    }

    fn send_groups_with(&self, groups: Vec<Group>, progress: &Progress, as_is: bool) -> Result<u64> {
        let cfg = &self.cfg.store;
        let n = cfg.connections.max(1) as usize;
        *progress.workers.lock().unwrap() = vec![WorkerStatus::default(); n];
        *progress.worker_logs.lock().unwrap() = vec![vec![]; n];
        *progress.started.lock().unwrap() = Some(std::time::Instant::now());
        progress.results.lock().unwrap().clear();
        progress.say(format!(
            "C-STORE {} へ、{} を単位に {} 本のアソシエーション({} 接続まで同時)で送る",
            cfg.target(),
            cfg.unit.label(),
            groups.len(),
            cfg.connections.max(1)
        ));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: Vec<Result<store::Stats>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..cfg.connections.max(1) as usize)
                .map(|k| {
                    let (next, groups) = (&next, &groups);
                    scope.spawn(move || {
                        let r = self.store_worker(k, groups, next, progress, as_is);
                        if let Err(e) = &r {
                            progress.say(format!("エラー: 接続 {}: {e:#}", k + 1));
                            // ほかの接続も止める。残りを送っても結果が片手落ちになるため
                            progress.cancel.store(true, Ordering::Relaxed);
                        }
                        r
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap_or_else(|_| Err(anyhow::anyhow!("送信のスレッドが途中で止まった")))).collect()
        });
        let mut total = store::Stats::default();
        let mut first_err = None;
        let mut cancelled = false;
        for r in results {
            match r {
                Ok(s) => total += s,
                // ほかの接続の失敗を受けて止まった「中断した」より、本当の原因を先に伝える
                Err(e) if e.to_string() == CANCELLED => cancelled = true,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        progress.say(format!("送信 {} 個、受け側が拒否 {} 個", total.sent, total.rejected));
        if let Some(e) = first_err {
            return Err(e);
        }
        if cancelled {
            progress.say(CANCELLED);
            return Ok(total.sent);
        }
        if total.rejected > 0 {
            bail!("{} 個が受け側に拒否された(ログを見てほしい)", total.rejected);
        }
        Ok(total.sent)
    }

    fn store_worker(&self, k: usize, groups: &[Group], next: &std::sync::atomic::AtomicUsize, progress: &Progress, as_is: bool) -> Result<store::Stats> {
        let mut total = store::Stats::default();
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                bail!(CANCELLED);
            }
            let g = next.fetch_add(1, Ordering::Relaxed);
            let Some(group) = groups.get(g) else { break };
            let started = std::time::Instant::now();
            let set_status = |done: u64| {
                if let Some(w) = progress.workers.lock().unwrap().get_mut(k) {
                    *w = WorkerStatus { group: Some(group.label.clone()), done, total: group.count, started: Some(started) };
                }
            };
            set_status(0);
            let r = self.send_group(k, group, progress, as_is, &set_status);
            if let Some(w) = progress.workers.lock().unwrap().get_mut(k) {
                *w = WorkerStatus::default();
            }
            let secs = started.elapsed().as_secs_f64();
            match r {
                Ok(st) => {
                    progress.results.lock().unwrap().push(GroupResult { label: group.label.clone(), count: group.count, sent: st.sent, rejected: st.rejected, secs, error: None });
                    total += st;
                }
                Err(e) => {
                    progress.results.lock().unwrap().push(GroupResult { label: group.label.clone(), count: group.count, sent: 0, rejected: 0, secs, error: Some(format!("{e:#}")) });
                    return Err(e);
                }
            }
        }
        Ok(total)
    }

    /// 1 まとまりを、アソシエーションを 1 本張って送り切る。
    fn send_group(&self, k: usize, group: &Group, progress: &Progress, as_is: bool, set_status: &dyn Fn(u64)) -> Result<store::Stats> {
        let cfg = &self.cfg.store;
        progress.wlog(k, format!("A-ASSOCIATE-RQ {} ← {} : {}", cfg.target(), cfg.calling_ae.trim(), group.label));
        for (sop, ts) in &group.contexts {
            progress.wlog(k, format!("  提案 {sop} [{}]", ts.join(", ")));
        }
        let (mut conn, refused) = match store::Connection::open(cfg, &group.contexts) {
            Ok(c) => c,
            Err(e) => {
                progress.wlog(k, format!("A-ASSOCIATE 失敗: {e:#}"));
                return Err(e.context(group.label.clone()));
            }
        };
        progress.wlog(k, format!("A-ASSOCIATE-AC 受諾 ({} 個を送る)", group.count));
        progress.say(format!("接続 {}: {} を張った({} 個、SOP Class {} 種)", k + 1, group.label, group.count, group.contexts.len()));
        for r in &refused {
            progress.wlog(k, format!("  拒否された抽象構文 {r}"));
        }
        if !refused.is_empty() {
            progress.say(format!("接続 {}: 受け側が受けつけない SOP Class(送れない): {}", k + 1, refused.join(", ")));
        }
        let mut done = 0u64;
        for (path, place, ixs) in &group.items {
            // そのまま送るときは、読んだものを書き換えずに送る
            let prepared = if as_is { None } else { self.prepare(path, place, progress)? };
            if !as_is && prepared.is_none() {
                continue;
            }
            for &ix in ixs {
                if progress.cancel.load(Ordering::Relaxed) {
                    let _ = conn.close();
                    bail!(CANCELLED);
                }
                let (sop, ds) = match &prepared {
                    Some(p) => self.make(p, place, ix)?,
                    None => {
                        let ds = open_file(path).with_context(|| path.display().to_string())?;
                        let sop = ds.meta().media_storage_sop_instance_uid().trim_end_matches(['\0', ' ']).to_string();
                        (sop, ds)
                    }
                };
                if !as_is && self.writes_files() {
                    self.write(&ds, &sop, ix, path)?;
                }
                // ディスクを経由せず、データセットをそのまま送る
                let inst = store::Instance { sop_class: place.sop_class.clone(), sop_instance: sop, object: ds.into_inner() };
                let short = inst.sop_instance.rsplit('.').next().unwrap_or("").chars().rev().take(10).collect::<String>().chars().rev().collect::<String>();
                let outcome = match conn.store(&inst) {
                    Ok(o) => o,
                    Err(e) => {
                        progress.wlog(k, format!("C-STORE-RQ …{short} 送信に失敗: {e:#}"));
                        return Err(e);
                    }
                };
                match outcome {
                    store::Outcome::Sent => progress.wlog(k, format!("C-STORE-RQ …{short}  → RSP 0000 (成功)")),
                    store::Outcome::Warned(st) => {
                        progress.wlog(k, format!("C-STORE-RQ …{short}  → RSP {st:04X} (警告)"));
                        progress.say(format!("警告つきで受けつけた(status {st:04X}): {}", inst.sop_instance));
                    }
                    store::Outcome::Rejected(st) => {
                        progress.wlog(k, format!("C-STORE-RQ …{short}  → RSP {st:04X} (失敗)"));
                        progress.say(format!("受け側が拒否した(status {st:04X}): {}", inst.sop_instance));
                    }
                    store::Outcome::Unsupported => {
                        progress.wlog(k, format!("C-STORE-RQ …{short}  送れない: 抽象構文 {} が未受諾", inst.sop_class));
                        progress.say(format!("送れない: SOP Class {} を受け側が受けつけない({})", inst.sop_class, inst.sop_instance));
                    }
                }
                done += 1;
                set_status(done);
                progress.done.fetch_add(1, Ordering::Relaxed);
            }
        }
        let st = conn.close().with_context(|| group.label.clone())?;
        progress.wlog(k, format!("A-RELEASE 完了: 成功 {} / 拒否 {}", st.sent, st.rejected));
        Ok(st)
    }

    fn combos(&self) -> Vec<Idx> {
        let c = &self.cfg;
        let mut combos = Vec::with_capacity((c.patients * c.studies * c.series * c.images) as usize);
        for p in 0..c.patients {
            for s in 0..c.studies {
                for r in 0..c.series {
                    for i in 0..c.images {
                        combos.push(Idx { p, s, r, i });
                    }
                }
            }
        }
        combos
    }

    /// 1 複製を書く。`出力先/P患者/S検査/Rシリーズ/SOPInstanceUID.dcm`。
    fn write(&self, ds: &dicom_object::DefaultDicomObject, sop: &str, ix: Idx, path: &Path) -> Result<()> {
        let dir = Path::new(self.cfg.output.trim())
            .join(format!("P{:06}", ix.p))
            .join(format!("S{:03}", ix.s))
            .join(format!("R{:03}", ix.r));
        std::fs::create_dir_all(&dir)?;
        // ファイルへ直接書くと、符号化に失敗したとき途中で切れた壊れたファイルが残る
        let mut buf = Vec::new();
        ds.write_all(&mut buf).with_context(|| path.display().to_string())?;
        std::fs::write(dir.join(format!("{sop}.dcm")), buf)?;
        Ok(())
    }

    /// 書き出す前に、CSV の全行(CSV を使わない規則は 1 回)を規則に当てて、書けない値を見つける。
    /// 10 万ファイルを書いた途中で、CSV の 58 行目の字が書けないと分かるのでは遅いため。
    /// 元ファイルの文字セットに頼る「触れない」は、元ファイルを読むまで分からないので、ここでは調べない。
    fn preflight(&self) -> Result<()> {
        let flavor = (self.cfg.charset == Charset::Iso2022jp).then(Iso2022::japanese);
        let place = Place::default();
        let ints = self.ints(Idx { p: 0, s: 0, r: 0, i: 0 }, &place, 1);
        let strs = Self::strs(&place);
        for rule in &self.rules {
            let Some(vr) = rule.vr else { continue };
            let rows: Vec<Option<&Row>> = match &rule.table {
                Some(t) if rule.tpl.uses_csv(&t.columns) => t.rows.iter().map(Some).collect(),
                _ => vec![None],
            };
            for (k, row) in rows.into_iter().enumerate() {
                let origin = match (&rule.table, row) {
                    (Some(t), Some(_)) => format!("CSV の {} 行目(列: {})", k + 2, t.columns.join(" ")),
                    _ => "テンプレート".to_string(),
                };
                let text = rule
                    .tpl
                    .render(&Vars { ints: &ints, strs: &strs, csv: row })
                    .with_context(|| format!("{} {}: {origin}", rule.name, rule.tag))?;
                check_writable(vr, &text, flavor.as_ref()).with_context(|| format!("{} {}: {origin}: `{text}`", rule.name, rule.tag))?;
            }
        }
        Ok(())
    }

    /// ファイルに書く経路。元を 1 回読み、複製を並列に作って書く。
    fn process_file(&self, path: &Path, place: &Place, progress: &Progress) -> Result<u64> {
        let Some(prepared) = self.prepare(path, place, progress)? else { return Ok(0) };
        let combos = self.combos();
        combos.par_iter().try_for_each(|&ix| -> Result<()> {
            if progress.cancel.load(Ordering::Relaxed) {
                bail!(CANCELLED);
            }
            let (sop, ds) = self.make(&prepared, place, ix)?;
            self.write(&ds, &sop, ix, path)?;
            progress.done.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;
        Ok(combos.len() as u64)
    }

    /// 元ファイルを読み、複製ごとに同じ結果になる書き換えを済ませておく。読めなければ None(飛ばす)。
    fn prepare(&self, path: &Path, _place: &Place, progress: &Progress) -> Result<Option<Prepared>> {
        let c = &self.cfg;
        // 木を読めたファイルだけが来るが、念のため読めなければ飛ばす
        let mut src = match open_file(path) {
            Ok(s) => s,
            Err(e) => {
                progress.say(format!("飛ばした: {} ({e})", path.display()));
                progress.total.fetch_sub(
                    c.patients as u64 * c.studies as u64 * c.series as u64 * c.images as u64,
                    Ordering::Relaxed,
                );
                return Ok(None);
            }
        };
        let src_scs = src
            .get(tags::SPECIFIC_CHARACTER_SET)
            .and_then(|e| e.to_str().ok())
            .map(|v| scs_values(&v))
            .unwrap_or_default();
        let multi = is_multi_iso2022(&src_scs);
        let spec: Option<Iso2022> = if multi { Iso2022::from_values(&src_scs) } else { None };
        // 日本語などを自前で符号化するときの文字セットの組
        let flavor: Option<Iso2022> = match c.charset {
            Charset::Iso2022jp => Some(Iso2022::japanese()),
            Charset::Keep if multi => Some(spec.clone().with_context(|| unsupported_scs(path, &src_scs))?),
            _ => None,
        };
        // 触れない(Keep)ときに、新しい値が元ファイルの文字セットで書けるかを調べるための符号化器
        let keep_check = (c.charset == Charset::Keep && !multi).then(|| {
            let code = src_scs.first().map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("ISO_IR 6");
            (code.to_string(), SpecificCharacterSet::from_code(code))
        });
        // dicom-rs は複数値の ISO 2022 を先頭の値だけで読み、ESC $ B などが文字列に残る。
        // 触れない(Keep)なら、そのバイト列のまま書き戻せば元どおりなので、読み直すのは書き換えるときだけ
        if multi && c.charset != Charset::Keep {
            let spec = spec.as_ref().with_context(|| unsupported_scs(path, &src_scs))?;
            decode_source_text(&mut src, spec).with_context(|| format!("元ファイルの文字列を読めない: {}", path.display()))?;
        }
        // 複製ごとに同じ結果になる書き換えは、複製する前に元に対して 1 回だけ行う。
        // 触れない(Keep)の元ファイルの文字列は、dicom-rs が読んだ文字列をそのバイト列に書き戻すだけで元どおり
        if let (Some(f), Charset::Iso2022jp) = (&flavor, c.charset) {
            reencode_existing(&mut src, f).with_context(|| path.display().to_string())?;
        }
        for rule in &self.rules {
            ensure_private_creator(&mut src, rule.tag);
        }
        let src_inst_no: i64 = src
            .get(tags::INSTANCE_NUMBER)
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        Ok(Some(Prepared { src, flavor, keep_check, src_inst_no, path: path.to_path_buf() }))
    }

    /// `ix` の複製を作る。UID を振り直し、規則を当てる。(SOPInstanceUID, データセット) を返す。
    fn make(&self, prepared: &Prepared, place: &Place, ix: Idx) -> Result<(String, dicom_object::DefaultDicomObject)> {
        let c = &self.cfg;
        let path = &prepared.path;
        let mut ds = prepared.src.clone();
        self.rewrite_uids(&mut ds, ix);

        if let Some(code) = c.charset.code() {
            ds.put_str(tags::SPECIFIC_CHARACTER_SET, VR::CS, code);
        }
        let ints = self.ints(ix, place, prepared.src_inst_no);
        let strs = Self::strs(place);
        for rule in &self.rules {
            let vars = Vars { ints: &ints, strs: &strs, csv: self.csv_row(rule, ix, place) };
            let text = rule.tpl.render(&vars).with_context(|| format!("{} ({})", rule.tag, path.display()))?;
            // 元ファイルにある要素はその VR を、なければ辞書か規則の VR を使う。暗黙 VR のファイルでは
            // 辞書にないタグが UN で読まれるので、UN は「決まっていない」として扱う
            let source_vr = ds.get(rule.tag).map(|e| e.vr()).filter(|v| *v != VR::UN);
            let Some(vr) = source_vr.or(rule.vr) else {
                bail!("{} の VR が決まらない(元ファイルでは UN)。規則に /LO のように VR を添えてほしい", rule.tag)
            };
            if let Some((code, cs)) = &prepared.keep_check {
                check_keep(vr, &text, code, cs.as_ref()).with_context(|| format!("{} {} ({})", rule.name, rule.tag, path.display()))?;
            }
            put_value(&mut ds, rule.tag, vr, &text, prepared.flavor.as_ref())
                .with_context(|| format!("{} {} ({})", rule.name, rule.tag, path.display()))?;
        }

        let sop = ds
            .get(tags::SOP_INSTANCE_UID)
            .map(|e| e.to_str().map(|s| s.trim_end_matches('\0').to_string()))
            .transpose()?
            .context("SOPInstanceUID がない")?;
        ds.meta_mut().media_storage_sop_instance_uid = sop.clone();
        ds.meta_mut().update_information_group_length();
        Ok((sop, ds))
    }

    /// シーケンスの中まで辿って、UID のタグを置き換える。
    fn rewrite_uids(&self, obj: &mut InMemDicomObject, ix: Idx) {
        let found: Vec<Tag> = obj.tags().collect();
        for tag in found {
            let level = UID_TAGS.iter().find(|(t, _)| *t == tag).map(|(_, l)| *l);
            obj.update_value(tag, |v| match v {
                Value::Primitive(p) => {
                    if let Some(level) = level {
                        let orig = p.to_str().trim_end_matches('\0').trim().to_string();
                        *p = PrimitiveValue::from(self.uid(&orig, level, ix));
                    }
                }
                Value::Sequence(seq) => {
                    for item in seq.items_mut() {
                        self.rewrite_uids(item, ix);
                    }
                }
                _ => {}
            });
        }
    }
}

fn unsupported_scs(path: &Path, values: &[String]) -> String {
    format!(
        "元ファイルの文字セット `{}` は扱えない(先頭が空・IR 6・IR 13・IR 100/101/109/110/126/127/138/144/166 のどれかで、続きが IR 87 かそれらの別のセットの組だけ): {}",
        values.join("\\"),
        path.display()
    )
}

/// dicom-rs が読んだ複数値の ISO 2022 の文字列を、本来の文字列に直す(シーケンスの中も)。
fn decode_source_text(obj: &mut InMemDicomObject, spec: &Iso2022) -> Result<()> {
    let found: Vec<(Tag, VR)> = obj.iter().map(|e| (e.tag(), e.vr())).collect();
    for (tag, vr) in found {
        if vr != VR::SQ && !is_text_vr(vr) {
            continue;
        }
        let mut err: Option<anyhow::Error> = None;
        obj.update_value(tag, |v| match v {
            Value::Sequence(seq) => {
                for item in seq.items_mut() {
                    if let Err(e) = decode_source_text(item, spec) {
                        err = Some(e);
                    }
                }
            }
            Value::Primitive(p) => {
                let decoded: Result<Vec<String>> = p
                    .to_multi_str()
                    .iter()
                    .map(|t| match spec.raw_bytes_of(t) {
                        // ESC がなければ、dicom-rs が先頭の値で読んだとおりで正しい
                        Some(b) if b.contains(&0x1b) => spec.decode(&b).with_context(|| format!("{tag}")),
                        _ => Ok(t.clone()),
                    })
                    .collect();
                match decoded {
                    Ok(mut texts) if texts.len() == 1 => *p = PrimitiveValue::from(texts.remove(0)),
                    Ok(texts) => *p = PrimitiveValue::Strs(texts.into()),
                    Err(e) => err = Some(e),
                }
            }
            _ => {}
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    Ok(())
}

/// プライベートタグ(奇数グループ)は、その持ち主を示す要素(グループ, 0x00NN)が先にないと規格に合わない。
/// 元ファイルになければ足す。
fn ensure_private_creator(obj: &mut InMemDicomObject, tag: Tag) {
    if tag.group() % 2 == 1 && tag.element() >= 0x1000 {
        let creator = Tag(tag.group(), tag.element() >> 8);
        if obj.get(creator).is_none() {
            obj.put_str(creator, VR::LO, "DicoAmplio");
        }
    }
}

/// 元ファイルにすでにある日本語などの文字列を、ISO 2022 のバイト列に直す。
/// dicom-rs は複数値の文字セットを切り替えて書けないので、文字セットを替えたぶんは自分で符号化する。
fn reencode_existing(obj: &mut InMemDicomObject, flavor: &Iso2022) -> Result<()> {
    let found: Vec<(Tag, VR)> = obj.iter().map(|e| (e.tag(), e.vr())).collect();
    for (tag, vr) in found {
        if vr == VR::SQ {
            let mut err = None;
            obj.update_value(tag, |v| {
                if let Value::Sequence(seq) = v {
                    for item in seq.items_mut() {
                        if let Err(e) = reencode_existing(item, flavor) {
                            err = Some(e);
                        }
                    }
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        } else if is_text_vr(vr) {
            let Some(text) = obj.get(tag).and_then(|e| match e.value() {
                Value::Primitive(p) => Some(p.to_str().into_owned()),
                _ => None,
            }) else {
                continue;
            };
            if !text.is_ascii() {
                let bytes = flavor.encode(&text).with_context(|| format!("元ファイルの {tag}: `{text}`"))?;
                obj.put(DataElement::new(tag, vr, PrimitiveValue::U8(bytes.into())));
            }
        }
    }
    Ok(())
}

/// 数値の VR は数として、日本語を ISO 2022 にするときは符号化したバイト列として、
/// それ以外は文字列として入れる。
fn put_value(obj: &mut InMemDicomObject, tag: Tag, vr: VR, v: &str, flavor: Option<&Iso2022>) -> Result<()> {
    let num = |v: &str| -> Result<f64> { v.trim().parse::<f64>().with_context(|| format!("{tag} は数値が要る: `{v}`")) };
    let el = |value: PrimitiveValue| DataElement::new(tag, vr, value);
    match vr {
        VR::US => obj.put(el(PrimitiveValue::from(num(v)? as u16))),
        VR::UL => obj.put(el(PrimitiveValue::from(num(v)? as u32))),
        VR::SS => obj.put(el(PrimitiveValue::from(num(v)? as i16))),
        VR::SL => obj.put(el(PrimitiveValue::from(num(v)? as i32))),
        VR::FL => obj.put(el(PrimitiveValue::from(num(v)? as f32))),
        VR::FD => obj.put(el(PrimitiveValue::from(num(v)?))),
        _ if flavor.is_some() && is_text_vr(vr) && !v.is_ascii() => {
            let bytes = flavor.unwrap().encode(v).with_context(|| format!("値 `{v}`"))?;
            obj.put(el(PrimitiveValue::U8(bytes.into())))
        }
        _ => obj.put_str(tag, vr, v),
    };
    Ok(())
}

fn parse_rule(spec: &RuleSpec, table: Option<Arc<Table>>) -> Result<Rule> {
    let csv_cols: &[String] = table.as_ref().map(|t| t.columns.as_slice()).unwrap_or(&[]);
    // `タグ` か `タグ/VR`。辞書にないタグ(プライベートなど)は VR が要る
    let (key, vr_text) = match spec.tag.trim().split_once('/') {
        Some((k, v)) => (k.trim(), Some(v.trim())),
        None => (spec.tag.trim(), None),
    };
    let given_vr = match vr_text {
        Some(v) => Some(
            <[u8; 2]>::try_from(v.as_bytes()).ok().and_then(VR::from_binary).with_context(|| format!("VR が読めない: `{v}`(LO や PN のような 2 文字)"))?,
        ),
        None => None,
    };
    let entry = StandardDataDictionary.by_expr(key);
    let tag = match entry {
        Some(e) => e.tag(),
        None => key.parse::<Tag>().ok().with_context(|| format!("タグ `{key}` が読めない(辞書の名前か (gggg,eeee))"))?,
    };
    if UID_TAGS.iter().any(|(t, _)| *t == tag) || tag == tags::MEDIA_STORAGE_SOP_INSTANCE_UID {
        bail!("{key} は UID として自動で振り直すので、規則では指定できない");
    }
    let vr = given_vr.or(match entry.map(|e| e.vr()) {
        Some(VirtualVr::Exact(vr)) => Some(vr),
        _ => None,
    });
    if entry.is_none() && vr.is_none() {
        bail!("辞書にないタグ {key} は VR が要る。例: {key}/LO");
    }
    let name = entry.map(|e| e.alias().to_string()).unwrap_or_else(|| tag.to_string());
    let level = spec.level;
    let tpl = Template::parse(&spec.value).with_context(|| key.to_string())?;
    let allowed: Vec<&str> = vars_for(level).into_iter().chain(src_vars_for(level)).collect();
    for n in tpl.names() {
        let in_csv = csv_cols.iter().any(|c| c == n);
        let builtin = BUILTIN_VARS.contains(&n) || SRC_VARS.contains(&n);
        if in_csv && builtin {
            // 書式によって組み込みの番号と CSV の列のどちらが勝つかが変わり、レベルの検査もすり抜ける
            bail!("{key}: CSV の列名 `{n}` は組み込みの変数と同じで、どちらを指すか決まらない。CSV の列名を変えてほしい");
        }
        if in_csv {
            continue;
        }
        if builtin && !allowed.contains(&n) {
            // 患者のタグが画像番号で変わると、同じ患者の画像で値がばらつく
            bail!("{key} は{}レベルのタグなので、変数 `{n}` は使えない(使える変数: {})", level.label(), allowed.join(" "));
        }
        if !builtin {
            bail!("{key}: 変数 `{n}` がない(CSV の列名か {})", allowed.join(" "));
        }
    }
    Ok(Rule { tag, name, vr, tpl, value: spec.value.clone(), table, level })
}

/// 元のディレクトリにあるファイル(再帰)。DICOM でないものや DICOMDIR も入る。読むときに飛ばして、ログに出す。
pub fn list_sources(input: &str) -> Result<Vec<PathBuf>> {
    let input = input.trim();
    if input.is_empty() {
        bail!("元のディレクトリを指定してほしい");
    }
    if !Path::new(input).is_dir() {
        bail!("元のディレクトリがない: {input}");
    }
    Ok(WalkDir::new(input)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect())
}

fn cached_table(cache: &mut TableCache, path: &str, enc: CsvEncoding) -> Result<Arc<Table>> {
    let key = (path.to_string(), enc);
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some((cached_mtime, t)) = cache.get(&key) {
        if mtime.is_some() && *cached_mtime == mtime {
            return Ok(t.clone());
        }
    }
    let (columns, rows) = load_csv(Path::new(path), enc)?;
    let t = Arc::new(Table { columns, rows });
    cache.insert(key, (mtime, t.clone()));
    Ok(t)
}

/// 規則のタグの書き方(`PatientName`、`(0010,0010)`、`(0029,1010)/LO`)からタグだけを取り出す。
/// 画面が、既定の規則が上書きされたかを、書き方の違いによらず判定するために使う。
pub fn tag_of(key: &str) -> Option<Tag> {
    let key = key.trim().split('/').next()?.trim();
    StandardDataDictionary.by_expr(key).map(|e| e.tag()).or_else(|| key.parse::<Tag>().ok())
}

fn load_csv(path: &Path, enc: CsvEncoding) -> Result<(Vec<String>, Vec<Row>)> {
    let bytes = std::fs::read(path).with_context(|| format!("CSV を開けない: {}", path.display()))?;
    let text = match enc {
        CsvEncoding::Utf8 => String::from_utf8(bytes).context("CSV が UTF-8 として読めない")?,
        CsvEncoding::ShiftJis => encoding_rs::SHIFT_JIS.decode_without_bom_handling(&bytes).0.into_owned(),
        // Excel の「CSV」は Shift_JIS。UTF-8 として読めればそちらを優先する
        CsvEncoding::Auto => match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => encoding_rs::SHIFT_JIS.decode_without_bom_handling(e.as_bytes()).0.into_owned(),
        },
    };
    let text = text.trim_start_matches('\u{feff}');
    let mut rd = csv::ReaderBuilder::new().trim(csv::Trim::All).from_reader(text.as_bytes());
    let head: Vec<String> = rd.headers()?.iter().map(str::to_string).collect();
    let mut rows = Vec::new();
    for rec in rd.records() {
        let rec = rec?;
        rows.push(head.iter().cloned().zip(rec.iter().map(str::to_string)).collect());
    }
    if rows.is_empty() {
        bail!("CSV にデータ行がない: {}", path.display());
    }
    Ok((head, rows))
}

/// 触れない(Keep)で、値が元ファイルの文字セット(複数値でないもの)で書けるか。
/// 書けない値を dicom-rs に渡すと、書き込みの途中で失敗して「なぜ」が分からないため、先に調べる。
fn check_keep(vr: VR, text: &str, code: &str, cs: Option<&SpecificCharacterSet>) -> Result<()> {
    if !is_text_vr(vr) || text.is_ascii() {
        return Ok(());
    }
    let hint = "文字コードを utf8 か iso2022jp にすると書ける";
    let first_non_ascii = text.chars().find(|c| !c.is_ascii()).unwrap_or('?');
    // 指定なし(ISO_IR 6)は ASCII だけ。dicom-rs の既定の符号化器は ISO 8859-1 で ü などを通してしまうが、
    // 文字セットの指定がないファイルに 0x80 以上を書くのは規格にない
    if code == "ISO_IR 6" {
        bail!("元ファイルには文字セットの指定がなく(ASCII)、`{first_non_ascii}` (U+{:04X}) は書けない。{hint}", first_non_ascii as u32);
    }
    // ISO_IR 13 は JIS X 0201(半角カナまで)。dicom-rs の符号化器は Shift_JIS なので漢字も通してしまうが、規格にない
    if code == "ISO_IR 13" {
        if let Some(c) = text.chars().find(|c| !c.is_ascii() && !('\u{FF61}'..='\u{FF9F}').contains(c)) {
            bail!("元ファイルの文字セット `{code}` は半角カナまでで、`{c}` (U+{:04X}) は書けない。{hint}", c as u32);
        }
        return Ok(());
    }
    if let Some(cs) = cs {
        if cs.encode(text).is_err() {
            let c = text.chars().find(|c| cs.encode(&c.to_string()).is_err()).unwrap_or('?');
            bail!("元ファイルの文字セット `{code}` では `{c}` (U+{:04X}) が書けない。{hint}", c as u32);
        }
    }
    Ok(())
}

/// 値をその VR で書けるか。数値の VR は数として読めること、日本語を自前で符号化するときは符号化できること。
fn check_writable(vr: VR, text: &str, flavor: Option<&Iso2022>) -> Result<()> {
    if matches!(vr, VR::US | VR::UL | VR::SS | VR::SL | VR::FL | VR::FD) {
        text.trim().parse::<f64>().map(|_| ()).map_err(|_| anyhow::anyhow!("{vr} は数値が要る"))?;
    }
    if let Some(f) = flavor {
        if is_text_vr(vr) && !text.is_ascii() {
            f.encode(text)?;
        }
    }
    Ok(())
}

/// 値が DICOM の VR の決まりに収まっているか。収まらなくても書き出しは止めず、見本で知らせる。
pub fn check_value(vr: VR, v: &str) -> Vec<String> {
    let mut w = Vec::new();
    let chars = v.chars().count();
    let mut limit = |name: &str, max: usize| {
        if chars > max {
            w.push(format!("{name} は {max} 文字まで(いま {chars})"));
        }
    };
    match vr {
        VR::LO => limit("LO", 64),
        VR::SH => limit("SH", 16),
        VR::CS => limit("CS", 16),
        VR::AE => limit("AE", 16),
        VR::DS => limit("DS", 16),
        VR::IS => limit("IS", 12),
        VR::UI => limit("UI", 64),
        VR::PN => {
            let groups: Vec<&str> = v.split('=').collect();
            if groups.len() > 3 {
                w.push("PN の成分グループは 3 つまで(字形=表意=表音)".into());
            }
            for g in groups {
                if g.chars().count() > 64 {
                    w.push("PN の 1 グループは 64 文字まで".into());
                }
                if g.split('^').count() > 5 {
                    w.push("PN の成分(^)は 5 つまで".into());
                }
            }
        }
        _ => {}
    }
    if vr == VR::CS && !v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ' ' || c == '_') {
        w.push("CS は大文字・数字・空白・_ だけ".into());
    }
    if vr == VR::DA && !(v.len() == 8 && v.chars().all(|c| c.is_ascii_digit())) {
        w.push("DA は YYYYMMDD".into());
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_levels_follow_the_dicom_modules() {
        assert_eq!(standard_level(tags::PATIENT_NAME), Level::Patient);
        assert_eq!(standard_level(tags::PATIENT_WEIGHT), Level::Study);
        assert_eq!(standard_level(tags::STUDY_DESCRIPTION), Level::Study);
        assert_eq!(standard_level(tags::SERIES_NUMBER), Level::Series);
        assert_eq!(standard_level(tags::INSTANCE_NUMBER), Level::Image);
    }

    #[test]
    fn a_coarse_tag_cannot_use_a_finer_variable() {
        let spec = RuleSpec::new("PatientName", Level::Patient, "X%(i)d");
        let err = parse_rule(&spec, None).err().unwrap().to_string();
        assert!(err.contains("患者レベル") && err.contains('i'), "{err}");
        assert!(parse_rule(&RuleSpec::new("InstanceNumber", Level::Image, "%(i)d"), None).is_ok());
    }

    #[test]
    fn csv_rows_advance_per_the_level_of_the_tag() {
        let table = Arc::new(Table {
            columns: vec!["v".into()],
            rows: (0..100).map(|k| [("v".to_string(), k.to_string())].into()).collect(),
        });
        let cfg = Config { patients: 3, studies: 2, series: 1, images: 1, ..Config::default() };
        let rule = |level| parse_rule(&RuleSpec::new("StudyDescription", level, "%(v)s"), Some(table.clone())).unwrap();
        let eng = Engine::new(&cfg).unwrap();
        let at = |r: &Rule, p, s| eng.csv_row(r, Idx { p, s, r: 0, i: 0 }, &Place::default()).unwrap()["v"].clone();
        assert_eq!((at(&rule(Level::Patient), 2, 1), at(&rule(Level::Patient), 2, 0)), ("2".into(), "2".into()));
        assert_eq!((at(&rule(Level::Study), 2, 1), at(&rule(Level::Study), 0, 1)), ("5".into(), "1".into()));
        // 元の 2 人目の患者は、1 人目の複製と別の行を読む
        let second = Place { patient: 1, ..Place::default() };
        let at2 = |p| eng.csv_row(&rule(Level::Patient), Idx { p, s: 0, r: 0, i: 0 }, &second).unwrap()["v"].clone();
        assert_eq!((at2(0), at2(2)), ("3".into(), "5".into()));
    }

    #[test]
    fn source_patients_are_told_apart_by_the_default_rules() {
        let eng = Engine::new(&Config::default()).unwrap();
        let ix = Idx { p: 0, s: 0, r: 0, i: 0 };
        let first = Place::default();
        let second = Place { patient: 1, ordinal: 7, ..Place::default() };
        assert_eq!(eng.render_rule(tags::PATIENT_ID, ix, &first).unwrap().unwrap(), "EVAL000-0000");
        assert_eq!(eng.render_rule(tags::PATIENT_ID, ix, &second).unwrap().unwrap(), "EVAL001-0000");
        assert_eq!((eng.serial_number(ix, &first), eng.serial_number(ix, &second)), (1, 8));
    }

    #[test]
    fn source_values_are_available_as_string_variables() {
        let place = Place { patient_id: "1001".into(), modality: "CT".into(), ..Place::default() };
        let rule = parse_rule(&RuleSpec::new("PatientID", Level::Patient, "%(src_patient_id)s-%(p1)d"), None).unwrap();
        let eng = Engine::new(&Config::default()).unwrap();
        let ints = eng.ints(Idx { p: 1, s: 0, r: 0, i: 0 }, &place, 1);
        let strs = Engine::strs(&place);
        assert_eq!(rule.tpl.render(&Vars { ints: &ints, strs: &strs, csv: None }).unwrap(), "1001-2");
        // シリーズの値は患者レベルでは使えない
        let err = parse_rule(&RuleSpec::new("PatientID", Level::Patient, "%(src_modality)s"), None).err().unwrap().to_string();
        assert!(err.contains("患者レベル"), "{err}");
    }

    #[test]
    fn a_csv_column_named_like_a_builtin_variable_is_rejected() {
        let table = Arc::new(Table { columns: vec!["n".into()], rows: vec![[("n".to_string(), "x".to_string())].into()] });
        let err = parse_rule(&RuleSpec::new("PatientID", Level::Patient, "%(n)d"), Some(table)).err().unwrap().to_string();
        assert!(err.contains("列名") && err.contains('n'), "{err}");
    }

    #[test]
    fn keep_rejects_non_ascii_when_the_source_declares_no_character_set() {
        let cs = SpecificCharacterSet::from_code("ISO_IR 6");
        let err = check_keep(VR::PN, "Müller", "ISO_IR 6", cs.as_ref()).unwrap_err().to_string();
        assert!(err.contains("ASCII") && err.contains('ü'), "{err}");
        assert!(check_keep(VR::PN, "Mueller", "ISO_IR 6", cs.as_ref()).is_ok());
        let latin = SpecificCharacterSet::from_code("ISO_IR 100");
        assert!(check_keep(VR::PN, "Müller", "ISO_IR 100", latin.as_ref()).is_ok());
    }

    #[test]
    fn csv_line_numbers_count_the_header_and_wrap_around() {
        let table = Arc::new(Table {
            columns: vec!["v".into()],
            rows: (0..3).map(|k| [("v".to_string(), k.to_string())].into()).collect(),
        });
        let cfg = Config { patients: 5, ..Config::default() };
        let eng = Engine::new(&cfg).unwrap();
        let rule = parse_rule(&RuleSpec::new("PatientName", Level::Patient, "%(v)s"), Some(table.clone())).unwrap();
        let line = |p| eng.csv_line(&rule, Idx { p, s: 0, r: 0, i: 0 }, &Place::default());
        assert_eq!((line(0), line(2), line(3)), (Some(2), Some(4), Some(2)));
        let no_csv = parse_rule(&RuleSpec::new("PatientID", Level::Patient, "%(p)d"), Some(table)).unwrap();
        assert_eq!(eng.csv_line(&no_csv, Idx { p: 0, s: 0, r: 0, i: 0 }, &Place::default()), None);
    }

    #[test]
    fn a_zero_count_is_refused_before_anything_runs() {
        let err = Engine::new(&Config { patients: 0, ..Config::default() }).err().unwrap().to_string();
        assert!(err.contains("1 以上"), "{err}");
    }

    #[test]
    fn tag_of_accepts_every_spelling_of_a_rule_tag() {
        assert_eq!(tag_of("PatientName"), Some(tags::PATIENT_NAME));
        assert_eq!(tag_of("(0010,0010)"), Some(tags::PATIENT_NAME));
        assert_eq!(tag_of("(0029,1010)/LO"), Some(Tag(0x0029, 0x1010)));
        assert_eq!(tag_of("NoSuchTag"), None);
    }

    #[test]
    fn cs_must_be_uppercase() {
        assert!(check_value(VR::CS, "M").is_empty());
        assert!(!check_value(VR::CS, "male").is_empty());
    }

    #[test]
    fn long_names_are_flagged() {
        assert!(!check_value(VR::LO, &"あ".repeat(65)).is_empty());
        assert!(check_value(VR::LO, &"あ".repeat(64)).is_empty());
        assert!(!check_value(VR::PN, "a=b=c=d").is_empty());
    }
}
