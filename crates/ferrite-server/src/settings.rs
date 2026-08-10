//! Environment-driven configuration. Kept to environment variables rather
//! than a config file so the container image needs nothing mounted to boot.

use std::path::PathBuf;

/// The PostgreSQL port, so existing clients and connection strings need no
/// adjustment.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:5432";
pub const DEFAULT_USER: &str = "ferrite";
/// Relative, so a container that mounts nothing still starts; a deployment
/// that wants the data to survive the container points `FERRITE_DATA` at a
/// volume.
pub const DEFAULT_DATA_DIR: &str = "./data";
/// The port `postgres_exporter` conventionally uses, so existing Prometheus
/// scrape configurations and dashboards need no adjustment. Separate from
/// 5432 on purpose — see the `ferrite-metrics` crate docs.
pub const DEFAULT_METRICS_LISTEN: &str = "0.0.0.0:9187";

#[derive(Debug, Clone)]
pub struct Settings {
    pub listen: String,
    pub user: String,
    pub password: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Opt-out only: TLS is on unless this is explicitly set.
    pub tls_disabled: bool,
    /// Directory holding `ferrite.db` and `ferrite.wal`.
    pub data_dir: PathBuf,
    /// Where the Prometheus/health endpoint listens. `None` disables it.
    pub metrics_listen: Option<String>,
}

impl Settings {
    pub fn from_env() -> Result<Self, String> {
        let settings = Self {
            listen: var("FERRITE_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.to_owned()),
            user: var("FERRITE_USER").unwrap_or_else(|| DEFAULT_USER.to_owned()),
            password: var("FERRITE_PASSWORD"),
            tls_cert: var("FERRITE_TLS_CERT").map(PathBuf::from),
            tls_key: var("FERRITE_TLS_KEY").map(PathBuf::from),
            tls_disabled: is_truthy("FERRITE_TLS_DISABLE"),
            data_dir: var("FERRITE_DATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR)),
            metrics_listen: if is_truthy("FERRITE_METRICS_DISABLE") {
                None
            } else {
                Some(
                    var("FERRITE_METRICS_LISTEN")
                        .unwrap_or_else(|| DEFAULT_METRICS_LISTEN.to_owned()),
                )
            },
        };
        if settings.tls_cert.is_some() != settings.tls_key.is_some() {
            return Err("FERRITE_TLS_CERT and FERRITE_TLS_KEY must be set together".to_owned());
        }
        Ok(settings)
    }

    pub fn tls_description(&self) -> &'static str {
        match (self.tls_disabled, self.tls_cert.is_some()) {
            (true, _) => "disabled",
            (false, true) => "configured certificate",
            (false, false) => "ephemeral self-signed certificate",
        }
    }
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn is_truthy(key: &str) -> bool {
    var(key).is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_is_only_off_when_explicitly_disabled() {
        let mut settings = Settings {
            listen: DEFAULT_LISTEN.to_owned(),
            user: DEFAULT_USER.to_owned(),
            password: None,
            tls_cert: None,
            tls_key: None,
            tls_disabled: false,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            metrics_listen: Some(DEFAULT_METRICS_LISTEN.to_owned()),
        };
        assert_eq!(
            settings.tls_description(),
            "ephemeral self-signed certificate"
        );
        settings.tls_cert = Some(PathBuf::from("server.crt"));
        assert_eq!(settings.tls_description(), "configured certificate");
        settings.tls_disabled = true;
        assert_eq!(settings.tls_description(), "disabled");
    }
}
