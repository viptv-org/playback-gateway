//! Atomic lifecycle bookkeeping. Engine work happens outside this lock; stopping
//! jobs retain their admission reservation until the engine confirms teardown.
use crate::{
    credentials::{hash, random_hex, Credentials, Principal, Scope},
    protocol::{CreateSession, IDLE_GRACE_SECONDS, STARTUP_DEADLINE_SECONDS, VIEWER_LEASE_SECONDS},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Use the engine's inspected plan, never a parallel control-layer approximation.
pub use playback_engine::OutputPlan as ExecutionPlan;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Ready,
    Failed,
    Released,
    Expired,
}
#[derive(Clone, Debug, Serialize)]
pub struct Viewer {
    pub id: String,
    pub job_id: String,
    pub status: Status,
    pub expires_at: u64,
    pub error_code: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Reservation {
    pub viewer: Viewer,
    pub start_job: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobState {
    Starting,
    Ready,
    Stopping,
}
struct Job {
    id: String,
    input: String,
    fingerprint: String,
    state: JobState,
    created: u64,
    idle_since: Option<u64>,
    viewers: BTreeSet<String>,
    reservations: BTreeSet<String>,
}
struct OwnedViewer {
    view: Viewer,
    owner: String,
    request_hash: [u8; 32],
    idempotency: String,
    media_hash: Option<[u8; 32]>,
    media_token: Option<String>,
}

/// Returned only on issuance. Never include this capability in logs or a
/// control-plane session representation. Rotation invalidates the previous one.
#[derive(Serialize)]
pub struct MediaCredential {
    pub token: String,
}
impl std::fmt::Debug for MediaCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaCredential(<redacted>)")
    }
}

/// Allowlisted public failures, never raw engine diagnostics or upstream URLs.
#[derive(Clone, Copy, Debug)]
pub enum Failure {
    SourceUnavailable,
    SourceConnectionLimit,
    UnsupportedMedia,
    ProcessingFailed,
}
impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::SourceUnavailable => "source_unavailable",
            Self::SourceConnectionLimit => "source_connection_limit",
            Self::UnsupportedMedia => "unsupported_media",
            Self::ProcessingFailed => "processing_failed",
        }
    }
}

