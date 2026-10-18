# DicoAmplio

DICOM のタグを書き換えて、評価用に患者・検査・シリーズ・画像を水増しする(Rust、rayon で並列)。
引数なしで起動すると画面が開く。画面の見た目は [vw](https://github.com/xex-ken-xex/vw) と同じ。

```
cargo build --release
./target/release/dicoamplio 元のディレクトリ 出力先 \
  --patients 100 --studies 3 --series 2 --images 4 \
  --csv people.csv \
  --set 'PatientName:patient=%(name)s' --set 'PatientSex:patient=%(sex)s' \
  --set 'PatientWeight:patient=%(weight).1f' --set 'StudyDescription:study=CT %(s1)02d回目'
```

## ビルド

```
cargo build --release
```

できる実行ファイルは `target/release/dicoamplio`(Windows は `target\release\dicoamplio.exe`)。`target/` は `.gitignore` で除いてあるので、リポジトリには入らない。画面なしで作るなら `cargo build --release --no-default-features`(eframe などを入れないので小さい)。

- 件数は元を 1 組としたときの倍数。総数は 元ファイル数 × patients × studies × series × images。
- UID(Study/Series/SOP/FrameOfReference と、シーケンス内の参照)は、元の UID と複製の番号から決定的に作り直す。参照は同じ複製の中で張り替わる。
- 出力は `P患者/S検査/Rシリーズ/SOPInstanceUID.dcm`。

## タグの規則

`タグ:レベル=テンプレート`。タグは辞書の名前(`PatientName`)か、`(gggg,eeee)` で直接。辞書にないタグ(プライベートなど)は VR を添えて `(0029,1010)/LO:study=…` と書く(プライベートの持ち主の要素 `(0029,0010)` は元ファイルになければ自動で足す)。`--set` を繰り返すか、`--rules` のファイルに 1 行ずつ書く(`#` は注釈)。同じタグは後のものが勝つ。画面では、レベルごとの 4 つの節に分かれている。

### レベル(必須)

元の 1 組を n 倍にするとき、そのタグの値がどの単位で変わるか。`patient` / `study` / `series` / `image`。タグ名から推測して補うことはしない(外れると、同じ患者の中で値がばらつくデータができるため)。

- そのレベルの 1 つにつき値が 1 つ決まる。患者のタグは、同じ患者の全画像で同じ値になる。
- 使える変数は、そのレベルより粗い番号だけ。`patient` のタグで画像番号 `i` を使うとエラー。
- CSV の行は、そのタグのレベルの単位で 1 行ずつ進む。同じ CSV から `patient` のタグを複数読めば、1 人ぶんの行が揃って来る(名前と体重が別の人にならない)。
- DICOM の標準のレベルより細かくすると、プレビューで警告する(例: PatientName を `image` にした)。標準では年齢・身長・体重は検査レベルだが、既定の置き場は患者でよい。検査ごとに変えるなら `study` にする。

### テンプレート

printf 風で、変数は `%(名前)` で指定する: `EVAL%(p1)05d`、`%(weight).1f`、`%(name)-12s`。変換は `d s f x X`、書式は `- 0 +`・幅・`.精度`、`%%` は `%`。

| 変数 | 意味 |
|---|---|
| `p s r i` | 患者/検査/シリーズ/画像の番号(0 始まり)。`p1 s1 r1 i1` は 1 始まり |
| `n` | 通し番号(1 始まり) |
| `inst` | 元の InstanceNumber + 画像番号 × 10000 |
| CSV の列名 | CSV の 1 行目の列名がそのまま使える(文字列) |

### CSV

`--csv` は全体の CSV。タグごとに別の CSV を読むときは `タグ:レベル@CSVのパス=テンプレート`(プリセットでは `csv` と `csv_encoding`)。そのタグの変数には、そのタグの CSV の列が使える。行数を超えたら先頭へ戻る。

何も指定しないときの既定の規則(`src/engine.rs` の `DEFAULT_RULES`): PatientID・PatientName(患者)、StudyID・AccessionNumber(検査)、SeriesNumber(シリーズ)、InstanceNumber(画像)。

## 日本語(文字コード)

`--charset` か画面の「文字コード」で、(0008,0005) SpecificCharacterSet と符号化を選ぶ。

| 値 | (0008,0005) | 用途 |
|---|---|---|
| `utf8`(既定) | `ISO_IR 192` | 新しい装置・ビューア向け |
| `iso2022jp` | `ISO 2022 IR 13\ISO 2022 IR 87` | JAHIS など日本の運用向け。漢字は JIS X 0208 で `ESC $ B`、ローマ字へは `ESC ( J` で戻し、区切り文字(`^` `=` `\`)と値の終わりの前で必ず戻す。半角カナは G1。JIS X 0208 にない字はエラー |
| `keep` | 触れない | 元ファイルが日本語を表せる文字コードのときだけ |

PN は成分グループを `=` でつなぐ(字形=表意=表音)。例: `PatientName=%(name_alpha)s=%(name_kanji)s=%(name_kana)s`

### 元ファイルの文字セット

元ファイルがすでに持っている日本語も、出力の文字コードに合わせて読み直す。

- dicom-rs は複数値の ISO 2022(`ISO 2022 IR 13\ISO 2022 IR 87` など)を先頭の値だけで読み、`ESC $ B` が文字列に残る。そのまま UTF-8 で書くと文字化けするため、自前のデコーダで読み直してから書く。
- `keep` で元が複数値の ISO 2022 のときは、元の文字セットに合わせて新しい値を符号化する。
- 扱える複数値の組: 先頭が 空・IR 6・IR 13、または G1 の 1 バイトの文字セット(IR 100 Latin-1・101・109・110・126 ギリシャ・127 アラビア・138 ヘブライ・144 キリル・166 タイ)で、続きが IR 87(漢字)か、ほかの G1 のセット。値の途中で `ESC - A/B/C/D/F/G/H/L/T` や `ESC ) I`(半角カナ)で G1 を切り替える。IR 149・159・UTF-8 との混在は扱えない(エラーで止まる)。
- 区切り(`=` `\`)・改行・値の終わりで、G0 は英数字へ、G1 は先頭の値のセットへ戻す(規格どおり)。漢字から戻すのは、先頭が IR 13 なら `ESC ( J`、そうでなければ `ESC ( B`。
- 読むときは寛容にする: pydicom は漢字のあとを `ESC ( B` でなく `ESC - A` で戻すので、漢字の最中に G1 の割り当てが来たら漢字を終えたものと読む。書くときは規格どおり `ESC ( B`/`ESC ( J` で戻す。
- `keep` で、元の文字セットで書けない値(ASCII の元ファイルに日本語、`ISO_IR 13` に漢字など)は、書く前にエラーで止める。
- JIS X 0208 にない字(「髙」「﨑」「①」など)は、ISO 2022 では書けない。CSV の全行を、書き出す前に調べて、どの行のどの字かをエラーにする。例: `PatientName (0010,0010): CSV の 4 行目(列: name_kanji name_alpha): ... JIS X 0208 で表せない文字: `髙` (U+9AD9)`。UTF-8 なら書ける。
- 書き込みに失敗しても、途中で切れたファイルを出力に残さない。

`scripts/verify_charset.py`(要 pydicom)は、元ファイルの文字セット 11 種 × 出力 3 種を、元の文字列が変わらないことと、新しく書く値を pydicom が読めることの 2 点で確かめる。

CSV の文字コードは `--csv-encoding`(`auto` は UTF-8 → だめなら Shift_JIS。Excel の「CSV」も読める)。

## プリセット

画面の「プリセット」メニュー、またはコマンドラインの `--preset 名前` / `--save-preset 名前` で、設定一式(件数・レベルつきの規則・CSV・文字コード)を TOML で保存・再利用できる。置き場は実行ファイルの隣の `presets/`(`--preset-dir` で変更)。コマンドラインの指定は、プリセットの値を上書きする。`presets/` に例がある。

## 画面

引数なしで起動する(`--gui` でも開く)。設定(左)、タグの規則(中央)、プレビュー(右)、ログ(下)。プレビューは、何番目の複製に当たるかを選ぶと、規則を当てた値(ISO 2022 のときは符号化後のバイト列も)と、VR の決まり(桁数・CS の文字種など)に外れた値の警告を、元ファイルを読まずに見せる。画面なしでビルドするには `cargo build --release --no-default-features`。

## TODO(未完了・未確認)

- Windows の実機と、GUI の「参照」ダイアログ(rfd)は動作を確認していない(Linux の Xvfb で表示と実行のみ確認)
- DICOMDIR は作り直さない
- 元ファイルの文字セットが `ISO 2022 IR 87` 単独の場合は、ASCII を含む文字列の符号化が規格であいまいなので検証していない。IR 148(トルコ語)・IR 149(韓国語)・IR 159 を含む複数値は扱えない
- プリセットを GUI で上書きするときの確認がない
