#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Api,
    PlaybackStart,
    PlaybackSession,
    PlaybackHeartbeat,
    Discovery,
    GatewayCapabilities,
    GatewayControl,
    GatewayDns,
    GatewayPrepare,
    EngineStart,
    EngineHealth,
    GatewayRenew,
    GatewaySession,
    Media,
}
impl Operation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Api => "api.request",
            Self::PlaybackStart => "playback.start",
            Self::PlaybackSession => "playback.session",
            Self::PlaybackHeartbeat => "playback.heartbeat",
            Self::Discovery => "source.discovery",
            Self::GatewayCapabilities => "gateway.capabilities",
            Self::GatewayControl => "gateway.control",
            Self::GatewayDns => "gateway.dns",
            Self::GatewayPrepare => "gateway.prepare",
            Self::EngineStart => "engine.start",
            Self::EngineHealth => "engine.health",
            Self::GatewayRenew => "gateway.renew",
            Self::GatewaySession => "gateway.session",
            Self::Media => "media.delivery",
        }
    }
    pub fn route(path: &str) -> Self {
        let path = path.strip_prefix("/api").unwrap_or(path);
        if path == "/v2/playback" {
            Self::PlaybackStart
        } else if path.starts_with("/v2/playback/") && path.ends_with("/heartbeat") {
            Self::PlaybackHeartbeat
        } else if path.starts_with("/v2/playback/") {
            Self::PlaybackSession
        } else if path.starts_with("/v2/streams") {
            Self::Discovery
        } else if path == "/v1/sessions" {
            Self::GatewayPrepare
        } else if path.starts_with("/v1/sessions/") && path.ends_with("/renew") {
            Self::GatewayRenew
        } else if path.starts_with("/v1/sessions/") {
            Self::GatewaySession
        } else if path == "/v1/capabilities" {
            Self::GatewayCapabilities
        } else if path.starts_with("/media/") {
            Self::Media
        } else {
            Self::Api
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Authorization,
    Network,
    Dns,
    Timeout,
    Capacity,
    Source,
    Decode,
    Internal,
    Cancelled,
}
impl Failure {
    pub fn from_code(code: &str) -> Self {
        match code {
            "gateway_dns_unavailable" => Self::Dns,
            "gateway_unavailable" | "connection_failed" => Self::Network,
            "gateway_startup_timeout" | "startup_timeout" => Self::Timeout,
            "gateway_capacity"
            | "input_capacity"
            | "output_capacity"
            | "viewer_capacity"
            | "gateway_checks_busy" => Self::Capacity,
            "source_unavailable" | "source_preparation_failed" => Self::Source,
            "gateway_processing_failed" | "processing_failed" | "delivery_unsupported" => {
                Self::Decode
            }
            "unauthorized"
            | "forbidden"
            | "gateway_key_rejected"
            | "gateway_scope_missing"
            | "playback_expired"
            | "session_expired" => Self::Authorization,
            _ => Self::Internal,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Authorization => "authorization",
            Self::Network => "network",
            Self::Dns => "dns",
            Self::Timeout => "timeout",
            Self::Capacity => "capacity",
            Self::Source => "source",
            Self::Decode => "decode",
            Self::Internal => "internal",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Success,
    Http(u16),
    Failed(Failure),
}
impl Outcome {
    pub(crate) fn failure(self) -> Option<Failure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Http(401 | 403 | 410) => Some(Failure::Authorization),
            Self::Http(408 | 504) => Some(Failure::Timeout),
            Self::Http(429) => Some(Failure::Capacity),
            Self::Http(400..=499) => Some(Failure::Source),
            Self::Http(500..=599) => Some(Failure::Internal),
            _ => None,
        }
    }
}
