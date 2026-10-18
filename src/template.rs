//! printf 風のテンプレート。`%(名前)06d` のように変数名を括弧で指定する。
//!
//! 変換: `d`(整数) `s`(文字列) `f`(実数) `x` `X`(16進)、`%%` は `%` 一つ。
//! 書式: `-`(左寄せ) `0`(ゼロ埋め) `+`(符号) 幅 `.精度`。
//! 例: `EVAL%(p)06d` → EVAL000012、`%(weight).1f` → 62.5

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

#[derive(Debug, Clone)]
enum Piece {
    Lit(String),
    Spec {
        name: String,
        left: bool,
        zero: bool,
        plus: bool,
        width: usize,
        prec: Option<usize>,
        conv: char,
    },
}

#[derive(Debug, Clone)]
pub struct Template {
    pieces: Vec<Piece>,
}

/// 実行時の値。整数の変数、文字列の変数(元ファイルの値など)、CSV の列。
/// 名前が重なったときは、整数 → 文字列の変数 → CSV の順に見る。
pub struct Vars<'a> {
    pub ints: &'a [(&'static str, i64)],
    pub strs: &'a [(&'static str, &'a str)],
    pub csv: Option<&'a HashMap<String, String>>,
}

impl Template {
    pub fn parse(src: &str) -> Result<Template> {
        let cs: Vec<char> = src.chars().collect();
        let mut pieces = Vec::new();
        let mut lit = String::new();
        let mut k = 0;
        while k < cs.len() {
            if cs[k] != '%' {
                lit.push(cs[k]);
                k += 1;
                continue;
            }
            k += 1;
            if k < cs.len() && cs[k] == '%' {
                lit.push('%');
                k += 1;
                continue;
            }
            if k >= cs.len() || cs[k] != '(' {
                bail!("`%` の次は `(名前)` か `%` が必要: {src}");
            }
            let close = cs[k..].iter().position(|&c| c == ')').context("`)` がない")? + k;
            let name: String = cs[k + 1..close].iter().collect();
            k = close + 1;
            let (mut left, mut zero, mut plus) = (false, false, false);
            while k < cs.len() && matches!(cs[k], '-' | '0' | '+') {
                match cs[k] {
                    '-' => left = true,
                    '0' => zero = true,
                    _ => plus = true,
                }
                k += 1;
            }
            let mut width = 0usize;
            while k < cs.len() && cs[k].is_ascii_digit() {
                width = width * 10 + cs[k].to_digit(10).unwrap() as usize;
                k += 1;
            }
            let mut prec = None;
            if k < cs.len() && cs[k] == '.' {
                k += 1;
                let mut n = 0usize;
                while k < cs.len() && cs[k].is_ascii_digit() {
                    n = n * 10 + cs[k].to_digit(10).unwrap() as usize;
                    k += 1;
                }
                prec = Some(n);
            }
            let conv = *cs.get(k).context("変換文字(d s f x X)がない")?;
            if !matches!(conv, 'd' | 's' | 'f' | 'x' | 'X') {
                bail!("未対応の変換 `%{conv}`: {src}");
            }
            k += 1;
            if !lit.is_empty() {
                pieces.push(Piece::Lit(std::mem::take(&mut lit)));
            }
            pieces.push(Piece::Spec { name, left, zero, plus, width, prec, conv });
        }
        if !lit.is_empty() {
            pieces.push(Piece::Lit(lit));
        }
        Ok(Template { pieces })
    }

    /// CSV の列を使っているか(整数の変数だけなら、CSV の行を変えても値は変わらない)。
    pub fn uses_csv(&self, columns: &[String]) -> bool {
        self.names().any(|n| columns.iter().any(|c| c == n))
    }

