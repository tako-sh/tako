
install_systemd_service_unit() {
  mkdir -p /etc/systemd/system
  cat > /etc/systemd/system/tako-server.service <<EOF
[Unit]
Description=Tako Server
After=network.target
$TAKO_NETWORK_UNIT_ARGS

[Service]
Type=notify
NotifyAccess=all
User=$TAKO_USER
Group=$TAKO_USER
# sudo is restricted to the root-owned provisioning and maintenance helpers.
# DAC_READ_SEARCH stays bounding-only for PAM shadow reads by those helpers.
NoNewPrivileges=false
SupplementaryGroups=tako-app
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/tako-server --socket $TAKO_SOCKET --data-dir $TAKO_HOME $TAKO_PUBLIC_PORT_ARGS $TAKO_MANAGEMENT_ARGS
ExecReload=/bin/kill -HUP \$MAINPID
Restart=always
RestartSec=1
LimitNOFILE=1048576
KillMode=mixed
TimeoutStopSec=30min
RuntimeDirectory=tako
RuntimeDirectoryMode=0700

[Install]
WantedBy=multi-user.target
EOF
}

install_systemd_standby_unit() {
  cat > /etc/systemd/system/tako-server-standby.service <<EOF
[Unit]
Description=Tako Server Standby
After=network.target
$TAKO_NETWORK_UNIT_ARGS

[Service]
Type=notify
NotifyAccess=all
User=$TAKO_USER
Group=$TAKO_USER
NoNewPrivileges=false
SupplementaryGroups=tako-app
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/tako-server --standby --socket $TAKO_SOCKET --data-dir $TAKO_HOME $TAKO_PUBLIC_PORT_ARGS --instance-port-offset 1000
Restart=always
RestartSec=1
LimitNOFILE=1048576
KillMode=mixed
TimeoutStopSec=30min
RuntimeDirectory=tako
RuntimeDirectoryMode=0700

[Install]
WantedBy=multi-user.target
EOF
}

if [ "$SERVICE_MANAGER" = "systemd" ]; then
  install_systemd_service_unit
  install_systemd_standby_unit
fi

if [ "$SERVICE_MANAGER" = "systemd" ]; then
  systemctl daemon-reload
  if is_enabled "$TAKO_RESTART_SERVICE"; then
    systemctl enable tako-server
    if systemctl is-active --quiet tako-server; then
      # Restart also replaces children started under the previous security policy.
      echo "Restarting tako-server service"
      systemctl restart tako-server
      echo "OK tako-server restarted with updated service settings"
    else
      echo "Starting tako-server service"
      systemctl start tako-server
    fi
    systemctl --no-pager status tako-server || true
    if ! systemctl is-active --quiet tako-server; then
      echo "error: tako-server failed to start; see service status above" >&2
      exit 1
    fi
  else
    if systemctl is-active --quiet tako-server; then
      # Upgrades refresh a running service without restarting it. Repair boot startup too.
      systemctl enable tako-server
      echo "OK refreshed tako-server service without restarting"
    else
      echo "OK installed tako-server service (not started)"
      echo 'Run `tako servers add <host>` from your workstation to start and verify it.'
    fi
  fi
else
  # Install-refresh mode can run before init is active (for example in image builds).
  # In this mode we install binaries/users only and skip service definition install.
  echo "OK install refreshed without active service manager (TAKO_RESTART_SERVICE=0); skipped service definition install"
fi

echo "OK installed tako-server"
echo "OK configured user: $TAKO_USER"
