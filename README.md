# DicoAmplio

DICOM のタグを書き換えて、評価用に患者・検査・シリーズ・画像を水増しする(Rust、rayon で並列)。
引数なしで起動すると画面が開く。画面の見た目は [vw](https://github.com/xex-ken-xex/vw) と同じ。

```
cargo build --release
./target/release/dicoamplio 元のディレクトリ 出力先 \
  --patients 100 --studies 3 --series 2 --images 4 \
  --csv people.csv --rules rules.txt --set 'AccessionNumber=A%(n)07d'
```

- 件数は元を 1 組としたときの倍数。総数は 元ファイル数 × patients × studies × series × images。
- UID(Study/Series/SOP/FrameOfReference と、シーケンス内の参照)は、元の UID と複製の番号から決定的に作り直す。参照は同じ複製の中で張り替わる。
- 出力は `P患者/S検査/Rシリーズ/SOPInstanceUID.dcm`。

## タグの規則

`タグ=テンプレート`(タグは `PatientName` か `(0010,0010)`)。`--set` を繰り返すか、`--rules` のファイルに 1 行ずつ書く(`#` は注釈)。同じタグは後のものが勝つ。

テンプレートは printf 風で、変数は `%(名前)` で指定する: `EVAL%(p1)05d`、`%(weight).1f`、`%(name)-12s`。変換は `d s f x X`、書式は `- 0 +`・幅・`.精度`、`%%` は `%`。

| 変数 | 意味 |
|---|---|
| `p s r i` | 患者/検査/シリーズ/画像の番号(0 始まり)。`p1 s1 r1 i1` は 1 始まり |
| `n` | 通し番号(1 始まり) |
| `inst` | 元の InstanceNumber + 画像番号 × 10000 |
| CSV の列名 | `--csv` の 1 行目の列名がそのまま使える(文字列) |

CSV は `--csv-level`(patient / study / series / image、既定は patient)の単位で 1 行ずつ進み、行数を超えたら先頭へ戻る。

何も指定しないときの既定の規則: PatientID、PatientName、StudyID、AccessionNumber、SeriesNumber、InstanceNumber(`src/main.rs` の `DEFAULT_RULES`)。

## 日本語(文字コード)

`--charset` か画面の「文字コード」で、(0008,0005) SpecificCharacterSet と符号化を選ぶ。

| 値 | (0008,0005) | 用途 |
|---|---|---|
| `utf8`(既定) | `ISO_IR 192` | 新しい装置・ビューア向け |
| `iso2022jp` | `ISO 2022 IR 13\ISO 2022 IR 87` | JAHIS など日本の運用向け。漢字は JIS X 0208 で `ESC $ B`、ローマ字へは `ESC ( J` で戻し、区切り文字(`^` `=` `\`)と値の終わりの前で必ず戻す。半角カナは G1。JIS X 0208 にない字はエラー |
| `keep` | 触れない | 元ファイルが日本語を表せる文字コードのときだけ |

PN は成分グループを `=` でつなぐ(字形=表意=表音)。例: `PatientName=%(name_alpha)s=%(name_kanji)s=%(name_kana)s`

CSV の文字コードは `--csv-encoding`(`auto` は UTF-8 → だめなら Shift_JIS。Excel の「CSV」も読める)。

## プリセット

画面の「プリセット」メニュー、またはコマンドラインの `--preset 名前` / `--save-preset 名前` で、設定一式(件数・規則・CSV・文字コード)を TOML で保存・再利用できる。置き場は実行ファイルの隣の `presets/`(`--preset-dir` で変更)。コマンドラインの指定は、プリセットの値を上書きする。`presets/` に例がある。

## 画面

引数なしで起動する(`--gui` でも開く)。設定(左)、タグの規則(中央)、プレビュー(右)、ログ(下)。プレビューは、何番目の複製に当たるかを選ぶと、規則を当てた値(ISO 2022 のときは符号化後のバイト列も)と、VR の決まり(桁数・CS の文字種など)に外れた値の警告を、元ファイルを読まずに見せる。画面なしでビルドするには `cargo build --release --no-default-features`。
