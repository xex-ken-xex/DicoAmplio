#!/usr/bin/env python3
"""元ファイルの文字セット × 出力の文字コード(--charset)の全組を、pydicom で読み戻して検証する。

使い方: python3 scripts/verify_charset.py [dicoamplio の実行ファイル]   (pydicom と numpy が要る)

pydicom は dicom-rs と別の実装なので、「元ファイルを pydicom で読んだ値」と「出力を pydicom で読んだ値」が
一致すれば、再符号化で文字が変わっていないと言える。Latin-1 の元ファイルを ISO 2022 にする組だけは、
ä が JIS X 0208 にないので、エラーで止まるのが正しい。
"""
import glob, os, subprocess, sys, tempfile, warnings
import numpy as np
import pydicom
from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.uid import CTImageStorage, ExplicitVRLittleEndian, generate_uid

warnings.simplefilter("ignore")
BIN = sys.argv[1] if len(sys.argv) > 1 else "target/release/dicoamplio"

# 元ファイルの文字セット名 → (SpecificCharacterSet, 日本語などを入れる 4 か所, 出力に新しく書く値)。
# ISO 2022 IR 87 の単独は、ASCII を含む文字列の符号化が規格であいまいなので入れない。
CASES = {
    "utf8": ("ISO_IR 192", "頭部CT 山田", "東京病院", "頭部造影", "山田^太郎=山田^太郎=ヤマダ^タロウ", "新規 Müller 山田"),
    "jahis": (["ISO 2022 IR 13", "ISO 2022 IR 87"], "頭部CT 山田", "東京病院", "頭部造影", "Yamada^Tarou=山田^太郎=ﾔﾏﾀﾞ^ﾀﾛｳ", "新規 ﾔﾏﾀﾞ 山田"),
    "ascii_ir87": (["", "ISO 2022 IR 87"], "頭部CT 山田", "東京病院", "頭部造影", "Yamada^Tarou=山田^太郎=ヤマダ^タロウ", "新規 山田"),
    "ir13_only": ("ISO_IR 13", "ﾄｳﾌﾞCT", "ﾄｳｷｮｳ", "ｿﾞｳｴｲ", "YAMADA^TAROU=ﾔﾏﾀﾞ^ﾀﾛｳ", "ﾆｭｳ ﾔﾏﾀﾞ"),
    "latin1": ("ISO_IR 100", "Kopf-CT Müller", "Universitätsklinik", "Kontrast", "Müller^Jörg", "Neu Müller"),
    # 先頭が G1 の文字セット(Latin・キリル・ギリシャ)の複数値
    "lat_ir87": (["ISO 2022 IR 100", "ISO 2022 IR 87"], "Kopf-CT 頭部 Müller", "Universität 東京", "Kontrast 造影", "Müller^Jörg=ミュラー^ヨルグ", "Neu Müller 山田"),
    "iso_ir100_ir87": (["ISO_IR 100", "ISO 2022 IR 87"], "Kopf-CT 頭部 Müller", "Universität 東京", "Kontrast 造影", "Müller^Jörg=ミュラー^ヨルグ", "Neu Müller 山田"),
    "cyr_ir87": (["ISO 2022 IR 144", "ISO 2022 IR 87"], "Иванов 山田", "Больница 東京", "Контраст 造影", "Иванов^Пётр=山田^太郎", "Новый 山田"),
    "grk_ir87": (["ISO 2022 IR 126", "ISO 2022 IR 87"], "Παπαδόπουλος 山田", "Νοσοκομείο 東京", "Σκιαγραφικό 造影", "Παπαδόπουλος^Γιάννης=山田^太郎", "Νέο 山田"),
    "lat_cyr": (["ISO 2022 IR 100", "ISO 2022 IR 144"], "Müller Иванов", "Universität Больница", "Kontrast Контраст", "Müller^Jörg=Иванов^Пётр", "Neu Müller Новый"),
    "lat_cyr_ir87": (["ISO 2022 IR 100", "ISO 2022 IR 144", "ISO 2022 IR 87"], "Müller Иванов 山田", "Universität Больница 東京", "Kontrast Контраст 造影", "Müller^Jörg=Иванов^Пётр=山田^太郎", "Neu Müller Новый 山田"),
}
# 日本語向けの組(IR 13 + IR 87)で書けない文字(ü、ギリシャのアクセント、ё 以外の混在など)を含む組は、エラーが正しい
EXPECTED_ERRORS = {
    (name, "iso2022jp")
    for name in ("latin1", "lat_ir87", "iso_ir100_ir87", "grk_ir87", "lat_cyr", "lat_cyr_ir87", "utf8")
}


