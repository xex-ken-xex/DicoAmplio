//! 水増し本体。元の各ファイルを 1 回だけ読み込み、(患者, 検査, シリーズ, 画像) の組ごとに
//! 複製して UID と識別タグを振り直し、rayon で並列に書き出す。

use crate::charset::{is_multi_iso2022, scs_values, Charset, Iso2022};
use crate::config::{Config, CsvEncoding, Level, RuleSpec};
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

/// 複製の番号。UID はこれと元の UID から決定的に作る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Idx {
    pub p: u32,
    pub s: u32,
    pub r: u32,
    pub i: u32,
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
}

impl Progress {
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

    /// 書き出した数を返す。中断したときも、そこまでの数を返す。
    pub fn run(&self, progress: &Progress) -> Result<u64> {
        if self.cfg.output.trim().is_empty() {
            bail!("出力先を指定してほしい");
        }
        self.preflight()?;
        let files = self.sources()?;
        // 元の木を先に読む。元の患者の番号などを規則の変数にするため
        let tree = tree::scan(&files, &AtomicBool::new(false), &AtomicU64::new(0));
        let c = tree.counts();
        progress.total.store(self.planned(tree.files), Ordering::Relaxed);
        progress.say(format!(
            "元: {} 患者 / {} 検査 / {} シリーズ / {} 画像({} ファイル) → 出力 {} 個",
            c.patients,
            c.studies,
            c.series,
            c.images,
            tree.files,
            progress.total.load(Ordering::Relaxed)
        ));
        for s in &tree.skipped {
            progress.say(format!("飛ばした: {s}"));
        }
        let result: Result<Vec<u64>> = files
            .par_iter()
            .filter_map(|f| tree.places.get(f).map(|place| (f, place)))
            .map(|(f, place)| self.process(f, place, progress))
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

    fn process(&self, path: &Path, place: &Place, progress: &Progress) -> Result<u64> {
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
                return Ok(0);
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

        let mut combos = Vec::new();
        for p in 0..c.patients {
            for s in 0..c.studies {
                for r in 0..c.series {
                    for i in 0..c.images {
                        combos.push(Idx { p, s, r, i });
                    }
                }
            }
        }

        combos.par_iter().try_for_each(|&ix| -> Result<()> {
            if progress.cancel.load(Ordering::Relaxed) {
                bail!(CANCELLED);
            }
            let mut ds = src.clone();
            self.rewrite_uids(&mut ds, ix);

            if let Some(code) = c.charset.code() {
                ds.put_str(tags::SPECIFIC_CHARACTER_SET, VR::CS, code);
            }
            let ints = self.ints(ix, place, src_inst_no);
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
                if let Some((code, cs)) = &keep_check {
                    check_keep(vr, &text, code, cs.as_ref()).with_context(|| format!("{} {} ({})", rule.name, rule.tag, path.display()))?;
                }
                put_value(&mut ds, rule.tag, vr, &text, flavor.as_ref())
                    .with_context(|| format!("{} {} ({})", rule.name, rule.tag, path.display()))?;
            }

            let sop = ds
                .get(tags::SOP_INSTANCE_UID)
                .map(|e| e.to_str().map(|s| s.trim_end_matches('\0').to_string()))
                .transpose()?
                .context("SOPInstanceUID がない")?;
            ds.meta_mut().media_storage_sop_instance_uid = sop.clone();
            ds.meta_mut().update_information_group_length();

            let dir = Path::new(c.output.trim())
                .join(format!("P{:06}", ix.p))
                .join(format!("S{:03}", ix.s))
                .join(format!("R{:03}", ix.r));
            std::fs::create_dir_all(&dir)?;
            // ファイルへ直接書くと、符号化に失敗したとき途中で切れた壊れたファイルが残る
            let mut buf = Vec::new();
            ds.write_all(&mut buf).with_context(|| path.display().to_string())?;
            std::fs::write(dir.join(format!("{sop}.dcm")), buf)?;
            progress.done.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;
        Ok(combos.len() as u64)
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

/// 元のディレクトリにあるファイル(再帰。DICOMDIR は除く)。
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
        .filter(|e| e.file_type().is_file() && !e.file_name().to_string_lossy().eq_ignore_ascii_case("DICOMDIR"))
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
