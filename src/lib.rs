use std::io::Write;

use chrono::prelude::*;
use env_logger::Env;

pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:18888";

pub fn init_log() {
    const DEFAULT_LOG_LEVEL: &str = "info";

    env_logger::from_env(Env::default().default_filter_or(DEFAULT_LOG_LEVEL))
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:<5} {}:{}] {}",
                Local::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                record.level(),
                record.file().unwrap(),
                record.line().unwrap(),
                record.args()
            )
        })
        .init();
}
