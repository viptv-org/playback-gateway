use std::{path::PathBuf, time::Duration};
use zeroize::Zeroizing;

pub(crate) struct Settings {
    pub endpoint: String,
    pub key: Zeroizing<String>,
    pub budget: PathBuf,
    pub daily_bytes: u64,
    pub sample_rate: f64,
    pub interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidEnableFlag,
    MissingLicenseKey,
    InvalidLicenseKey,
    InvalidRegion,
    InvalidBudget,
    InvalidSampleRate,
    InvalidInterval,
    BudgetUnavailable,
    ExporterUnavailable,
}

impl Settings {
    pub fn read(
        get: impl Fn(&str) -> Option<String>,
        default_budget: PathBuf,
    ) -> Result<Option<Self>, ConfigError> {
        match get("OBSERVABILITY_ENABLED").as_deref() {
            None | Some("") | Some("false") | Some("0") => return Ok(None),
            Some("true") | Some("1") => {}
            _ => return Err(ConfigError::InvalidEnableFlag),
        }
        let key =
            Zeroizing::new(get("NEW_RELIC_LICENSE_KEY").ok_or(ConfigError::MissingLicenseKey)?);
        if key.len() < 16 || key.len() > 512 || !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(ConfigError::InvalidLicenseKey);
        }
        let endpoint = match get("NEW_RELIC_REGION").as_deref().unwrap_or("US") {
            "US" => "https://otlp.nr-data.net:4318",
            "EU" => "https://otlp.eu01.nr-data.net:4318",
            _ => return Err(ConfigError::InvalidRegion),
        }
        .to_owned();
        let budget = get("OBSERVABILITY_BUDGET_PATH")
            .map(PathBuf::from)
            .unwrap_or(default_budget);
        if !budget.is_absolute() {
            return Err(ConfigError::InvalidBudget);
        }
        let daily_bytes = number(
            &get,
            "OBSERVABILITY_DAILY_BYTES",
            10 * 1024 * 1024,
            1024,
            50 * 1024 * 1024,
        )?;
        let sample_rate = get("OBSERVABILITY_TRACE_SAMPLE_RATE")
            .unwrap_or_else(|| "0.1".into())
            .parse::<f64>()
            .map_err(|_| ConfigError::InvalidSampleRate)?;
        if !sample_rate.is_finite() || !(0.0..=1.0).contains(&sample_rate) {
            return Err(ConfigError::InvalidSampleRate);
        }
        let seconds = get("OBSERVABILITY_EXPORT_INTERVAL_SECONDS")
            .unwrap_or_else(|| "30".into())
            .parse::<u64>()
            .map_err(|_| ConfigError::InvalidInterval)?;
        if !(10..=300).contains(&seconds) {
            return Err(ConfigError::InvalidInterval);
        }
        Ok(Some(Self {
            endpoint,
            key,
            budget,
            daily_bytes,
            sample_rate,
            interval: Duration::from_secs(seconds),
        }))
    }
}

fn number(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ConfigError> {
    let value = match get(key) {
        Some(value) => value.parse().map_err(|_| ConfigError::InvalidBudget)?,
        None => default,
    };
    if !(min..=max).contains(&value) {
        return Err(ConfigError::InvalidBudget);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_does_not_read_keys_or_create_state() {
        let result = Settings::read(
            |name| {
                assert_eq!(name, "OBSERVABILITY_ENABLED");
                None
            },
            "/not-created/budget".into(),
        )
        .unwrap();
        assert!(result.is_none());
    }
    #[test]
    fn validates_opt_in_without_echoing_input() {
        let get = |name: &str| match name {
            "OBSERVABILITY_ENABLED" => Some("true".into()),
            "NEW_RELIC_LICENSE_KEY" => Some("test-license-key-not-real".into()),
            _ => None,
        };
        assert!(Settings::read(get, "/tmp/budget".into()).unwrap().is_some());
        assert_eq!(
            Settings::read(|_| Some("private-input".into()), "/tmp/budget".into()).err(),
            Some(ConfigError::InvalidEnableFlag)
        );
    }
}
