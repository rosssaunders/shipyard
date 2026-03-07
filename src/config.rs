use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub llm_endpoint: String,
    pub llm_model: String,
    pub api_key: String,
    pub data_dir: PathBuf,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            llm_endpoint: std::env::var("SHIPYARD_LLM_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:3000/v1".to_string()),
            llm_model: std::env::var("SHIPYARD_LLM_MODEL")
                .unwrap_or_else(|_| "gpt-5.4".to_string()),
            api_key: std::env::var("SHIPYARD_API_KEY").unwrap_or_default(),
            data_dir: std::env::var("SHIPYARD_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./data")),
            port: std::env::var("SHIPYARD_PORT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(3001),
        }
    }
}
