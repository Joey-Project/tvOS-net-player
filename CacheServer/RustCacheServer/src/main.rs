use tvos_net_player_cache_server::{config::CacheServerOptions, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = CacheServerOptions::from_args(std::env::args().skip(1))?;
    run(options).await
}
