//! Per-viewer in-flight media admission. Every media response holds its lane
//! until the body finishes or the connection drops, so one stalled viewer's
//! buffered media is a function of the lane limit, not of open sockets.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
};

pub(crate) const VIEWER_MEDIA_LIMIT: usize = 8;

pub(crate) struct MediaCapacity {
    lanes: Arc<Mutex<HashMap<String, Weak<ViewerLane>>>>,
}

struct ViewerLane {
    in_flight: AtomicUsize,
}

pub(crate) struct MediaLane {
    viewer: String,
    lane: Arc<ViewerLane>,
    lanes: Arc<Mutex<HashMap<String, Weak<ViewerLane>>>>,
}

impl MediaCapacity {
    pub(crate) fn new() -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub(crate) fn admit(&self, viewer: &str) -> Result<MediaLane, &'static str> {
        let mut lanes = self.lanes.lock().map_err(|_| "media_unavailable")?;
        let lane = match lanes.get(viewer).and_then(Weak::upgrade) {
            Some(lane) => lane,
            None => {
                let lane = Arc::new(ViewerLane {
                    in_flight: AtomicUsize::new(0),
                });
                lanes.insert(viewer.to_owned(), Arc::downgrade(&lane));
                lane
            }
        };
        let mut in_flight = lane.in_flight.load(Ordering::Acquire);
        loop {
            if in_flight >= VIEWER_MEDIA_LIMIT {
                return Err("media_capacity");
            }
            match lane.in_flight.compare_exchange_weak(
                in_flight,
                in_flight + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => in_flight = observed,
            }
        }
        Ok(MediaLane {
            viewer: viewer.to_owned(),
            lane,
            lanes: self.lanes.clone(),
        })
    }
}

impl MediaLane {
    pub(crate) fn hold(
        self,
        response: axum::response::Response,
    ) -> axum::response::Response {
        let (parts, body) = response.into_parts();
        let lane = self;
        // The stream owns the lane so the slot frees when the body finishes or
        // the client disconnects, not when this handler returns.
        let body = axum::body::Body::from_stream(futures::StreamExt::map(
            body.into_data_stream(),
            move |chunk| {
                let _ = &lane;
                chunk
            },
        ));
        axum::response::Response::from_parts(parts, body)
    }
}

impl Drop for MediaLane {
    fn drop(&mut self) {
        if self.lane.in_flight.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // A reuse that landed before this lock already raised the counter;
        // remove only a lane that stayed empty and is this allocation.
        if let Ok(mut lanes) = self.lanes.lock() {
            if self.lane.in_flight.load(Ordering::Acquire) == 0
                && lanes
                    .get(&self.viewer)
                    .is_some_and(|entry| entry.ptr_eq(&Arc::downgrade(&self.lane)))
            {
                lanes.remove(&self.viewer);
            }
        }
    }
}
