//! 日本語の文字コード(DICOM PS3.5 の Specific Character Set)。
//!
//! - `Utf8`: `ISO_IR 192`。dicom-rs が符号化する。
//! - `Iso2022Jp`: `ISO 2022 IR 13\ISO 2022 IR 87`(JAHIS などの日本の運用)。
//!   G0 の初期状態は JIS X 0201 ローマ字(`ESC ( J`)、漢字は JIS X 0208(`ESC $ B`)、
//!   半角カナは G1(0xA1-0xDF)。区切り文字(`^` `=` `\`)や値の終わりの前に、
//!   必ずローマ字へ戻す。dicom-rs は複数値の文字セットを切り替えて書けないので、
//!   バイト列は自前で作る。
//! - `Keep`: 元ファイルの SpecificCharacterSet に触れない。

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Charset {
    #[default]
    Utf8,
    Iso2022jp,
    Keep,
}

impl Charset {
    /// (0008,0005) に入れる値。Keep は触らない。
    pub fn code(self) -> Option<&'static str> {
        match self {
            Charset::Utf8 => Some("ISO_IR 192"),
            Charset::Iso2022jp => Some("ISO 2022 IR 13\\ISO 2022 IR 87"),
            Charset::Keep => None,
        }
    }
}

use encoding_rs::Encoding;

const ESC_KANJI: [u8; 3] = [0x1b, b'$', b'B'];

/// G1 に割り当てる文字セット。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum G1 {
    /// `ISO 2022 IR 13`。半角カナ(JIS X 0201)
    Katakana,
    /// `SETS` の何番目か。1 バイトの右側(0xA0〜0xFF)の文字セット
    Set(usize),
}

struct SetInfo {
    /// `IR 100` のような名前
    ir: &'static str,
    /// G1 への割り当て `ESC - <終端>` の終端
    designator: u8,
    /// None は ISO 8859-1(0xA0〜0xFF がそのまま U+00A0〜U+00FF)
    encoding: Option<&'static Encoding>,
}

/// G1 に割り当てられる 1 バイトの文字セット。IR 148(トルコ語)は、dicom-rs が先頭の値として
/// 読めないので入れない。
const SETS: &[SetInfo] = &[
    SetInfo { ir: "IR 100", designator: b'A', encoding: None },
    SetInfo { ir: "IR 101", designator: b'B', encoding: Some(encoding_rs::ISO_8859_2) },
    SetInfo { ir: "IR 109", designator: b'C', encoding: Some(encoding_rs::ISO_8859_3) },
    SetInfo { ir: "IR 110", designator: b'D', encoding: Some(encoding_rs::ISO_8859_4) },
    SetInfo { ir: "IR 144", designator: b'L', encoding: Some(encoding_rs::ISO_8859_5) },
    SetInfo { ir: "IR 127", designator: b'G', encoding: Some(encoding_rs::ISO_8859_6) },
    SetInfo { ir: "IR 126", designator: b'F', encoding: Some(encoding_rs::ISO_8859_7) },
    SetInfo { ir: "IR 138", designator: b'H', encoding: Some(encoding_rs::ISO_8859_8) },
    SetInfo { ir: "IR 166", designator: b'T', encoding: Some(encoding_rs::WINDOWS_874) },
];

fn set_encode(info: &SetInfo, ch: char) -> Option<u8> {
    match info.encoding {
        None => u8::try_from(ch as u32).ok().filter(|b| *b >= 0xA0),
        Some(enc) => {
            let mut buf = [0u8; 4];
            let (bytes, _, bad) = enc.encode(ch.encode_utf8(&mut buf));
            (!bad && bytes.len() == 1 && bytes[0] >= 0xA0).then(|| bytes[0])
        }
    }
}

fn set_decode(info: &SetInfo, b: u8) -> Option<char> {
    match info.encoding {
        None => Some(b as char),
        Some(enc) => {
            let byte = [b];
            let (s, bad) = enc.decode_without_bom_handling(&byte);
            if bad { None } else { s.chars().next() }
        }
    }
}

/// `(0008,0005)` の複数値の ISO 2022 を、符号化・復号できる形にしたもの。
///
/// 先頭の値が、初期状態(G0 は英数字、G1 は先頭の値が G1 の文字セットならそれ)を決める。
/// あとの値は、値の途中でエスケープで切り替えられる文字セット(IR 87 の漢字、G1 の別のセット)。
#[derive(Clone, Debug)]
pub struct Iso2022 {
    /// 漢字から英数字に戻すエスケープ。先頭が IR 13 なら `ESC ( J`(ローマ字)、それ以外は `ESC ( B`(ASCII)。
    /// 宣言していない方へ戻すと、規格に合わない文字列になる
    g0_reset: [u8; 3],
    initial_g1: Option<G1>,
    g1_sets: Vec<G1>,
    kanji: bool,
    /// 先頭に書く値の表示用
    values: String,
}

