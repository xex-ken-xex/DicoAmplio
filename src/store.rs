//! C-STORE 送信と C-ECHO。
//!
//! 1 つのアソシエーションは 1 つのグループ(モダリティ・検査など、設定の単位)を受け持つ。そのグループに
//! 要る SOP Class だけを提案して張り、送り切ったら解放する。実機の装置の送り方に近く、PACS 側の
//! 取り違えが減るため。速さのために、ディスクを経由せず、複製したデータセットをメモリから直接送る。
//! 提案する抽象構文は元ファイルの SOP Class UID そのものなので、私用の SOP Class も、受け側が
//! 受け入れれば送れる。

use crate::config::StoreConfig;
use anyhow::{anyhow, bail, Context, Result};
use dicom_core::{dicom_value, DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::{entries::IMPLICIT_VR_LITTLE_ENDIAN, TransferSyntaxRegistry};
use dicom_ul::pdu::{PDataValue, PDataValueType, PresentationContextResultReason};
use dicom_ul::{ClientAssociation, ClientAssociationOptions, Pdu};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub const EXPLICIT_LE: &str = "1.2.840.10008.1.2.1";
pub const IMPLICIT_LE: &str = "1.2.840.10008.1.2";
const VERIFICATION: &str = "1.2.840.10008.1.1";

/// 提案する転送構文。非圧縮のリトルエンディアンは明示・暗黙のどちらでも書けるので、両方を提案する
/// (元の方を先に)。圧縮やビッグエンディアンは、そのままの転送構文だけ。
pub fn proposable(ts: &str) -> Vec<String> {
    match ts {
        EXPLICIT_LE => vec![EXPLICIT_LE.into(), IMPLICIT_LE.into()],
        IMPLICIT_LE => vec![IMPLICIT_LE.into(), EXPLICIT_LE.into()],
        other => vec![other.to_string()],
    }
}

/// 送る 1 インスタンス。
pub struct Instance {
    pub sop_class: String,
    pub sop_instance: String,
    pub object: InMemDicomObject,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub sent: u64,
    /// 受け側が失敗の状態を返した、または SOP Class を受けつけなかったもの
    pub rejected: u64,
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, o: Stats) {
        self.sent += o.sent;
        self.rejected += o.rejected;
    }
}

/// 送った結果。
pub enum Outcome {
    Sent,
    /// 受けつけたが警告つき(status B000〜BFFF)
    Warned(u16),
    /// 受け側が失敗の状態を返した
    Rejected(u16),
    /// その SOP Class(か転送構文)を受け側が受けつけなかった
    Unsupported,
}

/// 張ったアソシエーション 1 本。
pub struct Connection {
    assoc: ClientAssociation<TcpStream>,
    /// SOP Class → (提示コンテキスト ID, 転送構文)
    accepted: HashMap<String, (u8, String)>,
    message_id: u16,
    stats: Stats,
}

fn options(cfg: &StoreConfig) -> ClientAssociationOptions<'_> {
    ClientAssociationOptions::new()
        .calling_ae_title(cfg.calling_ae.trim())
        .called_ae_title(cfg.called_ae.trim())
        .max_pdu_length(1 << 20)
        .connection_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .write_timeout(Duration::from_secs(120))
}

fn address(cfg: &StoreConfig) -> String {
    format!("{}:{}", cfg.host.trim(), cfg.port)
}

impl Connection {
    /// `contexts` は (SOP Class UID, 転送構文 UID の候補)。受け側が受けつけなかった SOP Class も返す。
    pub fn open(cfg: &StoreConfig, contexts: &[(String, Vec<String>)]) -> Result<(Connection, Vec<String>)> {
        if contexts.is_empty() {
            bail!("送るものがない");
        }
        if contexts.len() > 128 {
            bail!("SOP Class と転送構文の組が {} 通りあり、1 つのアソシエーションで提案できる 128 を超える", contexts.len());
        }
        let mut opt = options(cfg);
        for (sop, ts) in contexts {
            opt = opt.with_presentation_context(sop.as_str(), ts.iter().map(String::as_str).collect());
        }
        let assoc = opt
            .establish(address(cfg))
            .map_err(|e| anyhow!("{} ({}) にアソシエーションを張れない: {e}", address(cfg), cfg.called_ae.trim()))?;
        // 提案した順に ID が 1, 3, 5, … と振られる(PS3.8)
        let mut accepted = HashMap::new();
        for pc in assoc.presentation_contexts() {
            if pc.reason != PresentationContextResultReason::Acceptance {
                continue;
            }
            let index = (pc.id as usize).saturating_sub(1) / 2;
            if let Some((sop, _)) = contexts.get(index) {
                accepted.entry(sop.clone()).or_insert((pc.id, pc.transfer_syntax.trim_end_matches('\0').to_string()));
            }
        }
        let refused: Vec<String> = contexts.iter().filter(|(s, _)| !accepted.contains_key(s)).map(|(s, _)| s.clone()).collect();
        if accepted.is_empty() {
            let _ = assoc.abort();
            bail!("受け側が提示コンテキストを 1 つも受け入れない(SOP Class か転送構文が合わない): {}", refused.join(", "));
        }
        Ok((Connection { assoc, accepted, message_id: 1, stats: Stats::default() }, refused))
    }

