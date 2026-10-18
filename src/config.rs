//! 水増しの設定。画面とコマンドラインで共通で、プリセットとして TOML に保存する。

use crate::charset::Charset;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// DICOM の階層。タグはどれかのレベルの属性で、値はそのレベルの 1 つにつき 1 つ決まる
/// (患者のタグは患者ごと、画像のタグは画像ごと)。CSV の行も、そのレベルの単位で進める。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Patient,
    Study,
    Series,
    Image,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::Patient, Level::Study, Level::Series, Level::Image];

    pub fn label(self) -> &'static str {
        match self {
            Level::Patient => "患者",
            Level::Study => "検査",
            Level::Series => "シリーズ",
            Level::Image => "画像",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "patient" | "患者" => Some(Level::Patient),
            "study" | "検査" => Some(Level::Study),
            "series" | "シリーズ" => Some(Level::Series),
            "image" | "画像" => Some(Level::Image),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CsvEncoding {
    /// UTF-8 として読め(BOM 付きも可)なければ Shift_JIS(Excel の「CSV」)として読む
    #[default]
    Auto,
    Utf8,
    ShiftJis,
}

impl CsvEncoding {
    pub const ALL: [CsvEncoding; 3] = [CsvEncoding::Auto, CsvEncoding::Utf8, CsvEncoding::ShiftJis];

    pub fn label(self) -> &'static str {
        match self {
            CsvEncoding::Auto => "自動(UTF-8 → Shift_JIS)",
            CsvEncoding::Utf8 => "UTF-8",
            CsvEncoding::ShiftJis => "Shift_JIS",
        }
    }
}

/// `タグ = テンプレート` の 1 本。
///
/// レベルは必須。元の 1 組を n 倍にするとき、そのタグの値がどの単位で変わるかを決めるもので、
/// タグ名から推測して補わない(推測が外れると、同じ患者の中で値がばらつくデータができる)。
/// CSV・文字コードは、このタグだけの指定(なければ全体の設定を使う)。
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RuleSpec {
    pub tag: String,
    pub level: Level,
    pub value: String,
    /// このタグだけが読む CSV。空なら全体の CSV
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub csv: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csv_encoding: Option<CsvEncoding>,
}

impl RuleSpec {
    pub fn new(tag: &str, level: Level, value: &str) -> RuleSpec {
        RuleSpec { tag: tag.into(), level, value: value.into(), csv: String::new(), csv_encoding: None }
    }

    /// `タグ:レベル[@CSVのパス]=テンプレート`(レベルは patient / study / series / image)。
    /// パスに `=` を含む CSV は、この書き方では指定できない(プリセットに書く)。
    pub fn parse(spec: &str) -> Result<RuleSpec> {
        let (key, value) = spec
            .split_once('=')
            .with_context(|| format!("`タグ:レベル=テンプレート` の形でない: {spec}"))?;
        let (head, csv) = key.split_once('@').unwrap_or((key, ""));
        let (tag, level) = head
            .split_once(':')
            .with_context(|| format!("レベルが要る(patient / study / series / image)。例: PatientName:patient=…  ({spec})"))?;
        let level = Level::parse(level).with_context(|| format!("レベルは patient / study / series / image: {level}"))?;
        Ok(RuleSpec { csv: csv.trim().to_string(), ..RuleSpec::new(tag.trim(), level, value) })
    }
}

/// 1 つのアソシエーションで送るまとまり。実機の装置は検査(モダリティ)の単位で送るので、
/// 混ぜて送ると PACS 側で取り違えが起きやすい。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AssocUnit {
    /// モダリティごと(CT だけ、MR だけ、で 1 本)
    #[default]
    Modality,
    /// 複製後の検査ごと
    Study,
    /// 複製後のシリーズごと
    Series,
    /// 複製後の患者ごと
    Patient,
    /// 分けない(接続の数に均等に割る)
    All,
}

impl AssocUnit {
    pub const ALL: [AssocUnit; 5] = [AssocUnit::Modality, AssocUnit::Study, AssocUnit::Series, AssocUnit::Patient, AssocUnit::All];