impl Iso2022 {
    /// 日本語向け(`ISO 2022 IR 13\ISO 2022 IR 87`)。
    pub fn japanese() -> Iso2022 {
        let values = scs_values("ISO 2022 IR 13\\ISO 2022 IR 87");
        Iso2022::from_values(&values).expect("日本語向けの組は扱える")
    }

    /// 扱えない組(IR 149・IR 159・UTF-8 との混在など)は None。
    pub fn from_values(values: &[String]) -> Option<Iso2022> {
        let mut spec = Iso2022 { g0_reset: [0x1b, b'(', b'B'], initial_g1: None, g1_sets: vec![], kanji: false, values: values.join("\\") };
        for (i, v) in values.iter().enumerate() {
            let ir = v.strip_prefix("ISO 2022 ").or_else(|| v.strip_prefix("ISO_")).unwrap_or(v);
            let g1 = match ir {
                "" | "IR 6" if i == 0 => None,
                "IR 87" if i > 0 => {
                    spec.kanji = true;
                    continue;
                }
                "IR 13" => {
                    if i == 0 {
                        spec.g0_reset = [0x1b, b'(', b'J'];
                    }
                    Some(G1::Katakana)
                }
                other => Some(G1::Set(SETS.iter().position(|s| s.ir == other)?)),
            };
            if let Some(g1) = g1 {
                if i == 0 {
                    spec.initial_g1 = Some(g1);
                }
                spec.g1_sets.push(g1);
            }
        }
        Some(spec)
    }

    fn g1_designation(g1: G1) -> [u8; 3] {
        match g1 {
            G1::Katakana => [0x1b, b')', b'I'],
            G1::Set(i) => [0x1b, b'-', SETS[i].designator],
        }
    }

    fn g1_encode(g1: G1, ch: char) -> Option<u8> {
        match g1 {
            G1::Katakana => ('\u{FF61}'..='\u{FF9F}').contains(&ch).then(|| (ch as u32 - 0xFF61 + 0xA1) as u8),
            G1::Set(i) => set_encode(&SETS[i], ch),
        }
    }

    fn g1_decode(g1: Option<G1>, b: u8) -> Option<char> {
        match g1 {
            // G1 を宣言していなくても、0xA1〜0xDF は半角カナとして読む(寛容に)
            None | Some(G1::Katakana) => (0xA1..=0xDF).contains(&b).then(|| char::from_u32(0xFF61 + (b - 0xA1) as u32).unwrap()),
            Some(G1::Set(i)) => set_decode(&SETS[i], b),
        }
    }

