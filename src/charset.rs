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

const ESC_ROMAJI: [u8; 3] = [0x1b, b'(', b'J'];
const ESC_KANJI: [u8; 3] = [0x1b, b'$', b'B'];

/// 文字列を ISO 2022 IR 13 / IR 87 のバイト列にする(末尾は偶数長に空白で埋める)。
pub fn encode_iso2022jp(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 2);
    let mut kanji = false;
    for ch in s.chars() {
        if ch.is_ascii() {
            if kanji {
                out.extend_from_slice(&ESC_ROMAJI);
                kanji = false;
            }
            out.push(ch as u8);
        } else if ('\u{FF61}'..='\u{FF9F}').contains(&ch) {
            // 半角カナ(JIS X 0201 の G1)。G0 の状態とは独立
            out.push((ch as u32 - 0xFF61 + 0xA1) as u8);
        } else {
            let mut buf = [0u8; 4];
            let (bytes, _, bad) = encoding_rs::EUC_JP.encode(ch.encode_utf8(&mut buf));
            if bad || bytes.len() != 2 || bytes[0] < 0xA1 || bytes[1] < 0xA1 {
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
        out.extend_from_slice(&ESC_ROMAJI);
    }
    if out.len() % 2 == 1 {
        out.push(b' ');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_plain() {
        assert_eq!(encode_iso2022jp("Yamada^Taro").unwrap(), b"Yamada^Taro ");
    }

    #[test]
    fn kanji_and_reset_before_delimiters() {
        // 山田^太郎 → ESC $ B 3E 33 45 44 ESC ( J ^ ESC $ B 42 40 4F 3A ESC ( J
        let b = encode_iso2022jp("山田^太郎").unwrap();
        assert_eq!(
            b,
            [&ESC_KANJI[..], &[0x3b, 0x33, 0x45, 0x44], &ESC_ROMAJI[..], b"^", &ESC_KANJI[..], &[0x42, 0x40, 0x4f, 0x3a], &ESC_ROMAJI[..], b" "].concat()
        );
    }

    #[test]
    fn halfwidth_kana_and_errors() {
        assert_eq!(encode_iso2022jp("ﾔﾏﾀﾞ").unwrap(), [0xD4, 0xCF, 0xC0, 0xDE]);
        assert!(encode_iso2022jp("😀").is_err());
    }
}
