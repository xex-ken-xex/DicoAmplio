//! DICOM のタグを書き換えて、患者・検査・シリーズ・画像を水増しする。
//! 引数なしで起動すると画面(GUI)が開く。引数があればコマンドラインで動く。
//!
//! Windows では GUI サブシステムで作る(ダブルクリックで黒いコンソールの窓が出ないように)。
//! コマンドラインで使うときは、起動元のコンソールにつなぎ直して出力する(`attach_console`)。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod charset;
mod config;
mod engine;
#[cfg(feature = "gui")]
mod fonts;
#[cfg(feature = "gui")]
mod gui;
mod store;
mod template;
mod tree;

use anyhow::{Context, Result};
use charset::Charset;
use clap::{CommandFactory, Parser};
use config::{Config, CsvEncoding, RuleSpec};
use engine::{Engine, Progress};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    about = "DICOM のタグを書き換えて患者/検査/シリーズ/画像を水増しする(引数なしで画面が開く)",
    after_help = "\
レベル: 元の 1 組を n 倍にするとき、そのタグの値がどの単位で変わるか。必須。
  patient = 患者ごと、study = 検査ごと、series = シリーズごと、image = 画像ごと。
  CSV の行も、そのタグのレベルの単位で 1 行ずつ進む(行数を超えたら先頭へ戻る)。
テンプレートの変数(整数): p s r i = 患者/検査/シリーズ/画像の番号(0 始まり)、
  p1 s1 r1 i1 = 同(1 始まり)、n = 通し番号(1 始まり)、
  inst = 元の InstanceNumber + 画像番号 × 10000。
  細かいレベルの番号は粗いレベルのタグには使えない(patient のタグでは p p1 だけ)。
CSV を指定すると、列名がそのまま変数(文字列)になる。
例: --csv people.csv \\
    --set 'PatientName:patient=%(name)s' --set 'PatientSex:patient=%(sex)s' \\
    --set 'PatientWeight:patient=%(weight).1f' --set 'StudyDescription:study=CT %(s1)02d'"
)]
struct Args {
    /// 元の画像セットのディレクトリ(再帰的に探す)
    input: Option<String>,
    /// 出力先ディレクトリ(--send だけのときは省略できる)
    output: Option<String>,
    /// C-STORE で送る先 `AET@host:port`。指定すると、ディスクに書かずにメモリから直接送る
    #[arg(long, value_name = "AET@HOST:PORT")]
    send: Option<String>,
    /// 送るときの自分の AE タイトル
    #[arg(long)]
    calling_ae: Option<String>,
    /// 同時に張るアソシエーションの数
    #[arg(long)]
    connections: Option<u32>,
    /// 1 つのアソシエーションで送るまとまり(既定はモダリティごと)
    #[arg(long, value_enum)]
    assoc_unit: Option<config::AssocUnit>,
    /// 送るのに加えて、出力先にも書く
    #[arg(long)]
    also_write: bool,
    /// C-ECHO で送り先との接続を試して終わる(--send と一緒に)
    #[arg(long)]
    echo: bool,
    /// 水増しせず、このディレクトリの DICOM をそのまま送る(--send と一緒に。元のディレクトリは要らない)
    #[arg(long, value_name = "DIR")]
    send_only: Option<String>,
    /// 患者の複製数(元を 1 組とした総数)
    #[arg(long)]
    patients: Option<u32>,
    /// 患者あたりの検査の複製数
    #[arg(long)]
    studies: Option<u32>,
    /// 検査あたりのシリーズの複製数
    #[arg(long)]
    series: Option<u32>,
    /// シリーズあたりの画像の複製数(元の各画像をこの数に増やす)
    #[arg(long)]
    images: Option<u32>,
    /// タグの規則 `タグ:レベル=テンプレート`(タグは PatientName か (0010,0010)、レベルは patient / study / series / image)。
    /// そのタグ専用の CSV は `タグ:レベル@CSV=テンプレート`。複数指定可
    #[arg(long = "set", value_name = "TAG:LEVEL[@CSV]=TEMPLATE")]
    sets: Vec<String>,
    /// 規則のファイル。1 行に 1 つ `タグ:レベル=テンプレート`、`#` で始まる行は注釈
    #[arg(long)]
    rules: Option<PathBuf>,
    /// 日本語などの文字コード。utf8 = ISO_IR 192、iso2022jp = ISO 2022 IR 13\ISO 2022 IR 87、keep = 触れない
    #[arg(long, value_enum)]
    charset: Option<Charset>,
    /// 差し込む CSV(1 行目が列名)
    #[arg(long)]
    csv: Option<String>,
    /// CSV の文字コード
    #[arg(long, value_enum)]
    csv_encoding: Option<CsvEncoding>,
    /// UID の接頭辞(ルート)。末尾のドットは不要
    #[arg(long)]
    uid_root: Option<String>,
    /// プリセットを読む(名前か、.toml のパス)
    #[arg(long)]
    preset: Option<String>,
    /// いまの設定をプリセットに保存する(名前か、.toml のパス)
    #[arg(long)]
    save_preset: Option<String>,
    /// プリセットの置き場(既定は実行ファイルの隣の presets/)
    #[arg(long)]
    preset_dir: Option<PathBuf>,
    /// プリセットの一覧を出して終わる
    #[arg(long)]
    list_presets: bool,
    /// 画面を開く
    #[arg(long)]
    gui: bool,
    /// スレッド数(0 で自動)
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

/// 名前なら置き場の `名前.toml`、パスなら、そのファイル。
fn resolve_preset(arg: &str, dir: &Path) -> Result<PathBuf> {
    if arg.ends_with(".toml") || arg.contains(['/', '\\']) {
        Ok(PathBuf::from(arg))
    } else {
        config::preset_path(dir, arg)
    }
}

fn build_config(a: &Args, dir: &Path) -> Result<Config> {
    let mut c = match &a.preset {
        Some(p) => Config::load(&resolve_preset(p, dir)?)?,
        None => Config::default(),
    };
    if let Some(v) = &a.input {
        c.input = v.clone();
    }
    if let Some(v) = &a.output {
        c.output = v.clone();
    }
    c.patients = a.patients.unwrap_or(c.patients);
    c.studies = a.studies.unwrap_or(c.studies);
    c.series = a.series.unwrap_or(c.series);
    c.images = a.images.unwrap_or(c.images);
    c.charset = a.charset.unwrap_or(c.charset);
    c.csv_encoding = a.csv_encoding.unwrap_or(c.csv_encoding);
    if let Some(v) = &a.csv {
        c.csv = v.clone();
    }
    if let Some(v) = &a.uid_root {
        c.uid_root = v.clone();
    }
    if let Some(t) = &a.send {
        c.store.parse_target(t)?;
    }
    if let Some(v) = &a.calling_ae {
        c.store.calling_ae = v.clone();
    }
    if let Some(v) = a.connections {
        c.store.connections = v.max(1);
    }
    if let Some(u) = a.assoc_unit {
        c.store.unit = u;
    }
    if a.also_write {
        c.store.also_write = true;
    }
    if let Some(d) = &a.send_only {
        c.store.as_is = true;
        c.store.as_is_dir = d.clone();
    }
    if let Some(f) = &a.rules {
        let text = std::fs::read_to_string(f).with_context(|| format!("規則ファイルを読めない: {}", f.display()))?;
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            c.rules.push(RuleSpec::parse(l)?);
        }
    }
    for s in &a.sets {
        c.rules.push(RuleSpec::parse(s)?);
    }
    Ok(c)
}