    pub fn label(self) -> &'static str {
        match self {
            AssocUnit::Modality => "モダリティごと",
            AssocUnit::Study => "検査ごと",
            AssocUnit::Series => "シリーズごと",
            AssocUnit::Patient => "患者ごと",
            AssocUnit::All => "分けない",
        }
    }
}

/// C-STORE の送り先。`enabled` のときは、複製をディスクに書かずにそのまま送る(`also_write` なら両方)。
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub called_ae: String,
    pub calling_ae: String,
    /// 同時に張るアソシエーションの数
    pub connections: u32,
    /// 1 つのアソシエーションで送るまとまり
    pub unit: AssocUnit,
    pub also_write: bool,
    /// 水増しせず、`as_is_dir` の DICOM をそのまま送る
    pub as_is: bool,
    pub as_is_dir: String,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            enabled: false,
            host: "127.0.0.1".into(),
            port: 104,
            called_ae: "ANY-SCP".into(),
            calling_ae: "DICOAMPLIO".into(),
            connections: 4,
            unit: AssocUnit::default(),
            also_write: false,
            as_is: false,
            as_is_dir: String::new(),
        }
    }
}

/// 送り先の帳簿の 1 件。
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Destination {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub called_ae: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DestinationBook {
    #[serde(rename = "destination")]
    destinations: Vec<Destination>,
}

pub const DESTINATIONS_FILE: &str = "destinations.toml";

/// 帳簿を読む。ファイルがなければ空。
pub fn load_destinations(dir: &Path) -> Result<Vec<Destination>> {
    let path = dir.join(DESTINATIONS_FILE);
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("読めない: {}", path.display()))?;
    let book: DestinationBook = toml::from_str(&text).with_context(|| format!("送り先の帳簿の書式が違う: {}", path.display()))?;
    Ok(book.destinations)
}

pub fn save_destinations(dir: &Path, destinations: &[Destination]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let book = DestinationBook { destinations: destinations.to_vec() };
    let path = dir.join(DESTINATIONS_FILE);
    std::fs::write(&path, toml::to_string_pretty(&book)?).with_context(|| format!("書けない: {}", path.display()))
}

impl StoreConfig {
    /// `AET@host:port` の形から。AET は省略できる(`host:port`)。
    pub fn parse_target(&mut self, s: &str) -> Result<()> {
        let (ae, addr) = match s.split_once('@') {
            Some((ae, addr)) => (Some(ae.trim()), addr.trim()),
            None => (None, s.trim()),
        };
        let (host, port) = addr.rsplit_once(':').with_context(|| format!("送り先は AET@host:port の形: {s}"))?;
        if host.is_empty() {
            bail!("送り先のホストが空: {s}");
        }
        self.port = port.parse().with_context(|| format!("送り先のポートが数でない: {port}"))?;
        self.host = host.to_string();
        if let Some(ae) = ae.filter(|a| !a.is_empty()) {
            self.called_ae = ae.to_string();
        }
        self.enabled = true;
        Ok(())
    }

    pub fn target(&self) -> String {
        format!("{}@{}:{}", self.called_ae, self.host, self.port)
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub input: String,
    pub output: String,
    pub patients: u32,
    pub studies: u32,
    pub series: u32,
    pub images: u32,
    pub charset: Charset,
    pub uid_root: String,
    pub csv: String,
    pub csv_encoding: CsvEncoding,
    pub store: StoreConfig,
    #[serde(rename = "rule")]
    pub rules: Vec<RuleSpec>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            input: String::new(),
            output: String::new(),
            patients: 1,
            studies: 1,
            series: 1,
            images: 1,
            charset: Charset::default(),
            uid_root: "2.25".into(),
            csv: String::new(),
            csv_encoding: CsvEncoding::default(),
            store: StoreConfig::default(),
            rules: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("読めない: {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("プリセットの書式が違う: {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?).with_context(|| format!("書けない: {}", path.display()))
    }
}

/// プリセットの置き場。実行ファイルの隣の `presets/`。持ち運んでも設定が付いてくるように、
/// OS の設定フォルダではなくここに置く。
pub fn default_preset_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("presets")))
        .unwrap_or_else(|| PathBuf::from("presets"))
}

/// 名前で探すときのファイル名。パスの区切りなどが入った名前は受けつけない。
pub fn preset_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
        bail!("プリセット名に使えない文字がある: `{name}`");
    }
    Ok(dir.join(format!("{name}.toml")))
}

