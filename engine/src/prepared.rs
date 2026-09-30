//! Preparation retains admission and inspected media facts without starting an
//! output worker. Dropping a preparation releases its reservations; consuming it
//! starts exactly the output represented by its normalized plan.
use crate::PlaybackResponse;
use serde::Serialize;
use std::{future::Future, pin::Pin};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutputPlan {
    pub mode: String,
    pub format: String,
    pub video_mode: String,
    pub audio_mode: String,
    pub video_codec: String,
    pub audio_codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub audio_track: Option<u32>,
    pub subtitle_track: Option<u32>,
    pub position_micros: u64,
    /// Includes segment-container/cut policy, not caller capability reports.
    pub segmentation: String,
    /// Streaming fMP4 has a single-consumer transport. Do not share it until
    /// that transport is replaced by a multi-reader implementation.
    pub shareable: bool,
    /// Internal physical execution fact, never part of the public protocol.
    /// Only eligible live outputs read one independently owned source ingest.
    #[serde(skip)]
    pub shared_ingest: bool,
}

pub struct PreparedPlayback<'a> {
    /// None only for the legacy client-inspection path, whose output is not yet
    /// known. A gateway coordinator must not use that path for shared workers.
    pub plan: Option<OutputPlan>,
    run: Pin<Box<dyn Future<Output = Result<PlaybackResponse, String>> + Send + 'a>>,
}
impl<'a> PreparedPlayback<'a> {
    pub(crate) fn new(
        plan: Option<OutputPlan>,
        run: impl Future<Output = Result<PlaybackResponse, String>> + Send + 'a,
    ) -> Self {
        Self {
            plan,
            run: Box::pin(run),
        }
    }
    pub async fn start(self) -> Result<PlaybackResponse, String> {
        self.run.await
    }
}
