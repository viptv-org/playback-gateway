use crate::{Failure, Observation, Operation, Outcome, Telemetry};
use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use opentelemetry::{propagation::TextMapPropagator, trace::SpanKind};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

pub async fn observe(State(telemetry): State<Telemetry>, request: Request, next: Next) -> Response {
    if !telemetry.enabled() {
        return next.run(request).await;
    }
    let operation = Operation::route(request.uri().path());
    let mut propagation = http::HeaderMap::new();
    if let Some(value) = request
        .headers()
        .get("traceparent")
        .filter(|value| value.as_bytes().len() == 55)
    {
        propagation.insert("traceparent", value.clone());
    }
    // Baggage and tracestate may contain account/user data and are not imported.
    let parent =
        TraceContextPropagator::new().extract(&opentelemetry_http::HeaderExtractor(&propagation));
    let mut observation = telemetry.start(operation, parent, SpanKind::Server);
    observation.method(request.method().as_str());
    let response = crate::in_context(observation.context(), next.run(request)).await;
    let status = response.status().as_u16();
    if operation != Operation::Media || response.body().is_end_stream() {
        observation.finish(Outcome::Http(status));
        return response;
    }
    observation.response_status(status);
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(MeteredBody {
            body,
            observation: Some(observation),
            status,
        }),
    )
}

struct MeteredBody {
    body: Body,
    observation: Option<Observation>,
    status: u16,
}
impl HttpBody for MeteredBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.body).poll_frame(cx);
        match &frame {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(observation), Some(data)) = (&mut this.observation, frame.data_ref()) {
                    observation.bytes(data.len());
                }
            }
            Poll::Ready(None) => {
                if let Some(observation) = this.observation.take() {
                    observation.finish(Outcome::Http(this.status));
                }
            }
            Poll::Ready(Some(Err(_))) => {
                if let Some(observation) = this.observation.take() {
                    observation.finish(Outcome::Failed(Failure::Network));
                }
            }
            Poll::Pending => {}
        }
        frame
    }
    fn is_end_stream(&self) -> bool {
        self.observation.is_none() && self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
