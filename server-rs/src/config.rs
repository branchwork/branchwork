use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

impl std::str::FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "max" => Ok(Effort::Max),
            _ => Err(format!("invalid effort: {s}")),
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effort::Low => write!(f, "low"),
            Effort::Medium => write!(f, "medium"),
            Effort::High => write!(f, "high"),
            Effort::Max => write!(f, "max"),
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "orchestrai", about = "orchestrAI dashboard server")]
pub struct Cli {
    /// Port to listen on
    #[arg(long, default_value_t = 3100)]
    pub port: u16,

    /// Effort level for spawned agents
    #[arg(long, value_enum, default_value_t = Effort::High)]
    pub effort: Effort,

    /// Path to .claude directory
    #[arg(long, default_value_os_t = default_claude_dir())]
    pub claude_dir: PathBuf,

    /// Webhook URL to notify on agent completion / phase advance.
    /// Accepts Slack incoming webhooks (posts `{"text": "..."}`) or any
    /// JSON-accepting endpoint. Falls back to `ORCHESTRAI_WEBHOOK_URL` env.
    #[arg(long, env = "ORCHESTRAI_WEBHOOK_URL")]
    pub webhook_url: Option<String>,
}

fn default_claude_dir() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".claude")
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub effort: Effort,
    pub claude_dir: PathBuf,
    pub plans_dir: PathBuf,
    pub db_path: PathBuf,
    pub webhook_url: Option<String>,
}

impl Config {
    pub fn from_cli(cli: Cli) -> Self {
        let claude_dir = cli.claude_dir;
        Self {
            port: cli.port,
            effort: cli.effort,
            plans_dir: claude_dir.join("plans"),
            db_path: claude_dir.join("orchestrai.db"),
            claude_dir,
            webhook_url: cli.webhook_url.filter(|s| !s.trim().is_empty()),
        }
    }
}
