use super::*;
use crate::subtitle_sync::{Entry, Policy, Record};
use std::sync::{atomic::AtomicBool, Arc};
impl Sessions {
    pub(crate) fn start_sync(
        &mut self,
        principal: &Principal,
        id: &str,
        mut record: Record,
        cancel: Arc<AtomicBool>,
        now: u64,
    ) -> Result<Record, &'static str> {
        let viewer = self.get(principal, id, now)?;
        if viewer.status != Status::Ready {
            return Err("session_not_ready");
        }
        let owned = self.viewers.get_mut(id).ok_or("session_not_found")?;
        principal.require(&owned.namespace, Scope::Create)?;
        record.revision = owned
            .subtitle_sync
            .as_ref()
            .map(|s| s.record.revision.checked_add(1))
            .unwrap_or(Some(1))
            .ok_or("idempotency_conflict")?;
        owned.subtitle_sync = Some(Entry {
            record: record.clone(),
            cancel,
        });
        Ok(record)
    }
    pub(crate) fn sync(
        &self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<Record, &'static str> {
        let viewer = self.get(principal, id, now)?;
        if viewer.status != Status::Ready {
            return Err("session_not_ready");
        }
        self.viewers
            .get(id)
            .and_then(|v| v.subtitle_sync.as_ref())
            .map(|s| s.record.clone())
            .ok_or("media_not_found")
    }
    pub(crate) fn stop_sync(
        &mut self,
        principal: &Principal,
        id: &str,
        generation: &str,
        now: u64,
    ) -> Result<(), &'static str> {
        let record = self.sync(principal, id, now)?;
        let owned = self.viewers.get_mut(id).ok_or("session_not_found")?;
        principal.require(&owned.namespace, Scope::Create)?;
        if record.playback_generation != generation {
            return Err("idempotency_conflict");
        }
        owned.subtitle_sync = None;
        Ok(())
    }
    pub(crate) fn sync_policy(
        &mut self,
        principal: &Principal,
        id: &str,
        policy: Policy,
        now: u64,
    ) -> Result<Record, &'static str> {
        let record = self.sync(principal, id, now)?;
        let owned = self.viewers.get_mut(id).ok_or("session_not_found")?;
        principal.require(&owned.namespace, Scope::Create)?;
        if record.revision != policy.revision {
            return Err("idempotency_conflict");
        }
        let entry = owned.subtitle_sync.as_mut().ok_or("media_not_found")?;
        entry.record.manual = policy.manual;
        if policy.manual {
            entry
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
        entry.record.revision = entry
            .record
            .revision
            .checked_add(1)
            .ok_or("idempotency_conflict")?;
        Ok(entry.record.clone())
    }
    pub(crate) fn finish_sync(
        &mut self,
        principal: &Principal,
        id: &str,
        expected: &Record,
        result: Result<serde_json::Value, String>,
        now: u64,
    ) -> Result<(), &'static str> {
        let current = self.sync(principal, id, now)?;
        if current.revision != expected.revision
            || current.manual
            || current.playback_generation != expected.playback_generation
            || current.subtitle_hash != expected.subtitle_hash
            || current.media_identity != expected.media_identity
            || current.audio_identity != expected.audio_identity
        {
            return Err("idempotency_conflict");
        }
        let mut record = current;
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or("idempotency_conflict")?;
        match result {
            Ok(value)
                if value["algorithmVersion"] == record.algorithm_version
                    && matches!(
                        value["state"].as_str(),
                        Some("accepted" | "observing" | "ambiguous" | "unavailable")
                    ) =>
            {
                record.state = value["state"].as_str().unwrap().into();
                if record.state == "observing" {
                    record.state = "ambiguous".into();
                    record.reason = Some("analysis_insufficient_evidence".into());
                }
                let correction = &value["correction"];
                if record.state == "accepted" {
                    let valid = correction["offsetUs"]
                        .as_i64()
                        .is_some_and(|v| (-30_000_000..=30_000_000).contains(&v))
                        && correction["anchorUs"].as_i64().is_some()
                        && correction["scale"]
                            .as_f64()
                            .is_some_and(|v| v.is_finite() && (0.98..=1.02).contains(&v));
                    if valid {
                        record.correction = Some(correction.clone());
                        record.correction_evidence = Some(value["correctionEvidence"].clone());
                    } else {
                        record.state = "unavailable".into();
                        record.reason = Some("analysis_estimator_protocol".into());
                    }
                }
                record.evidence = Some(value["evidence"].clone());
            }
            Err(reason) => {
                record.state = "unavailable".into();
                record.reason = Some(reason);
            }
            _ => {
                record.state = "unavailable".into();
                record.reason = Some("analysis_estimator_protocol".into());
            }
        }
        self.viewers
            .get_mut(id)
            .unwrap()
            .subtitle_sync
            .as_mut()
            .unwrap()
            .record = record;
        Ok(())
    }
}
