#![no_main]
//! The server list is JSON fetched from a user-supplied URL, then filtered.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(servers) = serde_json::from_slice::<Vec<librespeed_cli::defs::Server>>(data) else {
        return;
    };
    let out = librespeed_cli::output::Output::default();
    for server in &servers {
        let _ = server.sponsor(out);
        let _ = server.get_url();
    }
    let _ = librespeed_cli::speedtest::preprocess_servers(
        servers,
        librespeed_cli::speedtest::ForceScheme::Nothing,
        &[],
        &[],
        true,
    );
});
