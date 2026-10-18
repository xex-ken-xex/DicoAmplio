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

/// G0 の「英数字」の指定。元ファイルの SpecificCharacterSet の先頭が `ISO 2022 IR 13` なら JIS X 0201
/// ローマ字(`ESC ( J`)、空か `ISO 2022 IR 6` なら ASCII(`ESC ( B`)に戻す。宣言していない方へ戻すと、
/// 規格に合わない文字列になる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    Romaji,
    Ascii,
}

impl Flavor {
    fn esc(self) -> [u8; 3] {
        match self {
            Flavor::Romaji => [0x1b, b'(', b'J'],
            Flavor::Ascii => [0x1b, b'(', b'B'],
        }
    }
}

const ESC_KANJI: [u8; 3] = [0x1b, b'$', b'B'];

/// 文字列を ISO 2022 のバイト列にする(末尾は偶数長に空白で埋める)。
/// 半角カナ(G1)は `ISO 2022 IR 13` を宣言しているとき(Romaji)だけ書ける。
pub fn encode_iso2022jp(s: &str, flavor: Flavor) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 2);
    let mut kanji = false;
    for ch in s.chars() {
        if ch.is_ascii() {
            if kanji {
                out.extend_from_slice(&flavor.esc());
                kanji = false;
            }
            out.push(ch as u8);
        } else if ('\u{FF61}'..='\u{FF9F}').contains(&ch) {
            if flavor != Flavor::Romaji {
                bail!("半角カナ `{ch}` は、文字セットに ISO 2022 IR 13 がないと書けない");
            }
            // 半角カナ(JIS X 0201 の G1)。G0 の状態とは独立
            out.push((ch as u32 - 0xFF61 + 0xA1) as u8);
        } else {
            let mut buf = [0u8; 4];
            let (bytes, _, bad) = encoding_rs::EUC_JP.encode(ch.encode_utf8(&mut buf));
            // EUC-JP は NEC・IBM の拡張(「髙」「﨑」「①」など)も 2 バイトで返すが、JIS X 0208 にあるのは
            // 1〜8 区と 16〜84 区だけ。範囲を見ないと、規格にないバイト列を黙って書いてしまう
            let ku = bytes.first().map(|b| b.wrapping_sub(0xA0)).unwrap_or(0);
            if bad || bytes.len() != 2 || bytes[1] < 0xA1 || !matches!(ku, 1..=8 | 16..=84) {
                bail!("JIS X 0208 で表せない文字: `{ch}` (U+{:04X})", ch as u32);
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
        out.extend_from_slice(&flavor.esc());
    }
    if out.len() % 2 == 1 {
        out.push(b' ');
    }
    Ok(out)
}

/// ISO 2022 のバイト列を文字列にする(`ESC ( B/J/I`、`ESC $ B/@`、G1 の半角カナ)。
/// 改行・改ページで G0 を初期状態(英数字)に戻す。
pub fn decode_iso2022jp(bytes: &[u8]) -> Result<String> {
    #[derive(PartialEq)]
    enum G0 {
        Latin,
        Kana,
        Kanji,
    }
    let mut out = String::new();
    let mut g0 = G0::Latin;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            let esc = &bytes[i..bytes.len().min(i + 3)];
            g0 = match esc {
                [0x1b, b'(', b'B'] | [0x1b, b'(', b'J'] => G0::Latin,
                [0x1b, b'(', b'I'] => G0::Kana,
                [0x1b, b'$', b'B'] | [0x1b, b'$', b'@'] => G0::Kanji,
                _ => bail!("未対応のエスケープシーケンス: {esc:02X?}"),
            };
            i += 3;
        } else if b >= 0xA1 && b <= 0xDF {
            out.push(char::from_u32(0xFF61 + (b - 0xA1) as u32).unwrap());
            i += 1;
        } else if matches!(b, 0x0A | 0x0C | 0x0D) {
            g0 = G0::Latin;
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
/// 読む(`ESC $ B` などを解釈しない)ので、先頭が ASCII か IR 13 のときは、ASCII と半角カナだけが
/// 変換されていて、1 文字 1 バイトに戻せる。戻せない(それ以外の文字を含む)ときは None。
pub fn raw_bytes_of(decoded: &str) -> Option<Vec<u8>> {
    decoded
        .chars()
        .map(|c| match c {
            c if c.is_ascii() => Some(c as u8),
            '\u{FF61}'..='\u{FF9F}' => Some((c as u32 - 0xFF61 + 0xA1) as u8),
            _ => None,
        })
        .collect()
}

/// (0008,0005) の値の並び。空の値(先頭の空)は ASCII を表すので、残す。
pub fn scs_values(raw: &str) -> Vec<String> {
    raw.trim_end_matches(['\0', ' ']).split('\\').map(|v| v.trim().to_string()).collect()
}

/// dicom-rs が正しく読めない、複数値の ISO 2022 の組か。
pub fn is_multi_iso2022(values: &[String]) -> bool {
    values.len() > 1 && values.iter().any(|v| v.starts_with("ISO 2022"))
}

/// 複数値の ISO 2022 で、自前の符号化・復号が扱える組(先頭が空・IR 6・IR 13、あとは IR 87 など)の
/// 英数字の戻し先。扱えない組(先頭が Latin など)は None。
pub fn flavor_of(values: &[String]) -> Option<Flavor> {
    match values.first().map(String::as_str) {
        Some("ISO 2022 IR 13") => Some(Flavor::Romaji),
        Some("") | Some("ISO 2022 IR 6") => Some(Flavor::Ascii),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_plain() {
        assert_eq!(encode_iso2022jp("Yamada^Taro", Flavor::Romaji).unwrap(), b"Yamada^Taro ");
    }

    #[test]
    fn kanji_and_reset_before_delimiters() {
        // 山田^太郎 → ESC $ B 3E 33 45 44 ESC ( J ^ ESC $ B 42 40 4F 3A ESC ( J
        let b = encode_iso2022jp("山田^太郎", Flavor::Romaji).unwrap();
        assert_eq!(
            b,
            [&ESC_KANJI[..], &[0x3b, 0x33, 0x45, 0x44], &Flavor::Romaji.esc()[..], b"^", &ESC_KANJI[..], &[0x42, 0x40, 0x4f, 0x3a], &Flavor::Romaji.esc()[..], b" "].concat()
        );
    }

    #[test]
    fn halfwidth_kana_and_errors() {
        assert_eq!(encode_iso2022jp("ﾔﾏﾀﾞ", Flavor::Romaji).unwrap(), [0xD4, 0xCF, 0xC0, 0xDE]);
        assert!(encode_iso2022jp("ﾔ", Flavor::Ascii).unwrap_err().to_string().contains("IR 13"));
        let err = encode_iso2022jp("髙橋", Flavor::Romaji).unwrap_err().to_string();
        assert!(err.contains("JIS X 0208") && err.contains('髙') && err.contains("9AD9"), "{err}");
        assert!(encode_iso2022jp("😀", Flavor::Romaji).is_err());
        // NEC・IBM の拡張や丸数字は、EUC-JP では書けても JIS X 0208 にはない
        for c in ["髙", "﨑", "①", "㈱"] {
            assert!(encode_iso2022jp(c, Flavor::Romaji).is_err(), "{c}");
        }
        for c in ["高", "崎", "山", "ー", "々", "ヶ"] {
            assert!(encode_iso2022jp(c, Flavor::Romaji).is_ok(), "{c}");
        }
    }

    #[test]
    fn ascii_flavor_returns_to_ascii_not_romaji() {
        let b = encode_iso2022jp("山田", Flavor::Ascii).unwrap();
        assert_eq!(&b[b.len() - 3..], &[0x1b, b'(', b'B']);
        let b = encode_iso2022jp("山田", Flavor::Romaji).unwrap();
        assert_eq!(&b[b.len() - 3..], &[0x1b, b'(', b'J']);
    }

    #[test]
    fn decodes_what_it_encodes() {
        for flavor in [Flavor::Romaji, Flavor::Ascii] {
            let s = "Yamada^Tarou=山田^太郎=ヤマダ^タロウ 頭部CT";
            assert_eq!(decode_iso2022jp(&encode_iso2022jp(s, flavor).unwrap()).unwrap().trim_end(), s);
        }
        assert_eq!(decode_iso2022jp(&[0xD4, 0xCF]).unwrap(), "ﾔﾏ");
        assert!(decode_iso2022jp(&[0x1b, b'$', b'(', b'D']).is_err());
    }

    #[test]
    fn recovers_the_raw_bytes_dicom_rs_decoded_with_the_first_value() {
        assert_eq!(raw_bytes_of("a\u{1b}$B;3ﾔ").unwrap(), [b'a', 0x1b, b'$', b'B', b';', b'3', 0xD4]);
        assert!(raw_bytes_of("山").is_none());
    }

    #[test]
    fn multi_valued_iso2022_sets_are_recognised() {
        let v = scs_values("ISO 2022 IR 13\\ISO 2022 IR 87");
        assert!(is_multi_iso2022(&v));
        assert_eq!(flavor_of(&v), Some(Flavor::Romaji));
        assert_eq!(flavor_of(&scs_values("\\ISO 2022 IR 87")), Some(Flavor::Ascii));
        assert!(!is_multi_iso2022(&scs_values("ISO_IR 192")));
        assert_eq!(flavor_of(&scs_values("ISO 2022 IR 100\\ISO 2022 IR 87")), None);
    }
}
