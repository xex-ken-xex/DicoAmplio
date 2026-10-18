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

# 元ファイルの文字セット名 → (SpecificCharacterSet, 日本語などを入れる 4 か所)。
# ISO 2022 IR 87 の単独は、ASCII を含む文字列の符号化が規格であいまいなので入れない。
CASES = {
    "utf8": ("ISO_IR 192", "頭部CT 山田", "東京病院", "頭部造影", "山田^太郎=山田^太郎=ヤマダ^タロウ"),
    "jahis": (["ISO 2022 IR 13", "ISO 2022 IR 87"], "頭部CT 山田", "東京病院", "頭部造影", "Yamada^Tarou=山田^太郎=ﾔﾏﾀﾞ^ﾀﾛｳ"),
    "ascii_ir87": (["", "ISO 2022 IR 87"], "頭部CT 山田", "東京病院", "頭部造影", "Yamada^Tarou=山田^太郎=ヤマダ^タロウ"),
    "ir13_only": ("ISO_IR 13", "ﾄｳﾌﾞCT", "ﾄｳｷｮｳ", "ｿﾞｳｴｲ", "YAMADA^TAROU=ﾔﾏﾀﾞ^ﾀﾛｳ"),
    "latin1": ("ISO_IR 100", "Kopf-CT Müller", "Universitätsklinik", "Kontrast", "Müller^Jörg"),
}
EXPECTED_ERRORS = {("latin1", "iso2022jp")}


def make(path, cs, desc, inst, seq, other):
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


failed = 0
with tempfile.TemporaryDirectory() as tmp:
    for name, case in CASES.items():
        src = os.path.join(tmp, "src", name)
        os.makedirs(src)
        make(os.path.join(src, "a.dcm"), *case)
        want = values(os.path.join(src, "a.dcm"))
        for charset in ("utf8", "iso2022jp", "keep"):
            out = os.path.join(tmp, "out", name, charset)
            r = subprocess.run([BIN, src, out, "--charset", charset], capture_output=True, text=True, env={**os.environ, "RUST_BACKTRACE": "0"})
            files = glob.glob(out + "/**/*.dcm", recursive=True)
            expected_error = (name, charset) in EXPECTED_ERRORS
            if r.returncode != 0 or not files:
                ok = expected_error and not files  # 失敗しても、途中で切れたファイルを残さない
                detail = r.stderr.strip().splitlines()[-1] if r.stderr.strip() else ""
                print(f"{'OK  ' if ok else 'NG  '} {name:10s} -> {charset:9s} エラー({'想定どおり' if ok else '想定外'}): {detail}")
                failed += not ok
                continue
            got = values(files[0])
            bad = {k: (want[k], got[k]) for k in want if want[k] != got[k]}
            failed += bool(bad) or expected_error
            print(f"{'OK  ' if not bad and not expected_error else 'NG  '} {name:10s} -> {charset:9s}", bad or "")
sys.exit(1 if failed else 0)
