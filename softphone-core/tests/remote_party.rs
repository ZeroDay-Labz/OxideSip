use softphone_core::events::RemoteParty;

#[test]
fn quoted_display_name_with_tag() {
    let p = RemoteParty::from_header_value("\"Alice Nguyen\" <sip:1001@pbx.lan>;tag=abc123");
    assert_eq!(p.display_name.as_deref(), Some("Alice Nguyen"));
    assert_eq!(p.user, "1001");
    assert_eq!(p.uri, "sip:1001@pbx.lan");
    assert_eq!(p.host(), "pbx.lan");
    assert_eq!(p.label(), "Alice Nguyen");
    assert_eq!(p.to_string(), "Alice Nguyen (1001)");
}

#[test]
fn legacy_raw_header_with_prefix() {
    let p = RemoteParty::from_header_value("From: \"Bob\" <sip:2002@10.0.0.5:5060;transport=udp>;tag=x");
    assert_eq!(p.display_name.as_deref(), Some("Bob"));
    assert_eq!(p.user, "2002");
    assert_eq!(p.uri, "sip:2002@10.0.0.5:5060");
    assert_eq!(p.host(), "10.0.0.5:5060");
}

#[test]
fn angle_brackets_without_name() {
    let p = RemoteParty::from_header_value("<sip:1001@pbx.lan>;tag=x");
    assert_eq!(p.display_name, None);
    assert_eq!(p.user, "1001");
    assert_eq!(p.label(), "1001");
    assert_eq!(p.to_string(), "1001");
}

#[test]
fn bare_uri_with_params() {
    let p = RemoteParty::from_header_value("sip:1001@pbx.lan;tag=x");
    assert_eq!(p.user, "1001");
    assert_eq!(p.uri, "sip:1001@pbx.lan");
}

#[test]
fn unquoted_display_name() {
    let p = RemoteParty::from_header_value("Front Desk <sip:100@pbx.lan>");
    assert_eq!(p.display_name.as_deref(), Some("Front Desk"));
    assert_eq!(p.user, "100");
}

#[test]
fn tel_uri() {
    let p = RemoteParty::from_header_value("<tel:+15551234567>");
    assert_eq!(p.user, "+15551234567");
    assert_eq!(p.uri, "tel:+15551234567");
}

#[test]
fn anonymous_and_empty_name() {
    let p = RemoteParty::from_header_value("\"\" <sip:anonymous@anonymous.invalid>");
    assert_eq!(p.display_name, None);
    assert_eq!(p.user, "anonymous");
    let p = RemoteParty::from_header_value("<sip:pbx.lan>");
    assert_eq!(p.user, "pbx.lan");
}