/// Windows の GUI サブシステムは、コンソールから起動されても標準出力・標準エラーがつながっていない。
/// 引数があるとき(コマンドラインの使い方)だけ、起動元のコンソールにつなぐ。ダブルクリックでは何もしない。
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: 引数なしの Win32 API。親にコンソールがなければ失敗して返るだけ
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() -> Result<()> {
    if std::env::args().len() > 1 {
        attach_console();
    }
    let args = Args::parse();
    let dir = args.preset_dir.clone().unwrap_or_else(config::default_preset_dir);

    if args.list_presets {
        for n in config::list_presets(&dir) {
            println!("{n}");
        }
        return Ok(());
    }
    if std::env::args().len() == 1 || args.gui {
        return open_gui(args, dir);
    }

    let cfg = build_config(&args, &dir)?;
    if let Some(p) = &args.save_preset {
        let path = resolve_preset(p, &dir)?;
        cfg.save(&path)?;
        eprintln!("プリセットを保存した: {}", path.display());
    }
    if args.echo {
        if !cfg.store.enabled {
            anyhow::bail!("--echo には --send AET@host:port が要る");
        }
        eprintln!("{}", store::echo(&cfg.store)?);
        return Ok(());
    }
    if cfg.store.as_is {
        if !cfg.store.enabled {
            anyhow::bail!("--send-only には --send AET@host:port が要る");
        }
        let progress = Progress { echo: true, ..Default::default() };
        let sent = Engine::new(&cfg)?.run_send_as_is(&progress)?;
        eprintln!("送信 {sent} 個");
        return Ok(());
    }
    let needs_output = !cfg.store.enabled || cfg.store.also_write;
    if cfg.input.trim().is_empty() || (needs_output && cfg.output.trim().is_empty()) {
        if args.save_preset.is_some() {
            return Ok(());
        }
        Args::command().print_help()?;
        std::process::exit(2);
    }
    if args.threads > 0 {
        rayon::ThreadPoolBuilder::new().num_threads(args.threads).build_global()?;
    }
    let progress = Progress { echo: true, ..Default::default() };
    let done = Engine::new(&cfg)?.run(&progress)?;
    eprintln!("{} {done} 個", if cfg.store.enabled { "送信" } else { "書き出し" });
    Ok(())
}

#[cfg(feature = "gui")]
fn open_gui(args: Args, dir: PathBuf) -> Result<()> {
    let initial = build_config(&args, &dir).ok().filter(|_| args.preset.is_some() || args.input.is_some());
    // 名前で指定したプリセットだけを「いま使っているプリセット」として見せる(パスで指定したものは名前がない)
    let preset = args.preset.clone().filter(|p| !p.ends_with(".toml") && !p.contains(['/', '\\']));
    gui::run(dir, initial, preset)
}

#[cfg(not(feature = "gui"))]
fn open_gui(_: Args, _: PathBuf) -> Result<()> {
    Args::command().print_help()?;
    anyhow::bail!("この実行ファイルは画面なしでビルドされている")
}