    /// 使っている変数名(起動時の検査用)。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.pieces.iter().filter_map(|p| match p {
            Piece::Spec { name, .. } => Some(name.as_str()),
            _ => None,
        })
    }

    pub fn render(&self, v: &Vars) -> Result<String> {
        let mut out = String::new();
        for p in &self.pieces {
            match p {
                Piece::Lit(s) => out.push_str(s),
                Piece::Spec { name, left, zero, plus, width, prec, conv } => {
                    let int = v.ints.iter().find(|(n, _)| n == name).map(|(_, x)| *x);
                    let text: Option<&str> = v
                        .strs
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, t)| *t)
                        .or_else(|| v.csv.and_then(|m| m.get(name)).map(String::as_str));
                    let body = match conv {
                        'd' | 'x' | 'X' => {
                            let n = match (int, text) {
                                (Some(n), _) => n,
                                (None, Some(t)) => t.trim().parse::<f64>()
                                    .with_context(|| format!("`{name}` が数でない: {t}"))?
                                    .round() as i64,
                                _ => bail!("変数 `{name}` がない"),
                            };
                            let digits = match conv {
                                'x' => format!("{:x}", n.unsigned_abs()),
                                'X' => format!("{:X}", n.unsigned_abs()),
                                _ => n.unsigned_abs().to_string(),
                            };
                            let sign = if n < 0 { "-" } else if *plus { "+" } else { "" };
                            pad_num(sign, &digits, *width, *left, *zero)
                        }
                        'f' => {
                            let x = match (int, text) {
                                (Some(n), _) => n as f64,
                                (None, Some(t)) => t.trim().parse::<f64>()
                                    .with_context(|| format!("`{name}` が数でない: {t}"))?,
                                _ => bail!("変数 `{name}` がない"),
                            };
                            let digits = format!("{:.*}", prec.unwrap_or(6), x.abs());
                            let sign = if x < 0.0 { "-" } else if *plus { "+" } else { "" };
                            pad_num(sign, &digits, *width, *left, *zero)
                        }
                        _ => {
                            let mut s = match (int, text) {
                                (_, Some(t)) => t.to_string(),
                                (Some(n), None) => n.to_string(),
                                _ => bail!("変数 `{name}` がない"),
                            };
                            if let Some(p) = prec {
                                s = s.chars().take(*p).collect();
                            }
                            pad(&s, *width, *left)
                        }
                    };
                    out.push_str(&body);
                }
            }
        }
        Ok(out)
    }
}

fn pad(s: &str, width: usize, left: bool) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let fill = " ".repeat(width - n);
    if left { format!("{s}{fill}") } else { format!("{fill}{s}") }
}

fn pad_num(sign: &str, digits: &str, width: usize, left: bool, zero: bool) -> String {
    let n = sign.len() + digits.len();
    if n >= width {
        return format!("{sign}{digits}");
    }
    let k = width - n;
    if left {
        format!("{sign}{digits}{}", " ".repeat(k))
    } else if zero {
        format!("{sign}{}{digits}", "0".repeat(k))
    } else {
        format!("{}{sign}{digits}", " ".repeat(k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(csv: Option<&HashMap<String, String>>) -> Vars<'_> {
        Vars {
            ints: &[("p", 12), ("s", 0), ("r", 1), ("i", 3), ("p1", 13), ("s1", 1), ("r1", 2), ("i1", 4), ("n", 99), ("inst", 7)],
            strs: &[("src_patient_id", "ORIG-7")],
            csv,
        }
    }

    #[test]
    fn printf() {
        let t = Template::parse("EVAL%(p)06d-%(s1)d %(n)x %%").unwrap();
        assert_eq!(t.render(&vars(None)).unwrap(), "EVAL000012-1 63 %");
    }

    #[test]
    fn csv() {
        let m: HashMap<String, String> =
            [("name", "Yamada^Taro"), ("weight", "62.46")].map(|(a, b)| (a.into(), b.into())).into();
        let t = Template::parse("%(name)s|%(weight).1f|%(weight)8.2f|%(name)-12s|").unwrap();
        assert_eq!(t.render(&vars(Some(&m))).unwrap(), "Yamada^Taro|62.5|   62.46|Yamada^Taro |");
    }

    #[test]
    fn string_variables_render_like_csv_columns() {
        let t = Template::parse("%(src_patient_id)s-%(p1)d|%(src_patient_id).4s|").unwrap();
        assert_eq!(t.render(&vars(None)).unwrap(), "ORIG-7-13|ORIG|");
    }

    #[test]
    fn errors() {
        assert!(Template::parse("%d").is_err());
        assert!(Template::parse("%(p)q").is_err());
        assert!(Template::parse("%(zz)d").unwrap().render(&vars(None)).is_err());
    }
}
