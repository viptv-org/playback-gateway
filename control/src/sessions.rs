//! Atomic lifecycle bookkeeping. Engine work happens outside this lock; stopping
//! jobs retain their admission reservation until the engine confirms teardown.
use crate::{
    credentials::{hash, random_hex, Credentials, Principal, Scope},
    protocol::{CreateSession, IDLE_GRACE_SECONDS, VIEWER_LEASE_SECONDS},
};
use serde::Serialize;
mod captions;
use std::collections::{BTreeMap, BTreeSet};

/// Use the engine's inspected plan, never a parallel control-layer approximation.
pub use playback_engine::OutputPlan as ExecutionPlan;

/// Progressive same-container remux: one private reader per viewer over the
/// shared live ingest. Such outputs are never joined by another viewer.
pub fn single_consumer(plan: &ExecutionPlan) -> bool {
    plan.segmentation == "progressive"
        && plan.shared_ingest
        && !plan.shareable
        && matches!(plan.format.as_str(), "mpegts" | "matroska")
}
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
    source: [u8; 32],
    fingerprint: String,
    state: JobState,
    created: u64,
    startup_seconds: u64,
    idle_since: Option<u64>,
    viewers: BTreeSet<String>,
    reservations: BTreeSet<String>,
}
struct OwnedViewer {
    view: Viewer,
    owner: String,
    request_hash: [u8; 32],
    client_frame_ack: bool,
    namespace: String,
    idempotency: String,
    media_hash: Option<[u8; 32]>,
    media_token: Option<String>,
    captions: BTreeMap<String, std::sync::Arc<str>>,
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
    preparing: BTreeMap<String, PendingSource>,
    idle_grace: u64,
    max_records: usize,
}
struct PendingSource {
    owner: String,
    source: [u8; 32],
    request: [u8; 32],
}
impl Default for Sessions {
    fn default() -> Self {
        Self {
            jobs: BTreeMap::new(),
            viewers: BTreeMap::new(),
            preparing: BTreeMap::new(),
            idle_grace: IDLE_GRACE_SECONDS,
            max_records: 16384,
        }
    }
}
impl Sessions {
    /// Reject a new opt-in transport before it allocates cache or contacts peers.
    /// Existing source preparations may still join when their input slot is full.
    pub fn check_source_admission(
        &self,
        principal: &Principal,
        request: &CreateSession,
        now: u64,
    ) -> Result<(), &'static str> {
        let available = self.available(principal, now);
        if available.viewers == 0 {
            return Err("viewer_capacity");
        }
        let existing = self.jobs.values().any(|job| {
            job.source == source_identity(request)
                && job.reservations.contains(&principal.key.id)
                && job.state != JobState::Stopping
        }) || self.preparing.values().any(|pending| {
            pending.owner == principal.key.id && pending.source == source_identity(request)
        });
        if !existing && available.inputs == 0 {
            return Err("input_capacity");
        }
        if !existing && available.outputs == 0 {
            return Err("output_capacity");
        }
        Ok(())
    }
    pub fn begin_source_preparation(
        &mut self,
        principal: &Principal,
        request: &CreateSession,
        now: u64,
    ) -> Result<String, &'static str> {
        self.check_source_admission(principal, request, now)?;
        if self.preparing.len() >= 1024 {
            return Err("input_capacity");
        }
        let id = random_hex(16);
        self.preparing.insert(
            id.clone(),
            PendingSource {
                owner: principal.key.id.clone(),
                source: source_identity(request),
                request: hash(
                    &serde_json::to_string(&(request, request.client_frame_ack))
                        .map_err(|_| "invalid_request")?,
                ),
            },
        );
        Ok(id)
    }
    pub fn finish_source_preparation(&mut self, id: &str) {
        self.preparing.remove(id);
    }
    pub fn reserve_prepared_source(
        &mut self,
        pending: &str,
        principal: &Principal,
        request: &CreateSession,
        plan: &ExecutionPlan,
        idempotency: &str,
        now: u64,
    ) -> Result<Reservation, &'static str> {
        let record = self
            .preparing
            .remove(pending)
            .ok_or("input_cleanup_pending")?;
        let result = self.reserve(principal, request, plan, idempotency, now);
        if result.is_err() {
            self.preparing.insert(pending.to_owned(), record);
        }
        result
    }
    fn pending_counts(&self, principal: &Principal) -> (u32, u32, u32) {
        let pending: Vec<_> = self
            .preparing
            .values()
            .filter(|pending| pending.owner == principal.key.id)
            .collect();
        let active_sources: BTreeSet<_> = self
            .jobs
            .values()
            .filter(|job| job.reservations.contains(&principal.key.id))
            .map(|job| job.source)
            .collect();
        let active_requests: BTreeSet<_> = self
            .viewers
            .values()
            .filter(|viewer| {
                viewer.owner == principal.key.id
                    && matches!(viewer.view.status, Status::Starting | Status::Ready)
            })
            .map(|viewer| viewer.request_hash)
            .collect();
        let inputs = pending
            .iter()
            .filter(|pending| !active_sources.contains(&pending.source))
            .map(|pending| pending.source)
            .collect::<BTreeSet<_>>()
            .len() as u32;
        let outputs = pending
            .iter()
            .filter(|pending| !active_requests.contains(&pending.request))
            .map(|pending| pending.request)
            .collect::<BTreeSet<_>>()
            .len() as u32;
        (inputs, outputs, pending.len() as u32)
    }
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
        let (pending_inputs, pending_outputs, pending_viewers) = self.pending_counts(principal);
        crate::credentials::Quotas {
            inputs: principal
                .key
                .spec
                .quotas
                .inputs
                .saturating_sub(inputs.saturating_add(pending_inputs)),
            outputs: principal
                .key
                .spec
                .quotas
                .outputs
                .saturating_sub((jobs.len() as u32).saturating_add(pending_outputs)),
            viewers: principal
                .key
                .spec
                .quotas
                .viewers
                .saturating_sub(viewers.saturating_add(pending_viewers)),
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
        let request_hash = hash(
            &serde_json::to_string(&(request, request.client_frame_ack))
                .map_err(|_| "invalid_request")?,
        );
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
        let single = single_consumer(plan);
        if !plan.shareable && !(single && request.input.live) {
            return Err("unsupported_output");
        }
        let body = serde_json::to_string(&(request, request.client_frame_ack))
            .map_err(|_| "invalid_request")?;
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
        if own.len() + self.pending_counts(principal).2 as usize
            >= principal.key.spec.quotas.viewers as usize
        {
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
            request.input.live,
            request.input.file_index,
            request.client_frame_ack,
            // A seek starts another provider pipeline. Only live outputs have
            // a common realtime origin independent of the output plan.
            if request.input.live {
                0
            } else {
                plan.position_micros
            }
        ]);
        // Without a physical shared ingest, each distinct normalized output
        // starts another upstream pipeline. Identical plans still join a job.
        let input = if request.input.live && plan.shared_ingest {
            hex(&hash(&identity.to_string()))
        } else {
            hex(&hash(&serde_json::json!([identity, plan]).to_string()))
        };
        // A single-consumer output is a distinct job for each viewer; only its
        // input identity (and so the input quota) is shared.
        let fingerprint = if single {
            format!("single_{}", random_hex(16))
        } else {
            hex(&hash(&serde_json::json!([input, plan]).to_string()))
        };
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
        let pending_source = self.preparing.values().any(|pending| {
            pending.owner == principal.key.id && pending.source == source_identity(request)
        });
        let pending_output = self
            .preparing
            .values()
            .any(|pending| pending.owner == principal.key.id && pending.request == request_hash);
        if !own_inputs.contains(input.as_str())
            && (own_inputs.len() >= principal.key.spec.quotas.inputs as usize
                || (self.available(principal, now).inputs == 0 && !pending_source))
        {
            return Err("input_capacity");
        }
        if !existing.is_some_and(|job| own_jobs.contains(job.id.as_str()))
            && (own_jobs.len() >= principal.key.spec.quotas.outputs as usize
                || (self.available(principal, now).outputs == 0 && !pending_output))
        {
            return Err("output_capacity");
        }
        let (job_id, start_job) = existing
            .map(|job| (job.id.clone(), false))
            .unwrap_or_else(|| (format!("job_{}", random_hex(16)), true));
        let job = self.jobs.entry(job_id.clone()).or_insert_with(|| Job {
            id: job_id.clone(),
            input,
            source: source_identity(request),
            fingerprint,
            state: JobState::Starting,
            created: now,
            startup_seconds: request.input.startup_seconds(),
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
                client_frame_ack: request.client_frame_ack,
                namespace: request.namespace.clone(),
                idempotency: idempotency.into(),
                media_hash: None,
                media_token: None,
                captions: BTreeMap::new(),
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
            || now.saturating_sub(job.created) >= job.startup_seconds
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
    pub fn first_frame_viewer(
        &self,
        principal: &Principal,
        id: &str,
        now: u64,
    ) -> Result<Viewer, &'static str> {
        if !principal.key.spec.scopes.contains(&Scope::Renew) {
            return Err("forbidden");
        }
        let view = self.get(principal, id, now)?;
        if !self
            .viewers
            .get(id)
            .is_some_and(|viewer| viewer.client_frame_ack)
        {
            return Err("unsupported_output");
        }
        if matches!(
            view.status,
            Status::Expired | Status::Released | Status::Failed
        ) {
            return Err("session_expired");
        }
        principal.require(
            &self.viewers.get(id).ok_or("session_not_found")?.namespace,
            Scope::Renew,
        )?;
        if view.status != Status::Ready {
            return Err("session_not_ready");
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
            tracing::warn!(session_tag = %crate::diagnostics::tag(id), expires_at = viewer.view.expires_at, now, "Viewer renewal refused: expired or terminal");
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
        tracing::warn!(job_tag = %crate::diagnostics::tag(id), error_code = failure.code(), viewers = job.viewers.len(), "Output job failed");
        for id in &job.viewers {
            if let Some(viewer) = self.viewers.get_mut(id) {
                if matches!(viewer.view.status, Status::Starting | Status::Ready) {
                    viewer.view.status = Status::Failed;
                    viewer.view.error_code = Some(failure.code().into());
                    viewer.view.expires_at = now;
                    viewer.media_hash = None;
                    viewer.captions.clear();
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
        tracing::info!(session_tag = %crate::diagnostics::tag(id), status = ?status, expires_at = viewer.view.expires_at, now, "Viewer detached");
        viewer.media_token = None;
        viewer.media_hash = None;
        viewer.captions.clear();
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
                && now.saturating_sub(job.created) >= job.startup_seconds;
            if (timed_out
                || job
                    .idle_since
                    .is_some_and(|at| now.saturating_sub(at) >= self.idle_grace))
                && job.state != JobState::Stopping
            {
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

fn source_identity(request: &CreateSession) -> [u8; 32] {
    let headers: BTreeMap<_, _> = request
        .input
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect();
    hash(
        &serde_json::json!([
            request.namespace,
            request.input.url,
            headers,
            request.input.live,
            request.input.file_index,
            request.client_frame_ack,
            request.input.archive_index,
            request.input.expected_file_size,
            request.input.trackers,
            request.position_seconds
        ])
        .to_string(),
    )
}
fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_ack_mode_fences_sharing_replay_scope_and_expiry() {
        let principal = principal("owner", "fixture");
        let mut legacy = request("fixture");
        legacy.input.url = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111".into();
        legacy.input.live = false;
        let mut modern = legacy.clone();
        modern.client_frame_ack = true;
        let mut sessions = Sessions::default();
        let old = sessions
            .reserve(&principal, &legacy, &plan(), "legacy", 0)
            .unwrap();
        let new = sessions
            .reserve(&principal, &modern, &plan(), "modern", 0)
            .unwrap();
        assert_ne!(old.viewer.job_id, new.viewer.job_id);
        assert_eq!(
            sessions.replay(&principal, &modern, "legacy", 0).err(),
            Some("idempotency_conflict")
        );
        assert_eq!(
            sessions
                .first_frame_viewer(&principal, &old.viewer.id, 0)
                .err(),
            Some("unsupported_output")
        );
        assert_eq!(
            sessions
                .first_frame_viewer(&principal, &new.viewer.id, 0)
                .err(),
            Some("session_not_ready")
        );
        sessions.ready(&new.viewer.job_id, 1);
        let before = sessions
            .get(&principal, &new.viewer.id, 1)
            .unwrap()
            .expires_at;
        assert!(sessions
            .first_frame_viewer(&principal, &new.viewer.id, 1)
            .is_ok());
        assert_eq!(
            sessions
                .get(&principal, &new.viewer.id, 1)
                .unwrap()
                .expires_at,
            before
        );
        let other = self::principal("other", "fixture");
        assert_eq!(
            sessions.first_frame_viewer(&other, &new.viewer.id, 1).err(),
            Some("session_not_found")
        );
        let mut denied_namespace = principal.clone();
        denied_namespace.key.spec.namespaces = vec!["different".into()];
        assert_eq!(
            sessions
                .first_frame_viewer(&denied_namespace, &new.viewer.id, 1)
                .err(),
            Some("forbidden")
        );
        assert_eq!(
            sessions
                .first_frame_viewer(&principal, &new.viewer.id, before)
                .err(),
            Some("session_expired")
        );
        sessions.release(&principal, &new.viewer.id, 2).unwrap();
        assert_eq!(
            sessions
                .first_frame_viewer(&principal, &new.viewer.id, 2)
                .err(),
            Some("session_expired")
        );
        let mut wire = serde_json::to_value(&legacy).unwrap();
        wire["client_frame_ack"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<CreateSession>(wire).is_err(),
            "v1 must not accept the private route-selected mode"
        );
    }
    #[test]
    fn pending_source_admission_blocks_another_input_and_can_promote_shared_viewers() {
        let mut sessions = Sessions::default();
        let mut principal = principal("key", "one");
        principal.key.spec.quotas.inputs = 1;
        let first = request("one");
        let initial = sessions
            .begin_source_preparation(&principal, &first, 0)
            .unwrap();
        assert_eq!(sessions.available(&principal, 0).inputs, 0);
        let shared = sessions
            .begin_source_preparation(&principal, &first, 0)
            .unwrap();
        let mut other = first.clone();
        other.input.url = "https://another.invalid/movie".into();
        assert_eq!(
            sessions
                .begin_source_preparation(&principal, &other, 0)
                .err(),
            Some("input_capacity")
        );
        let view = sessions
            .reserve_prepared_source(&initial, &principal, &first, &plan(), "first", 0)
            .unwrap();
        let second = sessions
            .reserve_prepared_source(&shared, &principal, &first, &plan(), "second", 0)
            .unwrap();
        assert_eq!(view.viewer.job_id, second.viewer.job_id);
        assert_eq!(sessions.available(&principal, 0).inputs, 0);
        sessions.finish_source_preparation(&initial);
        assert_eq!(sessions.available(&principal, 0).inputs, 0);
    }
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
            client_frame_ack: false,
            namespace: namespace.into(),
            input: Input {
                url: "https://provider.invalid/channel".into(),
                headers: BTreeMap::new(),
                live: true,
                file_index: None,
                archive_index: None,
                expected_file_size: None,
                trackers: Vec::new(),
            },
            output: Output {
                conversion: crate::protocol::Conversion::Auto,
                audio_language: None,
                preferred_audio_language: None,
                preferred_subtitle_language: None,
                subtitles_off: false,
                protocol: OutputProtocol::Hls,
                video_codecs: vec!["h264".into()],
                audio_codecs: vec!["aac".into()],
                max_width: 3840,
                max_height: 2160,
                audio_track: None,
                required_audio_index: None,
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
            shared_ingest: true,
        }
    }
    fn progressive_plan() -> ExecutionPlan {
        ExecutionPlan {
            format: "mpegts".into(),
            segmentation: "progressive".into(),
            shareable: false,
            ..plan()
        }
    }
    #[test]
    fn progressive_viewers_get_private_jobs_over_one_counted_input() {
        let p = principal("one", "family");
        let mut sessions = Sessions::default();
        let mut progressive = request("family");
        progressive.output.protocol = OutputProtocol::Progressive;
        let first = sessions
            .reserve(&p, &progressive, &progressive_plan(), "p-1", 0)
            .unwrap();
        let second = sessions
            .reserve(&p, &progressive, &progressive_plan(), "p-2", 0)
            .unwrap();
        // Each viewer starts its own reader; neither joins the other's output.
        assert!(first.start_job && second.start_job);
        assert_ne!(first.viewer.job_id, second.viewer.job_id);
        // An HLS output of the same live input shares the same physical input.
        let hls = sessions
            .reserve(&p, &request("family"), &plan(), "h-1", 0)
            .unwrap();
        assert!(hls.start_job);
        let available = sessions.available(&p, 0);
        assert_eq!(available.inputs, 1);
        assert_eq!(available.outputs, 0);
        // The output quota bounds readers; a fourth output is refused.
        assert_eq!(
            sessions
                .reserve(&p, &progressive, &progressive_plan(), "p-3", 0)
                .unwrap_err(),
            "output_capacity"
        );
        // Releasing one reader stops only its own job after the idle grace.
        assert!(sessions.ready(&first.viewer.job_id, 1));
        assert!(sessions.ready(&second.viewer.job_id, 1));
        assert!(sessions.ready(&hls.viewer.job_id, 1));
        sessions.release(&p, &first.viewer.id, 2).unwrap();
        assert_eq!(sessions.reap(40), vec![first.viewer.job_id.clone()]);
        assert_eq!(
            sessions.get(&p, &second.viewer.id, 40).unwrap().status,
            Status::Ready
        );
        // Single-consumer plans are never accepted for on-demand input or
        // without the shared ingest.
        let mut on_demand = progressive.clone();
        on_demand.input.live = false;
        assert_eq!(
            sessions
                .reserve(&p, &on_demand, &progressive_plan(), "p-4", 40)
                .unwrap_err(),
            "unsupported_output"
        );
        let unshared = ExecutionPlan {
            shared_ingest: false,
            ..progressive_plan()
        };
        assert!(!single_consumer(&unshared));
        assert!(!single_consumer(&ExecutionPlan {
            shareable: false,
            ..plan()
        }));
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
    fn distinct_vod_origins_reserve_distinct_inputs_until_actual_teardown() {
        let mut p = principal("one", "family");
        p.key.spec.quotas.inputs = 1;
        let mut sessions = Sessions::default();
        let mut source = request("family");
        source.input.live = false;
        let first = sessions.reserve(&p, &source, &plan(), "first", 0).unwrap();
        source.position_seconds = 30.0;
        let mut sought = plan();
        sought.position_micros = 30_000_000;
        assert_eq!(
            sessions
                .reserve(&p, &source, &sought, "sought", 1)
                .unwrap_err(),
            "input_capacity"
        );
        sessions.release(&p, &first.viewer.id, 1).unwrap();
        assert_eq!(sessions.reap(16), vec![first.viewer.job_id.clone()]);
        assert_eq!(
            sessions
                .reserve(&p, &source, &sought, "sought", 16)
                .unwrap_err(),
            "input_capacity"
        );
        sessions.finish_stop(&first.viewer.job_id);
        assert!(
            sessions
                .reserve(&p, &source, &sought, "sought", 17)
                .unwrap()
                .start_job
        );
    }

    #[test]
    fn different_live_outputs_share_input_admission_but_credential_changes_do_not() {
        let mut p = principal("one", "family");
        p.key.spec.quotas.inputs = 1;
        let mut sessions = Sessions::default();
        let mut source = request("family");
        source
            .input
            .headers
            .insert("Authorization".into(), "Bearer fixture-a".into());
        sessions.reserve(&p, &source, &plan(), "first", 0).unwrap();
        let mut encoded = plan();
        encoded.video_mode = "encode".into();
        encoded.mode = "transcode".into();
        let second = sessions
            .reserve(&p, &source, &encoded, "second", 1)
            .unwrap();
        assert!(second.start_job);
        assert_eq!(sessions.available(&p, 1).inputs, 0);
        source
            .input
            .headers
            .insert("Authorization".into(), "Bearer fixture-b".into());
        assert_eq!(
            sessions
                .reserve(&p, &source, &encoded, "rotated", 2)
                .unwrap_err(),
            "input_capacity"
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
    fn fallback_plans_charge_physical_inputs_and_hold_them_until_teardown() {
        let p = principal("one", "family");
        let mut sessions = Sessions::default();
        let source = request("family");
        let mut fallback = plan();
        fallback.shared_ingest = false;
        let first = sessions.reserve(&p, &source, &fallback, "one", 0).unwrap();
        let same = sessions.reserve(&p, &source, &fallback, "same", 0).unwrap();
        assert!(!same.start_job);
        assert_eq!(same.viewer.job_id, first.viewer.job_id);
        let mut distinct = fallback.clone();
        distinct.video_mode = "encode".into();
        distinct.mode = "transcode".into();
        let second = sessions.reserve(&p, &source, &distinct, "two", 0).unwrap();
        assert!(second.start_job);
        assert_eq!(sessions.available(&p, 0).inputs, 0);
        let mut third = distinct.clone();
        third.width = 640;
        assert_eq!(
            sessions
                .reserve(&p, &source, &third, "three", 1)
                .unwrap_err(),
            "input_capacity"
        );
        sessions.release(&p, &first.viewer.id, 1).unwrap();
        sessions.release(&p, &same.viewer.id, 1).unwrap();
        sessions.reap(16);
        assert_eq!(
            sessions
                .reserve(&p, &source, &third, "three", 16)
                .unwrap_err(),
            "input_capacity"
        );
        sessions.finish_stop(&first.viewer.job_id);
        assert!(
            sessions
                .reserve(&p, &source, &third, "three", 17)
                .unwrap()
                .start_job
        );
        let serialized = serde_json::to_string(&fallback).unwrap();
        assert!(!serialized.contains("shared_ingest"));
    }
    #[test]
    fn vod_output_pipelines_cannot_claim_realtime_input_sharing() {
        let mut p = principal("one", "family");
        p.key.spec.quotas.inputs = 1;
        let mut source = request("family");
        source.input.live = false;
        let mut sessions = Sessions::default();
        sessions.reserve(&p, &source, &plan(), "first", 0).unwrap();
        let mut other = plan();
        other.width = 640;
        other.video_mode = "encode".into();
        assert_eq!(
            sessions
                .reserve(&p, &source, &other, "second", 1)
                .unwrap_err(),
            "input_capacity"
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
        assert_eq!(
            sessions.available(&p, 17).inputs,
            0,
            "cleanup must remain charged in capacity hints"
        );
        let mut next = request("family");
        next.input.url.push_str("/other");
        assert_eq!(
            sessions.reserve(&p, &next, &plan(), "b", 17).unwrap_err(),
            "input_capacity"
        );
        sessions.finish_stop(&first.viewer.job_id);
        assert_eq!(sessions.available(&p, 18).inputs, 1);
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
