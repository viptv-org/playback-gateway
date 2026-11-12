use crate::{
    credentials::{Credentials, Principal},
    sessions::Status,
    workers::Workers,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::Duration,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Start {
    pub content_vtt: String,
    pub playback_generation: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stop {
    pub playback_generation: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub revision: u64,
    pub manual: bool,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Record {
    pub version: u32,
    pub playback_generation: String,
    pub media_identity: String,
    pub audio_identity: String,
    pub subtitle_hash: String,
    pub algorithm_version: String,
    pub revision: u64,
    pub state: String,
    pub manual: bool,
    pub correction: Option<Value>,
    pub evidence: Option<Value>,
    pub correction_evidence: Option<Value>,
    pub confidence_kind: String,
    pub reason: Option<String>,
}
pub(crate) struct Entry {
    pub record: Record,
    pub cancel: Arc<AtomicBool>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
impl Workers {
    pub(crate) async fn start_subtitle_sync(
        &self,
        principal: &Principal,
        id: &str,
        start: Start,
        now: u64,
    ) -> Result<Record, &'static str> {
        self.require_active(principal, now).await?;
        if start.playback_generation.is_empty()
            || start.playback_generation.len() > 128
            || !start
                .playback_generation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("invalid_caption");
        }
        let admission_started = std::time::Instant::now();
        let (cues, subtitle_hash) = tokio::task::spawn_blocking(move || {
            let cues = cue_intervals(&start.content_vtt)?;
            Ok::<_, &'static str>((cues, digest(start.content_vtt.as_bytes())))
        })
        .await
        .map_err(|_| "invalid_caption")??;
        let now = now.saturating_add(admission_started.elapsed().as_secs());
        self.require_active(principal, now).await?;
        let viewer = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .get(principal, id, now)?;
        if viewer.status != Status::Ready {
            return Err("session_not_ready");
        }
        let media = self
            .active
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .get(&viewer.job_id)
            .cloned()
            .ok_or("media_unavailable")?;
        let enabled = std::env::var_os("VIPTV_SUBTITLE_SYNC_VAD")
            .zip(std::env::var_os("VIPTV_SUBTITLE_SYNC_ESTIMATOR"));
        let eligible = !media.live
            && media.format == "hls"
            && media.position == 0.0
            && media.selected_audio.is_some();
        let permit = if eligible && enabled.is_some() {
            self.subtitle_analysis.clone().try_acquire_owned().ok()
        } else {
            None
        };
        let can_analyze = permit.is_some();
        let audio_identity =
            digest(&serde_json::to_vec(&media.selected_audio).map_err(|_| "invalid_caption")?);
        let cancel = Arc::new(AtomicBool::new(false));
        let record = Record {
            version: 1,
            playback_generation: start.playback_generation,
            media_identity: format!("session_{}", viewer.job_id),
            audio_identity,
            subtitle_hash,
            algorithm_version: "activity-offset-affine-v1".into(),
            revision: 1,
            state: if can_analyze {
                "observing"
            } else {
                "unavailable"
            }
            .into(),
            manual: false,
            correction: None,
            evidence: None,
            correction_evidence: None,
            confidence_kind: "uncalibratedHeuristic".into(),
            reason: if can_analyze {
                None
            } else {
                Some("retained_audio_or_worker_unavailable".into())
            },
        };
        let record = self
            .sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .start_sync(principal, id, record, cancel.clone(), now)?;
        if eligible {
            if let (Some((vad, estimator)), Some(permit)) = (enabled, permit) {
                let sessions = self.sessions.clone();
                let engine = self.engine.clone();
                let credentials = self.credentials.clone();
                let principal = principal.clone();
                let id = id.to_owned();
                let job = media.id.clone();
                let expected = record.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let started = std::time::Instant::now();
                    let mut outcome: Result<Value, String> = Err("analysis_observing".into());
                    for _ in 0..12 {
                        if cancel.load(Ordering::Acquire) {
                            return;
                        }
                        let timestamp = now.saturating_add(started.elapsed().as_secs());
                        if !worker_key_active(&credentials, &principal.key.id, timestamp).await
                            || sessions
                                .lock()
                                .ok()
                                .and_then(|s| s.sync(&principal, &id, timestamp).ok())
                                .is_none()
                        {
                            return;
                        }
                        let work =
                            engine.retained_subtitle_activity(&job, std::path::Path::new(&vad));
                        tokio::pin!(work);
                        let mut authorization = tokio::time::interval(Duration::from_secs(1));
                        let activity = loop {
                            tokio::select! {
                                result=&mut work=>break result,
                                _=tokio::time::sleep(Duration::from_millis(100))=>{
                                    let timestamp=now.saturating_add(started.elapsed().as_secs());
                                    if cancel.load(Ordering::Acquire) || sessions.lock().ok().and_then(|s|s.sync(&principal,&id,timestamp).ok()).is_none() { return; }
                                },
                                _=authorization.tick()=>{
                                    let timestamp=now.saturating_add(started.elapsed().as_secs());
                                    if !worker_key_active(&credentials,&principal.key.id,timestamp).await {return;}
                                }
                            }
                        };
                        match activity {
                            Ok(observations) => {
                                let batches = observations
                                    .as_array()
                                    .unwrap()
                                    .chunks(3000)
                                    .map(|part| part.to_vec())
                                    .collect::<Vec<_>>();
                                let body = serde_json::to_vec(
                                    &json!({"epoch":1,"cues":cues,"batches":batches}),
                                )
                                .unwrap();
                                let command = tokio::process::Command::new(&estimator);
                                outcome = playback_engine::subtitle_sync::run(
                                    command,
                                    &body,
                                    64 * 1024,
                                    16 * 1024,
                                )
                                .await
                                .and_then(|(out, _)| {
                                    serde_json::from_slice(&out)
                                        .map_err(|_| "analysis_estimator_protocol".into())
                                });
                                break;
                            }
                            Err(reason)
                                if reason == "analysis_observing" || reason == "analysis_busy" => {}
                            Err(reason) => {
                                outcome = Err(reason);
                                break;
                            }
                        }
                        for _ in 0..50 {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            if cancel.load(Ordering::Acquire) {
                                return;
                            }
                        }
                    }
                    let timestamp = now.saturating_add(started.elapsed().as_secs());
                    if cancel.load(Ordering::Acquire)
                        || !worker_key_active(&credentials, &principal.key.id, timestamp).await
                    {
                        return;
                    }
                    if let Ok(mut sessions) = sessions.lock() {
                        let _ =
                            sessions.finish_sync(&principal, &id, &expected, outcome, timestamp);
                    };
                });
            }
        }
        Ok(record)
    }
    pub(crate) async fn stop_subtitle_sync(
        &self,
        principal: &Principal,
        id: &str,
        stop: Stop,
        now: u64,
    ) -> Result<(), &'static str> {
        self.require_active(principal, now).await?;
        self.sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .stop_sync(principal, id, &stop.playback_generation, now)
    }
    pub(crate) async fn subtitle_sync(
        &self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<Record, &'static str> {
        self.require_active(principal, now).await?;
        self.sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .sync(principal, id, now)
    }
    pub(crate) async fn subtitle_sync_policy(
        &self,
        principal: &Principal,
        id: &str,
        policy: Policy,
        now: u64,
    ) -> Result<Record, &'static str> {
        self.require_active(principal, now).await?;
        self.sessions
            .lock()
            .map_err(|_| "session_store_unavailable")?
            .sync_policy(principal, id, policy, now)
    }
}
fn cue_intervals(vtt: &str) -> Result<Vec<Value>, &'static str> {
    let _ = crate::captions::srt(vtt, 0.0)?;
    let mut cues = Vec::new();
    for line in vtt.lines().filter(|l| l.contains("-->")) {
        let (start, end) = line.split_once("-->").ok_or("invalid_caption")?;
        let start = crate::captions::clock(start.trim()).ok_or("invalid_caption")?;
        let end = crate::captions::clock(end.split_whitespace().next().ok_or("invalid_caption")?)
            .ok_or("invalid_caption")?;
        cues.push(json!({"startUs":start*1000,"endUs":end*1000}));
    }
    if cues.len() > 16_384 || cues.is_empty() {
        return Err("invalid_caption");
    }
    cues.sort_by_key(|v| v["startUs"].as_u64().unwrap_or(0));
    Ok(cues)
}

async fn worker_key_active(credentials: &Arc<Credentials>, owner: &str, now: u64) -> bool {
    let Ok(now) = i64::try_from(now) else {
        return false;
    };
    let credentials = credentials.clone();
    let owner = owner.to_owned();
    tokio::time::timeout(
        Duration::from_secs(1),
        tokio::task::spawn_blocking(move || credentials.active(&owner, now)),
    )
    .await
    .is_ok_and(|r| matches!(r, Ok(Ok(true))))
}
