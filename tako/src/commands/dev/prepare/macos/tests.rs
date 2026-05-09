use super::*;

#[test]
fn install_action_line_uses_bullet_copy() {
    assert_eq!(
        install_action_line(),
        "Install local dev proxy for 127.77.0.1:80/443"
    );
}

#[test]
fn reload_action_line_uses_bullet_copy() {
    assert_eq!(
        reload_action_line(),
        "Repair local dev proxy for 127.77.0.1:80/443"
    );
}

#[test]
fn launchd_plist_configures_socket_activation_on_loopback_ports() {
    let plist = launchd_plist(Path::new(
        "/Library/Application Support/Tako/bin/tako-dev-proxy",
    ));
    assert!(plist.contains(DEV_PROXY_LABEL));
    assert!(plist.contains("/Library/Application Support/Tako/bin/tako-dev-proxy"));
    assert!(plist.contains("<key>Sockets</key>"));
    assert!(plist.contains("<key>https</key>"));
    assert!(plist.contains("<key>http</key>"));
    assert!(plist.contains("<string>127.77.0.1</string>"));
    assert!(plist.contains("<string>443</string>"));
    assert!(plist.contains("<string>80</string>"));
    assert!(plist.contains("<key>KeepAlive</key>"));
    assert!(plist.contains("<false/>"));
}

#[test]
fn bootstrap_launchd_plist_runs_helper_at_boot() {
    let plist = bootstrap_launchd_plist(Path::new(
        "/Library/Application Support/Tako/bin/tako-dev-proxy",
    ));
    assert!(plist.contains(DEV_PROXY_BOOTSTRAP_LABEL));
    assert!(plist.contains("/Library/Application Support/Tako/bin/tako-dev-proxy"));
    assert!(plist.contains("<string>bootstrap</string>"));
    assert!(plist.contains("<key>RunAtLoad</key>"));
    assert!(plist.contains("<true/>"));
    assert!(plist.contains("<key>KeepAlive</key>"));
    assert!(plist.contains("<false/>"));
}

#[test]
fn loopback_alias_present_matches_assigned_ipv4_lines() {
    assert!(loopback_alias_present(
        "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST>\n\tinet 127.0.0.1 netmask 0xff000000\n\tinet 127.77.0.1 netmask 0xff000000 alias\n",
        "127.77.0.1",
    ));
    assert!(!loopback_alias_present(
        "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST>\n\tinet 127.0.0.1 netmask 0xff000000\n",
        "127.77.0.1",
    ));
}

#[test]
fn plists_match_current_layout_when_both_plists_match_installed_binary() {
    let binary = Path::new("/Library/Application Support/Tako/bin/tako-dev-proxy");
    assert!(plists_match_installed_binary(
        binary,
        &launchd_plist(binary),
        &bootstrap_launchd_plist(binary),
    ));
}

#[test]
fn plists_match_current_layout_rejects_stale_plist_contents() {
    let binary = Path::new("/Library/Application Support/Tako/bin/tako-dev-proxy");
    assert!(!plists_match_installed_binary(
        binary,
        "<plist>stale</plist>",
        &bootstrap_launchd_plist(binary),
    ));
    assert!(!plists_match_installed_binary(
        binary,
        &launchd_plist(binary),
        "<plist>stale</plist>",
    ));
}

#[test]
fn repair_plan_is_none_when_files_loaded_alias_ready_and_ports_ready() {
    assert_eq!(
        repair_plan(true, true, true, true, true, true),
        DevProxyRepairPlan::None
    );
}

#[test]
fn repair_plan_reloads_when_launchd_or_ports_are_not_ready() {
    assert_eq!(
        repair_plan(true, true, true, false, true, true),
        DevProxyRepairPlan::ReloadService
    );
    assert_eq!(
        repair_plan(true, true, true, true, false, true),
        DevProxyRepairPlan::ReloadService
    );
}

#[test]
fn repair_plan_installs_when_files_are_missing_boot_helper_missing_or_alias_missing() {
    assert_eq!(
        repair_plan(false, true, true, true, true, true),
        DevProxyRepairPlan::InstallOrUpdate
    );
    assert_eq!(
        repair_plan(true, false, true, true, true, true),
        DevProxyRepairPlan::InstallOrUpdate
    );
    assert_eq!(
        repair_plan(true, true, false, true, true, true),
        DevProxyRepairPlan::InstallOrUpdate
    );
}

#[test]
fn idle_exit_only_happens_when_no_connections_and_timeout_elapsed() {
    assert!(!should_exit_for_idle(
        1,
        DEV_PROXY_IDLE_TIMEOUT + Duration::from_secs(1),
        DEV_PROXY_IDLE_TIMEOUT,
    ));
    assert!(!should_exit_for_idle(
        0,
        DEV_PROXY_IDLE_TIMEOUT - Duration::from_secs(1),
        DEV_PROXY_IDLE_TIMEOUT,
    ));
    assert!(should_exit_for_idle(
        0,
        DEV_PROXY_IDLE_TIMEOUT + Duration::from_secs(1),
        DEV_PROXY_IDLE_TIMEOUT,
    ));
}