pub struct Sessions {
    jobs: BTreeMap<String, Job>,
    viewers: BTreeMap<String, OwnedViewer>,
    idle_grace: u64,
    max_records: usize,
}
impl Default for Sessions {
    fn default() -> Self {
        Self {
            jobs: BTreeMap::new(),
            viewers: BTreeMap::new(),
            idle_grace: IDLE_GRACE_SECONDS,
            max_records: 16384,
        }
    }
}
impl Sessions {
    /// Per-key admission hints; stopping jobs stay charged until teardown.
    pub fn available(&self, principal: &Principal, now: u64) -> crate::credentials::Quotas {
        let jobs: Vec<_> = self
            .jobs
            .values()
            .filter(|job| job.reservations.contains(&principal.key.id))
            .collect();
        let inputs = jobs
            .iter()
            .map(|job| job.input.as_str())
            .collect::<BTreeSet<_>>()
            .len() as u32;
        let viewers = self
            .viewers
            .values()
            .filter(|viewer| {
                viewer.owner == principal.key.id
                    && viewer.view.expires_at > now
                    && matches!(viewer.view.status, Status::Starting | Status::Ready)
            })
            .count() as u32;
        crate::credentials::Quotas {
            inputs: principal.key.spec.quotas.inputs.saturating_sub(inputs),
            outputs: principal
                .key
                .spec
                .quotas
                .outputs
                .saturating_sub(jobs.len() as u32),
            viewers: principal.key.spec.quotas.viewers.saturating_sub(viewers),
        }
    }
    /// Resolve idempotent retries before performing any source network I/O.
    pub fn replay(
        &self,
        principal: &Principal,
        request: &CreateSession,
        idempotency: &str,
        now: u64,
    ) -> Result<Option<Viewer>, &'static str> {
        request.validate()?;
        principal.require(&request.namespace, Scope::Create)?;
        if idempotency.is_empty()
            || idempotency.len() > 128
            || !idempotency
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("invalid_idempotency_key");
        }
        let request_hash = hash(&serde_json::to_string(request).map_err(|_| "invalid_request")?);
        let Some(existing) = self
            .viewers
            .values()
            .find(|v| v.owner == principal.key.id && v.idempotency == idempotency)
        else {
            return Ok(None);
        };
        if existing.request_hash != request_hash {
            return Err("idempotency_conflict");
        }
        let mut view = existing.view.clone();
        if view.expires_at <= now && matches!(view.status, Status::Starting | Status::Ready) {
            view.status = Status::Expired;
        }
        Ok(Some(view))
    }
    pub fn reserve(
        &mut self,
        principal: &Principal,
        request: &CreateSession,
        plan: &ExecutionPlan,
        idempotency: &str,
        now: u64,
    ) -> Result<Reservation, &'static str> {
        if let Some(viewer) = self.replay(principal, request, idempotency, now)? {
            return Ok(Reservation {
                viewer,
                start_job: false,
            });
        }
        if !plan.shareable {
            return Err("unsupported_output");
        }
        let body = serde_json::to_string(request).map_err(|_| "invalid_request")?;
        let request_hash = hash(&body);
        if self.viewers.len() >= self.max_records {
            return Err("session_capacity");
        }
        let own: Vec<_> = self
            .viewers
            .values()
            .filter(|v| {
                v.owner == principal.key.id
                    && matches!(v.view.status, Status::Starting | Status::Ready)
                    && v.view.expires_at > now
            })
            .collect();
        if own.len() >= principal.key.spec.quotas.viewers as usize {
            return Err("viewer_capacity");
        }
        // Headers are case-insensitive. Structural validation rejects duplicate
        // spellings so identity cannot depend on an ambiguous wire representation.
        let headers: BTreeMap<_, _> = request
            .input
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        if headers.len() != request.input.headers.len() {
            return Err("invalid_headers");
        }
        let identity = serde_json::json!([
            request.namespace,
            request.input.url,
            headers,
            request.input.live
        ]);
        let input = hex(&hash(&identity.to_string()));
        let fingerprint = hex(&hash(&serde_json::json!([input, plan]).to_string()));
        let existing = self.jobs.values().find(|j| j.fingerprint == fingerprint);
        if existing.is_some_and(|job| job.state == JobState::Stopping) {
            return Err("input_cleanup_pending");
        }
        let own_jobs: BTreeSet<_> = self
            .jobs
            .values()
            .filter(|job| job.reservations.contains(&principal.key.id))
            .map(|job| job.id.as_str())
            .collect();
        let own_inputs: BTreeSet<_> = own_jobs
            .iter()
            .filter_map(|id| self.jobs.get(*id).map(|j| j.input.as_str()))
            .collect();
        if !own_inputs.contains(input.as_str())
            && own_inputs.len() >= principal.key.spec.quotas.inputs as usize
        {
            return Err("input_capacity");
        }
        if !existing.is_some_and(|job| own_jobs.contains(job.id.as_str()))
            && own_jobs.len() >= principal.key.spec.quotas.outputs as usize
        {
            return Err("output_capacity");
        }
        let (job_id, start_job) = existing
            .map(|job| (job.id.clone(), false))
            .unwrap_or_else(|| (format!("job_{}", random_hex(16)), true));
        let job = self.jobs.entry(job_id.clone()).or_insert_with(|| Job {
            id: job_id.clone(),
            input,
            fingerprint,
            state: JobState::Starting,
            created: now,
            idle_since: None,
            viewers: BTreeSet::new(),
            reservations: BTreeSet::new(),
        });
        let id = format!("viewer_{}", random_hex(16));
        let view = Viewer {
            id: id.clone(),
            job_id,
            status: if job.state == JobState::Ready {
                Status::Ready
            } else {
                Status::Starting
            },
            expires_at: now + VIEWER_LEASE_SECONDS,
            error_code: None,
        };
        job.viewers.insert(id.clone());
        job.reservations.insert(principal.key.id.clone());
        job.idle_since = None;
        self.viewers.insert(
            id,
            OwnedViewer {
                view: view.clone(),
                owner: principal.key.id.clone(),
                request_hash,
                idempotency: idempotency.into(),
                media_hash: None,
                media_token: None,
            },
        );
        Ok(Reservation {
            viewer: view,
            start_job,
        })
    }
    pub fn ready(&mut self, id: &str, now: u64) -> bool {
        let Some(job) = self.jobs.get_mut(id) else {
            return false;
        };
        if job.state != JobState::Starting
            || now.saturating_sub(job.created) >= STARTUP_DEADLINE_SECONDS
            || !job.viewers.iter().any(|id| {
                self.viewers
                    .get(id)
                    .is_some_and(|v| v.view.expires_at > now)
            })
        {
            return false;
        }
        job.state = JobState::Ready;
        for id in &job.viewers {
            if let Some(viewer) = self.viewers.get_mut(id) {
                if viewer.view.expires_at > now {
                    viewer.view.status = Status::Ready;
                }
            }
        }
        true
    }
    pub fn get(&self, principal: &Principal, id: &str, now: u64) -> Result<Viewer, &'static str> {
        if !principal.key.spec.scopes.contains(&Scope::Read) {
            return Err("forbidden");
        }
        let viewer = self
            .viewers
            .get(id)
            .filter(|v| v.owner == principal.key.id)
            .ok_or("session_not_found")?;
        let mut view = viewer.view.clone();
        if matches!(view.status, Status::Starting | Status::Ready) && view.expires_at <= now {
            view.status = Status::Expired;
        }
        Ok(view)
    }
    /// Call only with a freshly authenticated control principal. A media token
    /// can never mint another token or authorize a control operation.
    pub fn issue_media(
        &mut self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<MediaCredential, &'static str> {
        let view = self.get(principal, id, now)?;
        if !matches!(view.status, Status::Starting | Status::Ready) {
            return Err("session_expired");
        }
        let token = format!("pgm_{}", random_hex(32));
        self.viewers
            .get_mut(id)
            .ok_or("session_not_found")?
            .media_hash = Some(hash(&token));
        self.viewers
            .get_mut(id)
            .ok_or("session_not_found")?
            .media_token = Some(token.clone());
        Ok(MediaCredential { token })
    }
    /// Stable capability for idempotent create responses and polling. Explicit
    /// issue_media remains the rotation operation. Tokens exist only in memory.
    pub(crate) fn ensure_media(
        &mut self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<MediaCredential, &'static str> {
        if !principal
            .key
            .spec
            .scopes
            .iter()
            .any(|scope| matches!(scope, Scope::Create | Scope::Read | Scope::Renew))
        {
            return Err("forbidden");
        }
        let viewer = self
            .viewers
            .get_mut(id)
            .filter(|viewer| viewer.owner == principal.key.id)
            .ok_or("session_not_found")?;
        if viewer.view.expires_at <= now
            || !matches!(viewer.view.status, Status::Starting | Status::Ready)
        {
            return Err("session_expired");
        }
        if let Some(token) = &viewer.media_token {
            return Ok(MediaCredential {
                token: token.clone(),
            });
        }
        let token = format!("pgm_{}", random_hex(32));
        viewer.media_hash = Some(hash(&token));
        viewer.media_token = Some(token.clone());
        Ok(MediaCredential { token })
    }
    /// Every media resource (including child playlists, keys and subtitles)
    /// must pass this check. Reads never extend the viewer lease. The returned
    /// job ID is internal routing data, not an engine/media credential.
    pub fn authorize_media(
        &self,
        credentials: &Credentials,
        id: &str,
        token: &str,
        now: u64,
    ) -> Result<String, &'static str> {
        let denied = "media_unauthorized";
        if token.len() != 68 || !token.starts_with("pgm_") {
            return Err(denied);
        }
        let viewer = self.viewers.get(id).ok_or(denied)?;
        let expected = viewer.media_hash.ok_or(denied)?;
        let supplied = hash(token);
        let difference = expected
            .iter()
            .zip(supplied)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b));
        if difference != 0 || viewer.view.status != Status::Ready || viewer.view.expires_at <= now {
            return Err(denied);
        }
        let timestamp = i64::try_from(now).map_err(|_| denied)?;
        // Fail closed even if the credential store is unavailable. Consult the
        // current record, not the principal cached when the job was created.
        if !credentials
            .active(&viewer.owner, timestamp)
            .map_err(|_| denied)?
        {
            return Err(denied);
        }
        if !self
            .jobs
            .get(&viewer.view.job_id)
            .is_some_and(|job| job.state == JobState::Ready)
        {
            return Err(denied);
        }
        Ok(viewer.view.job_id.clone())
    }
    /// Mark all viewers failed immediately, but keep capacity reserved until
    /// the worker has stopped and calls finish_stop. Returns true once only.
    pub fn fail(&mut self, id: &str, failure: Failure, now: u64) -> bool {
        let Some(job) = self.jobs.get_mut(id) else {
            return false;
        };
        if job.state == JobState::Stopping {
            return false;
        }
        job.state = JobState::Stopping;
        for id in &job.viewers {
            if let Some(viewer) = self.viewers.get_mut(id) {
                if matches!(viewer.view.status, Status::Starting | Status::Ready) {
                    viewer.view.status = Status::Failed;
                    viewer.view.error_code = Some(failure.code().into());
                    viewer.view.expires_at = now;
                    viewer.media_hash = None;
                }
            }
        }
        true
    }
    pub fn renew(
        &mut self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<Viewer, &'static str> {
        if !principal.key.spec.scopes.contains(&Scope::Renew) {
            return Err("forbidden");
        }
        let viewer = self
            .viewers
            .get_mut(id)
            .filter(|v| v.owner == principal.key.id)
            .ok_or("session_not_found")?;
        if viewer.view.expires_at <= now
            || !matches!(viewer.view.status, Status::Starting | Status::Ready)
        {
            return Err("session_expired");
        }
        viewer.view.expires_at = now + VIEWER_LEASE_SECONDS;
        Ok(viewer.view.clone())
    }
    pub fn release(
        &mut self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<(), &'static str> {
        if !principal.key.spec.scopes.contains(&Scope::Release) {
            return Err("forbidden");
        }
        // Unknown and foreign IDs are both a no-op; release is idempotent and
        // must not become an existence oracle for another integration's viewer.
        if let Some(viewer) = self.viewers.get(id) {
            if viewer.owner != principal.key.id {
                return Ok(());
            }
        }
        self.detach(id, Status::Released, now);
        Ok(())
    }
    fn detach(&mut self, id: &str, status: Status, now: u64) {
        let Some(viewer) = self.viewers.get_mut(id) else {
            return;
        };
        if !matches!(viewer.view.status, Status::Starting | Status::Ready) {
            return;
        }
        viewer.view.status = status;
        viewer.media_token = None;
        viewer.media_hash = None;
        viewer.view.expires_at = now;
        let job_id = viewer.view.job_id.clone();
        let owner = viewer.owner.clone();
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.viewers.remove(id);
            if job.viewers.is_empty() {
                job.idle_since = Some(now);
            } else if !job
                .viewers
                .iter()
                .any(|id| self.viewers.get(id).is_some_and(|v| v.owner == owner))
            {
                job.reservations.remove(&owner);
            }
        }
    }
    pub fn revoke(&mut self, key_id: &str, now: u64) {
        let ids: Vec<_> = self
            .viewers
            .iter()
            .filter(|(_, v)| v.owner == key_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.detach(&id, Status::Released, now);
        }
    }
    /// Returns jobs whose teardown must be completed, without freeing admission.
    pub fn reap(&mut self, now: u64) -> Vec<String> {
        let expired: Vec<_> = self
            .viewers
            .iter()
            .filter(|(_, v)| {
                v.view.expires_at <= now
                    && matches!(v.view.status, Status::Starting | Status::Ready)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.detach(&id, Status::Expired, now);
        }
        let mut stopping = Vec::new();
        for job in self.jobs.values_mut() {
            let timed_out = job.state == JobState::Starting
                && now.saturating_sub(job.created) >= STARTUP_DEADLINE_SECONDS;
            if timed_out
                || job
                    .idle_since
                    .is_some_and(|at| now.saturating_sub(at) >= self.idle_grace)
            {
                if job.state != JobState::Stopping {
                    job.state = JobState::Stopping;
                    stopping.push(job.id.clone());
                    for id in &job.viewers {
                        if let Some(viewer) = self.viewers.get_mut(id) {
                            viewer.view.status = Status::Failed;
                            viewer.view.error_code = Some("startup_timeout".into());
                            viewer.view.expires_at = now;
                        }
                    }
                }
            }
        }
        self.viewers.retain(|_, v| {
            matches!(v.view.status, Status::Starting | Status::Ready)
                || now.saturating_sub(v.view.expires_at) < 300
        });
        stopping
    }
    pub fn finish_stop(&mut self, id: &str) -> bool {
        if self
            .jobs
            .get(id)
            .is_some_and(|job| job.state == JobState::Stopping)
        {
            self.jobs.remove(id);
            true
        } else {
            false
        }
    }
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }
    pub fn stopping_jobs(&self) -> Vec<String> {
        self.jobs
            .values()
            .filter(|job| job.state == JobState::Stopping)
            .map(|job| job.id.clone())
            .collect()
    }
    pub fn active_owners(&self) -> Vec<String> {
        self.viewers
            .values()
            .filter(|viewer| matches!(viewer.view.status, Status::Starting | Status::Ready))
            .map(|viewer| viewer.owner.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}
fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        credentials::{KeyRecord, KeySpec, Quotas},
        protocol::{Input, Output, OutputProtocol},
    };
    fn principal(id: &str, namespace: &str) -> Principal {
        Principal {
            key: KeyRecord {
                id: id.into(),
                created_at: 0,
                revoked: false,
                spec: KeySpec {
                    label: "Fixture".into(),
                    namespaces: vec![namespace.into()],
                    scopes: vec![Scope::Create, Scope::Read, Scope::Renew, Scope::Release],
                    quotas: Quotas {
                        inputs: 2,
                        outputs: 3,
                        viewers: 10,
                    },
                    expires_at: None,
                },
            },
        }
    }
    fn request(namespace: &str) -> CreateSession {
        CreateSession {
            namespace: namespace.into(),
            input: Input {
                url: "https://provider.invalid/channel".into(),
                headers: BTreeMap::new(),
                live: true,
            },
            output: Output {
                protocol: OutputProtocol::Hls,
                video_codecs: vec!["h264".into()],
                audio_codecs: vec!["aac".into()],
                max_width: 3840,
                max_height: 2160,
                audio_track: None,
                subtitle_track: None,
            },
            position_seconds: 0.0,
        }
    }
    fn plan() -> ExecutionPlan {
        ExecutionPlan {
            mode: "remux".into(),
            format: "hls".into(),
            video_codec: "h264".into(),
            audio_codec: Some("aac".into()),
            video_mode: "copy".into(),
            audio_mode: "copy".into(),
            width: 1920,
            height: 1080,
            audio_track: None,
            subtitle_track: None,
            position_micros: 0,
            segmentation: "mpegts:split".into(),
            shareable: true,
        }
    }
    #[test]
    fn five_viewers_share_one_job_and_single_stop_does_not_interrupt_others() {
        let p = principal("one", "family");
        let mut sessions = Sessions::default();
        let viewers: Vec<_> = (0..5)
            .map(|i| {
                sessions
                    .reserve(&p, &request("family"), &plan(), &format!("request-{i}"), 0)
                    .unwrap()
            })
            .collect();
        assert_eq!(viewers.iter().filter(|v| v.start_job).count(), 1);
        assert_eq!(sessions.job_count(), 1);
        assert!(sessions.ready(&viewers[0].viewer.job_id, 1));
        sessions.release(&p, &viewers[0].viewer.id, 2).unwrap();
        assert!(sessions.reap(20).is_empty());
        assert_eq!(
            sessions.get(&p, &viewers[1].viewer.id, 20).unwrap().status,
            Status::Ready
        );
        for viewer in &viewers[1..] {
            sessions.release(&p, &viewer.viewer.id, 21).unwrap();
        }
        assert!(sessions.reap(35).is_empty());
        assert_eq!(sessions.reap(36), vec![viewers[0].viewer.job_id.clone()]);
        assert!(sessions
            .reserve(&p, &request("family"), &plan(), "during-cleanup", 36)
            .is_err());
        assert!(sessions.finish_stop(&viewers[0].viewer.job_id));
        assert_eq!(sessions.job_count(), 0);
    }
    #[test]
    fn namespaces_idempotency_and_revocation_are_independent() {
        let p = principal("one", "family");
        let other = principal("two", "other");
        let mut sessions = Sessions::default();
        let first = sessions
            .reserve(&p, &request("family"), &plan(), "a", 0)
            .unwrap();
        assert_eq!(
            sessions
                .reserve(&p, &request("family"), &plan(), "a", 1)
                .unwrap()
                .viewer
                .id,
            first.viewer.id
        );
        let mut different = request("family");
        different.input.url.push_str("/another");
        assert_eq!(
            sessions
                .reserve(&p, &different, &plan(), "a", 1)
                .unwrap_err(),
            "idempotency_conflict"
        );
        assert!(sessions
            .reserve(&other, &request("family"), &plan(), "b", 1)
            .is_err());
        let second = sessions
            .reserve(&other, &request("other"), &plan(), "b", 1)
            .unwrap();
        assert_ne!(first.viewer.job_id, second.viewer.job_id);
        assert!(sessions.get(&other, &first.viewer.id, 1).is_err());
        sessions.ready(&second.viewer.job_id, 2);
        sessions.revoke("one", 2);
        assert_eq!(
            sessions.get(&other, &second.viewer.id, 3).unwrap().status,
            Status::Ready
        );
    }
    #[test]
    fn expiry_and_late_start_cannot_revive_sessions() {
        let p = principal("one", "family");
        let mut sessions = Sessions::default();
        let first = sessions
            .reserve(&p, &request("family"), &plan(), "a", 0)
            .unwrap();
        assert_eq!(sessions.reap(30), vec![first.viewer.job_id.clone()]);
        assert!(!sessions.ready(&first.viewer.job_id, 31));
        assert!(sessions.renew(&p, &first.viewer.id, 31).is_err());
        assert_eq!(
            sessions
                .get(&p, &first.viewer.id, 31)
                .unwrap()
                .error_code
                .as_deref(),
            Some("startup_timeout")
        );
    }
    #[test]
    fn cleanup_retains_key_capacity_even_for_a_different_input() {
        let mut p = principal("one", "family");
        p.key.spec.quotas.inputs = 1;
        let mut sessions = Sessions::default();
        let first = sessions
            .reserve(&p, &request("family"), &plan(), "a", 0)
            .unwrap();
        sessions.ready(&first.viewer.job_id, 1);
        sessions.release(&p, &first.viewer.id, 2).unwrap();
        sessions.reap(17);
        assert_eq!(sessions.available(&p,17).inputs,0,"cleanup must remain charged in capacity hints");
        let mut next = request("family");
        next.input.url.push_str("/other");
        assert_eq!(
            sessions.reserve(&p, &next, &plan(), "b", 17).unwrap_err(),
            "input_capacity"
        );
        sessions.finish_stop(&first.viewer.job_id);
        assert_eq!(sessions.available(&p,18).inputs,1);
        assert!(sessions.reserve(&p, &next, &plan(), "b", 18).is_ok());
    }

    #[test]
    fn media_capabilities_are_viewer_scoped_revocable_and_do_not_renew() {
        let bootstrap = "test-bootstrap-key-long-enough-for-fixture";
        let credentials = Credentials::from_connection(
            rusqlite::Connection::open_in_memory().unwrap(),
            bootstrap,
        )
        .unwrap();
        let issued = credentials
            .issue(bootstrap, principal("template", "family").key.spec, 0)
            .unwrap();
        let p = credentials.authenticate(&issued.secret, 0).unwrap();
        let mut sessions = Sessions::default();
        let one = sessions
            .reserve(&p, &request("family"), &plan(), "one", 0)
            .unwrap()
            .viewer;
        let two = sessions
            .reserve(&p, &request("family"), &plan(), "two", 0)
            .unwrap()
            .viewer;
        let old = sessions.issue_media(&p, &one.id, 0).unwrap();
        assert!(!format!("{old:?}").contains(&old.token));
        assert!(sessions
            .authorize_media(&credentials, &one.id, &old.token, 0)
            .is_err());
        assert!(sessions.ready(&one.job_id, 1));
        let fresh = sessions.issue_media(&p, &one.id, 1).unwrap();
        let second = sessions.issue_media(&p, &two.id, 1).unwrap();
        assert!(sessions
            .authorize_media(&credentials, &one.id, &old.token, 1)
            .is_err());
        assert!(sessions
            .authorize_media(&credentials, &two.id, &fresh.token, 1)
            .is_err());
        assert!(sessions
            .authorize_media(&credentials, &one.id, &issued.secret, 1)
            .is_err());
        assert!(credentials.authenticate(&fresh.token, 1).is_err());
        assert_eq!(
            sessions
                .authorize_media(&credentials, &one.id, &fresh.token, 59)
                .unwrap(),
            one.job_id
        );
        assert!(sessions
            .authorize_media(&credentials, &one.id, &fresh.token, 60)
            .is_err());
        sessions.renew(&p, &two.id, 59).unwrap();
        assert!(sessions
            .authorize_media(&credentials, &two.id, &second.token, 60)
            .is_ok());
        // Revocation works before the background reaper or explicit revoke hook.
        credentials.revoke(bootstrap, &p.key.id).unwrap();
        assert!(sessions
            .authorize_media(&credentials, &two.id, &second.token, 60)
            .is_err());
    }

    #[test]
    fn failed_jobs_deny_late_ready_and_retain_capacity_until_teardown() {
        let p = principal("one", "family");
        let mut sessions = Sessions::default();
        let one = sessions
            .reserve(&p, &request("family"), &plan(), "one", 0)
            .unwrap()
            .viewer;
        let two = sessions
            .reserve(&p, &request("family"), &plan(), "two", 0)
            .unwrap()
            .viewer;
        assert!(sessions.fail(&one.job_id, Failure::SourceConnectionLimit, 1));
        assert!(!sessions.fail(&one.job_id, Failure::ProcessingFailed, 2));
        assert!(!sessions.ready(&one.job_id, 2));
        for viewer in [one.clone(), two] {
            let state = sessions.get(&p, &viewer.id, 2).unwrap();
            assert_eq!(state.status, Status::Failed);
            assert_eq!(state.error_code.as_deref(), Some("source_connection_limit"));
            assert!(sessions.issue_media(&p, &viewer.id, 2).is_err());
            assert!(sessions.renew(&p, &viewer.id, 2).is_err());
        }
        assert_eq!(
            sessions
                .reserve(&p, &request("family"), &plan(), "three", 2)
                .unwrap_err(),
            "input_cleanup_pending"
        );
        assert!(sessions.finish_stop(&one.job_id));
        assert!(
            sessions
                .reserve(&p, &request("family"), &plan(), "three", 3)
                .unwrap()
                .start_job
        );
    }
}