def make(path, cs, desc, inst, seq, other, new):
    ds = Dataset()
    ds.file_meta = FileMetaDataset()
    ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
    sop = generate_uid()
    ds.file_meta.MediaStorageSOPClassUID = CTImageStorage
    ds.file_meta.MediaStorageSOPInstanceUID = sop
    ds.SOPClassUID, ds.SOPInstanceUID = CTImageStorage, sop
    ds.SpecificCharacterSet = cs
    ds.PatientName, ds.PatientID = "Real^Person", "1"
    ds.StudyDescription, ds.InstitutionName, ds.OtherPatientNames = desc, inst, other
    item = Dataset()
    item.RequestedProcedureDescription = seq
    ds.RequestAttributesSequence = [item]
    ds.StudyInstanceUID, ds.SeriesInstanceUID = generate_uid(), generate_uid()
    ds.InstanceNumber, ds.Modality = 1, "CT"
    ds.Rows = ds.Columns = 8
    ds.BitsAllocated = ds.BitsStored = 16
    ds.HighBit, ds.PixelRepresentation, ds.SamplesPerPixel = 15, 0, 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.PixelData = np.zeros((8, 8), np.uint16).tobytes()
    ds.save_as(path, enforce_file_format=True)


def values(f):
    d = pydicom.dcmread(f)
    return {
        "StudyDescription": d.get("StudyDescription"),
        "InstitutionName": d.get("InstitutionName"),
        "OtherPatientNames": str(d.get("OtherPatientNames")),
        "sequence item": d.RequestAttributesSequence[0].RequestedProcedureDescription,
    }


def run(src, out, charset, *extra):
    return subprocess.run([BIN, src, out, "--charset", charset, *extra], capture_output=True, text=True, env={**os.environ, "RUST_BACKTRACE": "0"})


failed = 0
with tempfile.TemporaryDirectory() as tmp:
    for name, case in CASES.items():
        src = os.path.join(tmp, "src", name)
        os.makedirs(src)
        make(os.path.join(src, "a.dcm"), *case)
        want = values(os.path.join(src, "a.dcm"))
        new = case[-1]
        for charset in ("utf8", "iso2022jp", "keep"):
            # 1: 元ファイルの文字列が変わらないこと。2: 新しく書く値(StudyDescription)を pydicom が読めること
            for label, extra, check in (
                ("元の文字列", (), lambda got: {k: (want[k], got[k]) for k in want if want[k] != got[k]}),
                ("新しい値", ("--set", "StudyDescription:study=" + new), lambda got: {} if got["StudyDescription"] == new else {"StudyDescription": (new, got["StudyDescription"])}),
            ):
                out = os.path.join(tmp, "out", name, charset, label)
                r = run(src, out, charset, *extra)
                files = glob.glob(out + "/**/*.dcm", recursive=True)
                expected_error = (name, charset) in EXPECTED_ERRORS
                if r.returncode != 0 or not files:
                    # 書けない文字は、エラーで止まり、途中で切れたファイルを残さないのが正しい
                    ok = expected_error and not files
                    detail = r.stderr.strip().splitlines()[-1].strip() if r.stderr.strip() else ""
                    print(f"{'OK  ' if ok else 'NG  '} {name:14s} -> {charset:9s} {label}: エラー({'想定どおり' if ok else '想定外'}): {detail}")
                    failed += not ok
                    continue
                bad = check(values(files[0]))
                ok = not bad
                failed += not ok
                print(f"{'OK  ' if ok else 'NG  '} {name:14s} -> {charset:9s} {label}", bad or "")
sys.exit(1 if failed else 0)
