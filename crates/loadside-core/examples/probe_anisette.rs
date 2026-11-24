//! Connectivity check: anisette server + Apple's GrandSlam URL bag.
//! Sends no account data. `cargo run -p loadside-core --example probe_anisette`
use isideload::{anisette::remote_v3::RemoteV3AnisetteProvider, auth::builder::AppleAccountBuilder, util::storage::InMemoryStorage};

#[tokio::main]
async fn main() {
    loadside_core::init();
    let cfg = loadside_core::config::Config::default();
    let provider = RemoteV3AnisetteProvider::new(&cfg.anisette_url, Box::new(InMemoryStorage::new()), "0".into()).unwrap();
    match AppleAccountBuilder::new("probe@example.com").anisette_provider(provider).build().await {
        Ok(_) => println!("OK: anisette client info + Apple URL bag reachable"),
        Err(e) => println!("FAILED: {e}"),
    }
}
