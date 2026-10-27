#!/bin/sh
# WeftOS machine mesh service install (systemd). PRINTED by `weaver mesh install-service`;
# nothing has been run. Read it, then run it as an administrator (sudo sh install.sh).
#
# What it does:
#  - creates the weftos account and the weftos group (no login, never root)
#  - adds YOU (the invoking user) to the weftos group: /var/run/weftos is owned by that group with
#    mode 0750, so only group members can reach the mesh socket. Log out and in (or start a new
#    login session) for the membership to apply.
#  - installs a root-owned copy of weaver at /usr/local/libexec/weftos/weaver (the service never runs from a
#    user-writable path)
#  - writes /etc/weftos/mesh.toml (only if absent) and installs the unit; it does NOT start the service
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'run this script as root (sudo sh install.sh)' >&2; exit 1; }
TARGET_USER="${SUDO_USER:-}"
[ -n "$TARGET_USER" ] && [ "$TARGET_USER" != root ] || { echo 'run it with sudo from your own account so the group member can be your user' >&2; exit 1; }

# --- account and group ---
cat > /etc/sysusers.d/weftos-mesh.conf.new <<'WEFTOS_EOF'
# WeftOS machine mesh service account (never root, no login shell).
u weftos - "WeftOS mesh service" /var/lib/weftos/mesh /usr/sbin/nologin
WEFTOS_EOF
chown root:root /etc/sysusers.d/weftos-mesh.conf.new && chmod 0644 /etc/sysusers.d/weftos-mesh.conf.new && mv /etc/sysusers.d/weftos-mesh.conf.new /etc/sysusers.d/weftos-mesh.conf
systemd-sysusers /etc/sysusers.d/weftos-mesh.conf
# add weftos membership for the invoking user
usermod -aG weftos "$TARGET_USER"

# --- directories and modes ---
install -d -m 0755 -o root -g root /etc/weftos /var/lib/weftos /usr/local/libexec/weftos
install -d -m 0700 -o weftos -g weftos /var/lib/weftos/mesh
install -d -m 0750 -o weftos -g weftos /var/run/weftos

# --- binary (root-owned copy) ---
install -m 0755 -o root -g root '/opt/we ftos/it'\''s/weaver' /usr/local/libexec/weftos/weaver

# --- box key: none adopted; the service generates a new one on first start (new node id) ---

# --- configuration (kept if already present) ---
if [ ! -e /etc/weftos/mesh.toml ]; then
cat > /etc/weftos/mesh.toml.new <<'WEFTOS_EOF'
# WeftOS machine mesh service (written by `weaver mesh install-service`).
state_dir = "/var/lib/weftos/mesh"
socket = "/var/run/weftos/mesh.sock"
listen = "127.0.0.1:9489"
health_listen = "127.0.0.1:9490"
WEFTOS_EOF
chown root:root /etc/weftos/mesh.toml.new && chmod 0644 /etc/weftos/mesh.toml.new && mv /etc/weftos/mesh.toml.new /etc/weftos/mesh.toml
else
  echo '/etc/weftos/mesh.toml exists; kept as is (--listen and --admin-uid were NOT applied; edit it yourself)'
fi

# --- unit ---
cat > /etc/systemd/system/weftos-mesh.service.new <<'WEFTOS_EOF'
[Unit]
Description=WeftOS machine mesh service
Documentation=https://github.com/WeftOS/weftos
After=network.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
User=weftos
Group=weftos
ExecStart="/usr/local/libexec/weftos/weaver" mesh serve --config /etc/weftos/mesh.toml
Restart=always
RestartSec=3
NotifyAccess=main
WatchdogSec=60
StateDirectory=weftos/mesh
StateDirectoryMode=0700
RuntimeDirectory=weftos
RuntimeDirectoryMode=0750
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
CapabilityBoundingSet=
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

[Install]
WantedBy=multi-user.target
WEFTOS_EOF
chown root:root /etc/systemd/system/weftos-mesh.service.new && chmod 0644 /etc/systemd/system/weftos-mesh.service.new && mv /etc/systemd/system/weftos-mesh.service.new /etc/systemd/system/weftos-mesh.service
systemctl daemon-reload

# --- done; NOT started ---
echo 'installed. weftos membership for '"$TARGET_USER"' takes effect at next login.'
echo 'stop any collapsed user daemon first (it holds 9489): weaver kernel stop'
echo 'then enable the service with:'
echo '  sudo systemctl enable --now weftos-mesh'
echo 'afterwards, on this host as root or an admin: run weaver mesh status. It prints the node id and the machine key fingerprint. Adopted key: the node id must equal the one you recorded before migrating. Fresh key: compare the fingerprint out of band (do not take it from a user-run status on another machine), then run weaver mesh trust as each user'
# ENABLE (exact command): systemctl enable --now weftos-mesh
