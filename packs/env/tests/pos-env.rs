use std::env;

pub fn load() -> Option<String> {
    let level = std::env::var("BAY_LOG_LEVEL").ok();
    let _ = env::var_os("BAY_HOME");
    level.or_else(|| option_env!("BAY_BUILD").map(str::to_owned))
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

pub fn port() -> String {
    required("BAY_PORT")
}
