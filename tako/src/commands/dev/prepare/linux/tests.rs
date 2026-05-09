use super::*;

// ── parse_loopback_alias ────────────────────────────────────────────

#[test]
fn loopback_alias_present_in_ip_addr_output() {
    let output = "\
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN group default qlen 1000
link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00
inet 127.0.0.1/8 scope host lo
   valid_lft forever preferred_lft forever
inet 127.77.0.1/8 scope host lo
   valid_lft forever preferred_lft forever
inet6 ::1/128 scope host noprefixroute
   valid_lft forever preferred_lft forever";
    assert!(parse_loopback_alias(output, "127.77.0.1"));
}

#[test]
fn loopback_alias_absent_in_ip_addr_output() {
    let output = "\
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN group default qlen 1000
link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00
inet 127.0.0.1/8 scope host lo
   valid_lft forever preferred_lft forever
inet6 ::1/128 scope host noprefixroute
   valid_lft forever preferred_lft forever";
    assert!(!parse_loopback_alias(output, "127.77.0.1"));
}

#[test]
fn loopback_alias_ignores_partial_match() {
    // Should not match 127.77.0.100 when looking for 127.77.0.1
    let output = "    inet 127.77.0.100/8 scope host lo";
    assert!(!parse_loopback_alias(output, "127.77.0.1"));
}

// ── parse_iptables_redirect ─────────────────────────────────────────

#[test]
fn iptables_redirect_present() {
    let output = "\
Chain OUTPUT (policy ACCEPT)
target     prot opt source               destination
REDIRECT   tcp  --  0.0.0.0/0            127.77.0.1           tcp dpt:443 redir ports 47831
REDIRECT   tcp  --  0.0.0.0/0            127.77.0.1           tcp dpt:80 redir ports 47830
REDIRECT   udp  --  0.0.0.0/0            127.77.0.1           udp dpt:53 redir ports 53535";
    assert!(parse_iptables_redirect(output, "127.77.0.1", 443, 47831));
    assert!(parse_iptables_redirect(output, "127.77.0.1", 80, 47830));
    assert!(parse_iptables_redirect(output, "127.77.0.1", 53, 53535));
}

#[test]
fn iptables_redirect_absent() {
    let output = "\
Chain OUTPUT (policy ACCEPT)
target     prot opt source               destination";
    assert!(!parse_iptables_redirect(output, "127.77.0.1", 443, 47831));
}

#[test]
fn iptables_redirect_wrong_port() {
    let output = "\
Chain OUTPUT (policy ACCEPT)
target     prot opt source               destination
REDIRECT   tcp  --  0.0.0.0/0            127.77.0.1           tcp dpt:8443 redir ports 47831";
    assert!(!parse_iptables_redirect(output, "127.77.0.1", 443, 47831));
}

#[test]
fn iptables_redirect_rejects_port_prefix_match() {
    // dpt:80 should NOT match a line for dpt:8080
    let output = "\
Chain OUTPUT (policy ACCEPT)
target     prot opt source               destination
REDIRECT   tcp  --  0.0.0.0/0            127.77.0.1           tcp dpt:8080 redir ports 47830";
    assert!(!parse_iptables_redirect(output, "127.77.0.1", 80, 47830));
}

#[test]
fn iptables_redirect_wrong_target_port() {
    let output = "\
Chain OUTPUT (policy ACCEPT)
target     prot opt source               destination
REDIRECT   tcp  --  0.0.0.0/0            127.77.0.1           tcp dpt:443 redir ports 9999";
    assert!(!parse_iptables_redirect(output, "127.77.0.1", 443, 47831));
}

// ── repair_plan ─────────────────────────────────────────────────────

#[test]
fn repair_plan_none_when_all_ok() {
    let status = LinuxSetupStatus {
        loopback_alias: true,
        redirect_443: true,
        redirect_80: true,
        redirect_dns: true,
        dns_configured: true,
        service_installed: true,
        is_nixos: false,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::None);
}

#[test]
fn repair_plan_setup_all_when_nothing_done() {
    let status = LinuxSetupStatus {
        loopback_alias: false,
        redirect_443: false,
        redirect_80: false,
        redirect_dns: false,
        dns_configured: false,
        service_installed: false,
        is_nixos: false,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::SetupAll);
}

#[test]
fn repair_plan_nixos_manual_when_not_configured() {
    let status = LinuxSetupStatus {
        loopback_alias: false,
        redirect_443: false,
        redirect_80: false,
        redirect_dns: false,
        dns_configured: false,
        service_installed: false,
        is_nixos: true,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::NixOsManual);
}

#[test]
fn repair_plan_nixos_none_when_configured() {
    // NixOS but everything is set up (user applied the nix config)
    let status = LinuxSetupStatus {
        loopback_alias: true,
        redirect_443: true,
        redirect_80: true,
        redirect_dns: true,
        dns_configured: true,
        service_installed: true,
        is_nixos: true,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::None);
}

#[test]
fn repair_plan_nixos_manual_when_redirects_ok_but_dns_missing() {
    // NixOS with redirects active but DNS not configured — should still
    // direct to NixOS manual setup, not fall through to SetupAll.
    let status = LinuxSetupStatus {
        loopback_alias: true,
        redirect_443: true,
        redirect_80: true,
        redirect_dns: true,
        dns_configured: false,
        service_installed: true,
        is_nixos: true,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::NixOsManual);
}

#[test]
fn repair_plan_repair_redirects_when_service_present_but_rules_missing() {
    let status = LinuxSetupStatus {
        loopback_alias: false,
        redirect_443: false,
        redirect_80: false,
        redirect_dns: false,
        dns_configured: true,
        service_installed: true,
        is_nixos: false,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::RepairRedirects);
}

#[test]
fn repair_plan_setup_all_when_dns_missing() {
    let status = LinuxSetupStatus {
        loopback_alias: true,
        redirect_443: true,
        redirect_80: true,
        redirect_dns: true,
        dns_configured: false,
        service_installed: false,
        is_nixos: false,
    };
    assert_eq!(repair_plan(&status), LinuxRepairPlan::SetupAll);
}

// ── content generators ──────────────────────────────────────────────

#[test]
fn systemd_service_contains_expected_commands() {
    let content = systemd_service_contents();
    assert!(content.contains("/sbin/ip addr add 127.77.0.1/8 dev lo"));
    assert!(content.contains("--dport 443"));
    assert!(content.contains("--to-port 47831"));
    assert!(content.contains("--dport 80"));
    assert!(content.contains("--to-port 47830"));
    assert!(content.contains("--dport 53"));
    assert!(content.contains("--to-port 53535"));
    assert!(content.contains("RemainAfterExit=yes"));
    assert!(content.contains("[Install]"));
    assert!(content.contains("WantedBy=multi-user.target"));
}

#[test]
fn resolved_drop_in_routes_tako_test() {
    let content = resolved_drop_in_contents();
    assert!(content.contains("DNS=127.77.0.1"));
    assert!(content.contains("Domains=~tako.test ~test"));
}

#[test]
fn nixos_snippet_contains_all_pieces() {
    let snippet = nixos_config_snippet();
    assert!(snippet.contains("tako-dev-redirect"));
    assert!(snippet.contains("127.77.0.1"));
    assert!(snippet.contains("--dport 443"));
    assert!(snippet.contains("--to-port 47831"));
    assert!(snippet.contains("tako.test"));
    assert!(snippet.contains("resolved"));
}

// ── action lines ────────────────────────────────────────────────────

#[test]
fn action_lines_are_nonempty() {
    assert!(!install_action_line().is_empty());
}
