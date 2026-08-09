use tracing_subscriber::EnvFilter;

const DEFAULT_FILTER: &str = "warn,slik=info,ort=error";

pub(crate) fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER)),
        )
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use tracing::Level;
    use tracing_subscriber::prelude::*;

    use super::*;

    #[test]
    fn default_filter_focuses_on_application_events() {
        let subscriber = tracing_subscriber::registry().with(EnvFilter::new(DEFAULT_FILTER));

        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: "slik", Level::INFO));
            assert!(!tracing::enabled!(target: "ort::logging", Level::INFO));
            assert!(!tracing::enabled!(target: "ort::logging", Level::WARN));
        });
    }
}
