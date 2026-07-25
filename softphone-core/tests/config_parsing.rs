use softphone_core::config;
use std::path::PathBuf;
use std::sync::Mutex;

// Env vars are process-global; serialize tests that touch OXIDESIP_* so they
// don't race under cargo's default parallel test execution.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn unique_temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("oxidesip_test_{name}_{}.toml", std::process::id()));
    path
}

#[test]
fn toml_load_and_env_override_precedence() {
    let _guard = ENV_LOCK.lock().unwrap();
    let path = unique_temp_path("config_parsing");
    std::fs::write(
        &path,
        r#"
sip_server_host = "pbx.example.com"
username = "1001"
password = "filepass"
ca_cert_path = "./ca.pem"
"#,
    )
    .unwrap();

    let cfg = config::load_config(&path).expect("should load from file");
    assert_eq!(cfg.sip_server_host, "pbx.example.com");
    assert_eq!(cfg.password, "filepass");
    assert_eq!(cfg.sip_server_port, 5060); // default applied (udp transport)

    unsafe {
        std::env::set_var("OXIDESIP_PASSWORD", "envpass");
    }
    let cfg = config::load_config(&path).expect("should load with env override");
    assert_eq!(cfg.password, "envpass");
    assert_eq!(cfg.sip_server_host, "pbx.example.com"); // file value preserved

    unsafe {
        std::env::remove_var("OXIDESIP_PASSWORD");
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn udp_transport_does_not_require_ca_cert() {
    let _guard = ENV_LOCK.lock().unwrap();
    let path = unique_temp_path("udp_no_ca");
    std::fs::write(
        &path,
        r#"
sip_server_host = "pbx.example.com"
username = "1001"
password = "filepass"
"#,
    )
    .unwrap();

    let cfg = config::load_config(&path).expect("udp transport should not require ca_cert_path");
    assert_eq!(cfg.transport, config::SipTransport::Udp);
    assert_eq!(cfg.sip_server_port, 5060);
    assert!(cfg.ca_cert_path.is_none());
    assert!(!cfg.srtp);

    std::fs::remove_file(&path).ok();
}

#[test]
fn env_overrides_transport_and_srtp() {
    let _guard = ENV_LOCK.lock().unwrap();
    let path = unique_temp_path("env_transport");
    std::fs::write(
        &path,
        r#"
sip_server_host = "pbx.example.com"
username = "1001"
password = "filepass"
"#,
    )
    .unwrap();

    unsafe {
        std::env::set_var("OXIDESIP_TRANSPORT", "tcp");
        std::env::set_var("OXIDESIP_SRTP", "true");
    }
    let cfg = config::load_config(&path).expect("should load with transport/srtp overrides");
    assert_eq!(cfg.transport, config::SipTransport::Tcp);
    assert_eq!(cfg.sip_server_port, 5060); // tcp still defaults to 5060
    assert!(cfg.srtp);

    unsafe {
        std::env::remove_var("OXIDESIP_TRANSPORT");
        std::env::remove_var("OXIDESIP_SRTP");
    }
    std::fs::remove_file(&path).ok();
}