    /// 文字列を ISO 2022 のバイト列にする(末尾は偶数長に空白で埋める)。
    /// 区切り(PN の `^` `=`、値の `\`)と制御文字(CR LF FF TAB)の前、値の終わりで、
    /// 初期状態(G0 は英数字、G1 は先頭の値のもの)に戻す(PS3.5 6.1.2.5.3)。
    /// pydicom は G1 を戻さずに `^` を書くが、戻しておけば厳密な読み手も寛容な読み手も同じに読める。
    pub fn encode(&self, s: &str) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len() * 2);
        let mut kanji = false;
        let mut g1 = self.initial_g1;
        let reset_g1 = |out: &mut Vec<u8>, g1: &mut Option<G1>| {
            if let (Some(init), true) = (self.initial_g1, *g1 != self.initial_g1) {
                out.extend_from_slice(&Self::g1_designation(init));
                *g1 = Some(init);
            }
        };
        for ch in s.chars() {
            if ch.is_ascii() {
                if kanji {
                    out.extend_from_slice(&self.g0_reset);
                    kanji = false;
                }
                if matches!(ch, '^' | '=' | '\\') || ch.is_ascii_control() {
                    reset_g1(&mut out, &mut g1);
                }
                out.push(ch as u8);
                continue;
            }
            // いまの G1 → 宣言されているほかの G1 → 漢字、の順に探す
            let current = g1.and_then(|g| Self::g1_encode(g, ch));
            if let Some(b) = current {
                out.push(b);
            } else if let Some((other, b)) = self.g1_sets.iter().find_map(|&o| Self::g1_encode(o, ch).map(|b| (o, b))) {
                out.extend_from_slice(&Self::g1_designation(other));
                g1 = Some(other);
                out.push(b);
            } else {
                let mut buf = [0u8; 4];
                let (bytes, _, bad) = encoding_rs::EUC_JP.encode(ch.encode_utf8(&mut buf));
                // EUC-JP は NEC・IBM の拡張(「髙」「﨑」「①」など)も 2 バイトで返すが、JIS X 0208 にあるのは
                // 1〜8 区と 16〜84 区だけ。範囲を見ないと、規格にないバイト列を黙って書いてしまう
                let ku = bytes.first().map(|b| b.wrapping_sub(0xA0)).unwrap_or(0);
                if !self.kanji || bad || bytes.len() != 2 || bytes[1] < 0xA1 || !matches!(ku, 1..=8 | 16..=84) {
                    if self.kanji {
                        bail!("JIS X 0208 で表せない文字: `{ch}` (U+{:04X})", ch as u32);
                    }
                    bail!("文字セット `{}` で表せない文字: `{ch}` (U+{:04X})", self.values, ch as u32);
                }
                if !kanji {
                    out.extend_from_slice(&ESC_KANJI);
                    kanji = true;
                }
                out.push(bytes[0] - 0x80);
                out.push(bytes[1] - 0x80);
            }
        }
        if kanji {
            out.extend_from_slice(&self.g0_reset);
        }
        reset_g1(&mut out, &mut g1);
        if out.len() % 2 == 1 {
            out.push(b' ');
        }
        Ok(out)
    }

    /// ISO 2022 のバイト列を文字列にする。`ESC ( B/J/I`、`ESC $ B/@`、G1 の割り当て(`ESC ) I`、`ESC - A` など)を読む。
    /// 改行・改ページで初期状態に戻す。
    ///
    /// 漢字の最中に `ESC - X`(G1 の割り当て)が来たら、漢字を終えたものとして読む。pydicom は漢字のあとを
    /// `ESC ( B` ではなく `ESC - A` で戻すため、そうしないと続きの英数字を漢字として読んでしまう。
    pub fn decode(&self, bytes: &[u8]) -> Result<String> {
        #[derive(PartialEq)]
        enum G0 {
            Latin,
            Kana,
            Kanji,
        }
        let mut out = String::new();
        let mut g0 = G0::Latin;
        let mut g1 = self.initial_g1;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == 0x1b {
                let esc = &bytes[i..bytes.len().min(i + 3)];
                match esc {
                    [0x1b, b'(', b'B'] | [0x1b, b'(', b'J'] => g0 = G0::Latin,
                    [0x1b, b'(', b'I'] => g0 = G0::Kana,
                    [0x1b, b'$', b'B'] | [0x1b, b'$', b'@'] => g0 = G0::Kanji,
                    [0x1b, b')', b'I'] => g1 = Some(G1::Katakana),
                    [0x1b, b'-', f] => {
                        let Some(i) = SETS.iter().position(|s| s.designator == *f) else {
                            bail!("未対応のエスケープシーケンス: {esc:02X?}")
                        };
                        g1 = Some(G1::Set(i));
                        if g0 == G0::Kanji {
                            g0 = G0::Latin;
                        }
                    }
                    _ => bail!("未対応のエスケープシーケンス: {esc:02X?}"),
                }
                i += 3;
            } else if b >= 0xA0 {
                match Self::g1_decode(g1, b) {
                    Some(c) => out.push(c),
                    None => bail!("G1 の文字セットで読めないバイト: {b:02X}"),
                }
                i += 1;
            } else if matches!(b, 0x0A | 0x0C | 0x0D) {
                g0 = G0::Latin;
                g1 = self.initial_g1;
                out.push(b as char);
                i += 1;
            } else if b < 0x80 && g0 == G0::Kanji && b > 0x20 {
                let Some(&b2) = bytes.get(i + 1) else { bail!("漢字の 2 バイト目がない") };
                let pair = [b | 0x80, b2 | 0x80];
                let (s, _, bad) = encoding_rs::EUC_JP.decode(&pair);
                if bad || b2 <= 0x20 {
                    bail!("JIS X 0208 として読めないバイト: {b:02X} {b2:02X}");
                }
                out.push_str(&s);
                i += 2;
            } else if b < 0x80 && g0 == G0::Kana && b > 0x20 && b < 0x60 {
                out.push(char::from_u32(0xFF61 + (b - 0x21) as u32).unwrap());
                i += 1;
            } else if b < 0x80 {
                out.push(b as char);
                i += 1;
            } else {
                bail!("読めないバイト: {b:02X}");
            }
        }
        Ok(out)
    }

    /// dicom-rs が読んだ文字列を、元のバイト列に戻す。dicom-rs は複数値の文字セットを先頭の値だけで
    /// 読み(`ESC $ B` などを解釈しない)、先頭の値が G1 の文字セットなら、0xA0 以上のバイトは
    /// そのセットで文字になっている。ASCII は 1 対 1、その文字は 1 バイトに戻せる。戻せなければ None。
    pub fn raw_bytes_of(&self, decoded: &str) -> Option<Vec<u8>> {
        decoded
            .chars()
            .map(|c| {
                if c.is_ascii() {
                    return Some(c as u8);
                }
                match self.initial_g1 {
                    Some(g1) => Self::g1_encode(g1, c),
                    None => None,
                }
            })
            .collect()
    }
}

