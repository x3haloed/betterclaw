#!/usr/bin/env bash
# VM bootstrap script for BetterClaw on GCP Compute Engine.
#
# Run on a fresh Debian 12 VM after SSH:
#   sudo bash setup.sh
#
# Prerequisites:
#   - VM has the betterclaw-vm service account attached
#   - Cloud SQL Auth Proxy accessible via IAM
#   - Artifact Registry image pushed

set -euo pipefail

# Must run as root
if [ "$(id -u)" -ne 0 ]; then
  echo "ERROR: This script must be run as root (sudo bash setup.sh)"
  exit 1
fi

echo "==> Installing Docker"
apt-get update
apt-get install -y docker.io
systemctl enable docker
systemctl start docker

echo "==> Installing Cloud SQL Auth Proxy"
curl -fsSL -o /usr/local/bin/cloud-sql-proxy \
  https://storage.googleapis.com/cloud-sql-connectors/cloud-sql-proxy/v2.14.3/cloud-sql-proxy.linux.amd64
chmod +x /usr/local/bin/cloud-sql-proxy

echo "==> Installing systemd services"
cp /tmp/deploy/cloud-sql-proxy.service /etc/systemd/system/
cp /tmp/deploy/betterclaw.service /etc/systemd/system/
systemctl daemon-reload

echo "==> Starting Cloud SQL Auth Proxy"
systemctl enable cloud-sql-proxy
systemctl start cloud-sql-proxy

echo "==> Configuring Docker registry auth"
# The VM service account provides Artifact Registry access
gcloud auth configure-docker us-central1-docker.pkg.dev --quiet

echo "==> Creating config directory"
# Owned by root, readable only by root. Docker reads --env-file as root
# before dropping to uid 1000 (betterclaw) inside the container.
mkdir -p /opt/betterclaw
chmod 700 /opt/betterclaw

if [ ! -f /opt/betterclaw/.env ]; then
  echo "WARNING: /opt/betterclaw/.env does not exist."
  echo "Create it with your configuration before starting BetterClaw."
  echo "See deploy/env.example for the required variables."
  echo ""
  echo "Then run: systemctl enable betterclaw && systemctl start betterclaw"
else
  chmod 600 /opt/betterclaw/.env
  echo "==> Starting BetterClaw"
  systemctl enable betterclaw
  systemctl start betterclaw
fi

echo "==> Setup complete"
echo ""
echo "Verify with:"
echo "  systemctl status cloud-sql-proxy"
echo "  systemctl status betterclaw"
echo "  docker logs betterclaw"