    /// 1 インスタンスを送る。通信の失敗は Err(アソシエーションは打ち切る)。
    pub fn store(&mut self, inst: &Instance) -> Result<Outcome> {
        let Some((pc_id, ts_uid)) = self.accepted.get(&inst.sop_class).cloned() else {
            self.stats.rejected += 1;
            return Ok(Outcome::Unsupported);
        };
        let r = store_one(&mut self.assoc, pc_id, &ts_uid, inst, self.message_id);
        self.message_id = self.message_id.wrapping_add(1).max(1);
        match r {
            Ok(0) => {
                self.stats.sent += 1;
                Ok(Outcome::Sent)
            }
            Ok(status @ 0xB000..=0xBFFF) => {
                self.stats.sent += 1;
                Ok(Outcome::Warned(status))
            }
            Ok(status) => {
                self.stats.rejected += 1;
                Ok(Outcome::Rejected(status))
            }
            // 通信が壊れたアソシエーションは、呼び出し側が Connection ごと捨てる(接続が閉じる)
            Err(e) => Err(e.context(format!("{} の送信", inst.sop_instance))),
        }
    }

    pub fn close(self) -> Result<Stats> {
        self.assoc.release().map_err(|e| anyhow!("アソシエーションの解放に失敗: {e}"))?;
        Ok(self.stats)
    }
}

/// 1 インスタンスを送り、受け側の状態(0 なら成功)を返す。
fn store_one(assoc: &mut ClientAssociation<TcpStream>, pc_id: u8, ts_uid: &str, inst: &Instance, message_id: u16) -> Result<u16> {
    let ts = TransferSyntaxRegistry.get(ts_uid).with_context(|| format!("転送構文 {ts_uid} を知らない"))?;
    let mut data = Vec::new();
    inst.object.write_dataset_with_ts(&mut data, ts).with_context(|| format!("転送構文 {} でデータセットを書けない", ts.name()))?;

    let cmd = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, PrimitiveValue::from(inst.sop_class.as_str())),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x0001])),
        DataElement::new(tags::MESSAGE_ID, VR::US, dicom_value!(U16, [message_id])),
        DataElement::new(tags::PRIORITY, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(tags::AFFECTED_SOP_INSTANCE_UID, VR::UI, PrimitiveValue::from(inst.sop_instance.as_str())),
    ]);
    let mut cmd_bytes = Vec::new();
    cmd.write_dataset_with_ts(&mut cmd_bytes, &IMPLICIT_VR_LITTLE_ENDIAN.erased())?;

    let max = assoc.acceptor_max_pdu_length() as usize;
    let command = PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Command, is_last: true, data: cmd_bytes };
    if max == 0 || command.data.len() + data.len() + 100 < max {
        // 小さければコマンドとデータを 1 つの PDU で送る(往復が減る)
        let payload = PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Data, is_last: true, data };
        assoc.send(&Pdu::PData { data: vec![command, payload] })?;
    } else {
        assoc.send(&Pdu::PData { data: vec![command] })?;
        let mut w = assoc.send_pdata(pc_id);
        w.write_all(&data)?;
        w.finish()?;
    }
    read_status(assoc)
}

/// 応答(C-STORE-RSP / C-ECHO-RSP)の状態を読む。
fn read_status(assoc: &mut ClientAssociation<TcpStream>) -> Result<u16> {
    match assoc.receive()? {
        Pdu::PData { data } => {
            let v = data.into_iter().find(|v| v.value_type == PDataValueType::Command).context("応答にコマンドがない")?;
            let obj = InMemDicomObject::read_dataset_with_ts(&v.data[..], &IMPLICIT_VR_LITTLE_ENDIAN.erased())?;
            Ok(obj.element(tags::STATUS)?.to_int::<u16>()?)
        }
        Pdu::AbortRQ { source } => bail!("受け側がアソシエーションを打ち切った({source:?})"),
        other => bail!("予期しない PDU: {other:?}"),
    }
}

/// C-ECHO で接続を試す。成功なら一言を返す。
pub fn echo(cfg: &StoreConfig) -> Result<String> {
    let started = Instant::now();
    let mut assoc = options(cfg)
        .with_abstract_syntax(VERIFICATION)
        .establish(address(cfg))
        .map_err(|e| anyhow!("{} ({}) にアソシエーションを張れない: {e}", address(cfg), cfg.called_ae.trim()))?;
    let pc = assoc
        .presentation_contexts()
        .iter()
        .find(|pc| pc.reason == PresentationContextResultReason::Acceptance)
        .map(|pc| pc.id)
        .context("受け側が C-ECHO(Verification)を受けつけない")?;
    let cmd = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, PrimitiveValue::from(VERIFICATION)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x0030])),
        DataElement::new(tags::MESSAGE_ID, VR::US, dicom_value!(U16, [1])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
    ]);
    let mut bytes = Vec::new();
    cmd.write_dataset_with_ts(&mut bytes, &IMPLICIT_VR_LITTLE_ENDIAN.erased())?;
    assoc.send(&Pdu::PData { data: vec![PDataValue { presentation_context_id: pc, value_type: PDataValueType::Command, is_last: true, data: bytes }] })?;
    let status = read_status(&mut assoc)?;
    assoc.release().map_err(|e| anyhow!("アソシエーションの解放に失敗: {e}"))?;
    if status != 0 {
        bail!("C-ECHO の応答が失敗(status {status:04X})");
    }
    Ok(format!("C-ECHO 成功: {} ({} ms)", cfg.target(), started.elapsed().as_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_little_endian_proposes_both_vr_encodings_with_the_original_first() {
        assert_eq!(proposable(EXPLICIT_LE), vec![EXPLICIT_LE, IMPLICIT_LE]);
        assert_eq!(proposable(IMPLICIT_LE), vec![IMPLICIT_LE, EXPLICIT_LE]);
        assert_eq!(proposable("1.2.840.10008.1.2.4.50"), vec!["1.2.840.10008.1.2.4.50"]);
        assert_eq!(proposable("1.2.840.10008.1.2.2"), vec!["1.2.840.10008.1.2.2"]);
    }
}
