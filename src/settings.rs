use std::env;
use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct Settings {
    /// hostname of this rendering server
    pub bind_to_host: String,
    /// port to listen on
    pub port: usize,
    /// Path to the CA certificate
    pub ca_cert_path: String,
    /// Path to the client certificate
    pub client_cert_path: String,
    /// Path to the clients certificate key
    pub client_key_path: String,
    /// Path to the revocation list
    pub revocation_list_path: String,
    /// Path to the folder where templates data are stored temporarily. Gets cleared on start
    pub temp_template_path: String,
    /// Max concurrent rendering threads
    pub max_rendering_threads: u64,
    /// Max runtime in seconds of a single export step (e.g. one pandoc/weasyprint/vivliostyle call) before it gets killed
    #[serde(default = "default_rendering_timeout_secs")]
    pub rendering_timeout_secs: u64,
}

fn default_rendering_timeout_secs() -> u64{
    600
}

impl Settings{
    pub fn new() -> Result<Self, ConfigError>{
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());

        let s = Config::builder().add_source(File::with_name("config/default"))
            .add_source( File::with_name(&format!("config/{}", run_mode))
                             .required(false),)
            .add_source(File::with_name("config/local").required(false))
            .add_source(Environment::with_prefix("app"))
            .build()?;

        let settings: Settings = s.try_deserialize()?;
        if settings.rendering_timeout_secs == 0{
            return Err(ConfigError::Message("rendering_timeout_secs must be greater than 0".to_string()));
        }
        Ok(settings)
    }
}