/// (0008,0005) の値の並び。空の値(先頭の空)は ASCII を表すので、残す。
pub fn scs_values(raw: &str) -> Vec<String> {
    raw.trim_end_matches(['\0', ' ']).split('\\').map(|v| v.trim().to_string()).collect()
}

/// dicom-rs が正しく読めない、複数値の ISO 2022 の組か。
pub fn is_multi_iso2022(values: &[String]) -> bool {
    values.len() > 1 && values.iter().any(|v| v.starts_with("ISO 2022"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(raw: &str) -> Iso2022 {
        Iso2022::from_values(&scs_values(raw)).unwrap()
    }

    const JAHIS: &str = "ISO 2022 IR 13\\ISO 2022 IR 87";

    #[test]
    fn ascii_is_plain() {
        assert_eq!(Iso2022::japanese().encode("Yamada^Taro").unwrap(), b"Yamada^Taro ");
    }

    #[test]
    fn kanji_resets_to_romaji_before_delimiters_when_ir13_leads() {
        let b = spec(JAHIS).encode("山田^太郎").unwrap();
        let romaji = [0x1b, b'(', b'J'];
        assert_eq!(b, [&ESC_KANJI[..], &[0x3b, 0x33, 0x45, 0x44], &romaji, b"^", &ESC_KANJI[..], &[0x42, 0x40, 0x4f, 0x3a], &romaji, b" "].concat());
    }

    #[test]
    fn kanji_resets_to_ascii_when_ir13_is_not_declared() {
        let b = spec("\\ISO 2022 IR 87").encode("山田").unwrap();
        assert_eq!(&b[b.len() - 3..], &[0x1b, b'(', b'B']);
        let b = spec(JAHIS).encode("山田").unwrap();
        assert_eq!(&b[b.len() - 3..], &[0x1b, b'(', b'J']);
    }

    #[test]
    fn halfwidth_kana_needs_ir13_and_other_characters_name_the_missing_set() {
        assert_eq!(Iso2022::japanese().encode("ﾔﾏﾀﾞ").unwrap(), [0xD4, 0xCF, 0xC0, 0xDE]);
        let err = spec("\\ISO 2022 IR 87").encode("ﾔ").unwrap_err().to_string();
        assert!(err.contains("ﾔ") && err.contains("FF94"), "{err}");
        let err = Iso2022::japanese().encode("髙橋").unwrap_err().to_string();
        assert!(err.contains("JIS X 0208") && err.contains('髙') && err.contains("9AD9"), "{err}");
        let err = spec("ISO 2022 IR 100\\ISO 2022 IR 144").encode("山").unwrap_err().to_string();
        assert!(err.contains("IR 100") && err.contains('山'), "{err}");
    }

    #[test]
    fn nec_and_ibm_extensions_are_not_jis_x_0208() {
        let j = Iso2022::japanese();
        for c in ["髙", "﨑", "①", "㈱", "😀"] {
            assert!(j.encode(c).is_err(), "{c}");
        }
        for c in ["高", "崎", "山", "ー", "々", "ヶ"] {
            assert!(j.encode(c).is_ok(), "{c}");
        }
    }

    #[test]
    fn decodes_what_it_encodes_for_every_supported_pair() {
        let cases = [
            (JAHIS, "Yamada^Tarou=山田^太郎=ヤマダ^タロウ 頭部CT"),
            ("\\ISO 2022 IR 87", "Yamada^Tarou=山田^太郎=ヤマダ^タロウ"),
            ("ISO 2022 IR 100\\ISO 2022 IR 87", "Müller^Jörg=山田^太郎 Kopf-CT"),
            ("ISO 2022 IR 144\\ISO 2022 IR 87", "Иванов^Пётр=山田^太郎"),
            ("ISO 2022 IR 126\\ISO 2022 IR 87", "Παπαδόπουλος 山田"),
            ("ISO 2022 IR 100\\ISO 2022 IR 144", "Müller Иванов Müller"),
            ("ISO 2022 IR 100\\ISO 2022 IR 144\\ISO 2022 IR 87", "Müller=Иванов=山田"),
        ];
        for (raw, text) in cases {
            let s = spec(raw);
            assert_eq!(s.decode(&s.encode(text).unwrap()).unwrap().trim_end(), text, "{raw}");
        }
        assert_eq!(Iso2022::japanese().decode(&[0xD4, 0xCF]).unwrap(), "ﾔﾏ");
        assert!(Iso2022::japanese().decode(&[0x1b, b'$', b'(', b'D']).is_err());
    }

    #[test]
    fn g1_is_returned_to_the_first_set_before_every_delimiter() {
        let s = spec("ISO 2022 IR 100\\ISO 2022 IR 144");
        let cyrillic = [0x1b, b'-', b'L'];
        let latin = [0x1b, b'-', b'A'];
        for delim in ['=', '^', '\\', '\t', '\n'] {
            let b = s.encode(&format!("Иван{delim}Müller")).unwrap();
            assert_eq!(b[..3], cyrillic);
            let at = b.iter().position(|&x| x == delim as u8).unwrap();
            assert_eq!(b[at - 3..at], latin, "{delim:?} の前に先頭の G1(IR 100)へ戻す");
        }
        // 戻したあとの Latin は、同じセットなので割り当てを繰り返さない
        let b = s.encode("Иван^Müller").unwrap();
        assert_eq!(b.windows(3).filter(|w| *w == latin).count(), 1);
    }

    #[test]
    fn a_kanji_run_ends_at_a_g1_designation_as_pydicom_writes_it() {
        // pydicom は漢字のあとを ESC ( B ではなく ESC - A で戻す
        let bytes = b"\x1b-AKopf-CT \x1b$BF,It\x1b-A M\xfcller";
        assert_eq!(spec("ISO 2022 IR 100\\ISO 2022 IR 87").decode(bytes).unwrap(), "Kopf-CT 頭部 Müller");
    }

    #[test]
    fn raw_bytes_are_recovered_from_what_dicom_rs_read_with_the_first_value() {
        let latin = spec("ISO 2022 IR 100\\ISO 2022 IR 87");
        assert_eq!(latin.raw_bytes_of("a\u{1b}$B;3ü").unwrap(), [b'a', 0x1b, b'$', b'B', b';', b'3', 0xFC]);
        assert!(latin.raw_bytes_of("山").is_none());
        let jahis = spec(JAHIS);
        assert_eq!(jahis.raw_bytes_of("a\u{1b}$B;3ﾔ").unwrap(), [b'a', 0x1b, b'$', b'B', b';', b'3', 0xD4]);
        let cyrillic = spec("ISO 2022 IR 144\\ISO 2022 IR 87");
        assert_eq!(cyrillic.raw_bytes_of("И").unwrap(), [0xB8]);
    }

    #[test]
    fn a_kanji_whose_second_byte_is_a_backslash_is_not_a_delimiter() {
        // 「旬」は JIS X 0208 で 3D 5C。vw の charset.rs が同じ点をテストしている
        let s = Iso2022::japanese();
        assert_eq!(s.decode(b"\x1b$B=\\\x1b(J").unwrap(), "旬");
        assert_eq!(s.decode(&s.encode("旬").unwrap()).unwrap().trim_end(), "旬");
    }

    #[test]
    fn unsupported_sets_are_rejected() {
        for raw in ["ISO 2022 IR 149\\ISO 2022 IR 87", "ISO 2022 IR 100\\ISO 2022 IR 159", "ISO_IR 192\\ISO 2022 IR 87", "ISO 2022 IR 87\\ISO 2022 IR 13"] {
            assert!(Iso2022::from_values(&scs_values(raw)).is_none(), "{raw}");
        }
        assert!(is_multi_iso2022(&scs_values(JAHIS)));
        assert!(!is_multi_iso2022(&scs_values("ISO_IR 192")));
    }

    #[test]
    fn the_first_value_may_use_the_iso_ir_spelling() {
        assert!(Iso2022::from_values(&scs_values("ISO_IR 100\\ISO 2022 IR 87")).is_some());
    }
}