/// 直前の設定を覚えておくファイル名。一覧には出さない。
pub const LAST_NAME: &str = "_last";

pub fn list_presets(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension()? != "toml" {
                return None;
            }
            Some(p.file_stem()?.to_string_lossy().into_owned())
        })
        .filter(|n| n != LAST_NAME)
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_toml_with_japanese_rules() {
        let c = Config {
            patients: 100,
            charset: Charset::Iso2022jp,
            csv_encoding: CsvEncoding::ShiftJis,
            rules: vec![RuleSpec {
                csv: "names.csv".into(),
                csv_encoding: Some(CsvEncoding::ShiftJis),
                ..RuleSpec::new("PatientName", Level::Patient, "%(name_alpha)s=%(name_kanji)s=%(name_kana)s")
            }],
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("[[rule]]"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let c: Config = toml::from_str("patients = 5\n").unwrap();
        assert_eq!(c.patients, 5);
        assert_eq!(c.images, 1);
        assert_eq!(c.charset, Charset::Utf8);
    }

    #[test]
    fn preset_names_cannot_escape_the_directory() {
        let d = Path::new("presets");
        assert!(preset_path(d, "../x").is_err());
        assert!(preset_path(d, "a/b").is_err());
        assert!(preset_path(d, ".hidden").is_err());
        assert_eq!(preset_path(d, "日本人患者").unwrap(), d.join("日本人患者.toml"));
    }

    #[test]
    fn destination_book_roundtrips_and_is_empty_when_missing() {
        let dir = std::env::temp_dir().join(format!("dicoamplio-dest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_destinations(&dir).unwrap().is_empty());
        let list = vec![Destination { name: "評価 PACS".into(), host: "10.0.0.5".into(), port: 11112, called_ae: "PACS".into() }];
        save_destinations(&dir, &list).unwrap();
        assert_eq!(load_destinations(&dir).unwrap(), list);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_target_parses_ae_title_host_and_port() {
        let mut s = StoreConfig::default();
        s.parse_target("PACS@10.0.0.5:11112").unwrap();
        assert_eq!((s.called_ae.as_str(), s.host.as_str(), s.port, s.enabled), ("PACS", "10.0.0.5", 11112, true));
        let mut s = StoreConfig::default();
        s.parse_target("localhost:104").unwrap();
        assert_eq!((s.called_ae.as_str(), s.host.as_str()), ("ANY-SCP", "localhost"));
        assert!(StoreConfig::default().parse_target("PACS@host").is_err());
        assert!(StoreConfig::default().parse_target("PACS@host:abc").is_err());
    }

    #[test]
    fn rule_spec_splits_at_the_first_equals_only() {
        let r = RuleSpec::parse("PatientName:patient=%(a)s=%(b)s").unwrap();
        assert_eq!((r.tag.as_str(), r.value.as_str()), ("PatientName", "%(a)s=%(b)s"));
    }

    #[test]
    fn rule_spec_takes_a_level_and_a_csv() {
        let r = RuleSpec::parse("PatientName:study@/data/names.csv=%(name)s").unwrap();
        assert_eq!((r.tag.as_str(), r.csv.as_str(), r.value.as_str()), ("PatientName", "/data/names.csv", "%(name)s"));
        assert_eq!(r.level, Level::Study);
        assert!(RuleSpec::parse("PatientSex:universe=x").is_err());
    }

    #[test]
    fn the_level_is_required_and_never_guessed() {
        assert!(RuleSpec::parse("PatientSex=%(sex)s").is_err());
        let err = toml::from_str::<Config>("[[rule]]\ntag = \"PatientSex\"\nvalue = \"M\"\n").unwrap_err();
        assert!(err.to_string().contains("level"), "{err}");
    }

    #[test]
    fn rule_without_its_own_csv_writes_only_tag_and_value() {
        let c = Config { rules: vec![RuleSpec::new("StudyID", Level::Study, "%(s1)d")], ..Config::default() };
        let text = toml::to_string_pretty(&c).unwrap();
        let rule = &text[text.find("[[rule]]").unwrap()..];
        assert!(rule.contains("level = \"study\"") && !rule.contains("csv"), "{text}");
    }
}
