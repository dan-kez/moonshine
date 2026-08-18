#!/bin/sh
# Pre-remove script for nfpm-generated packages (.deb/.rpm/.pkg.tar.zst).
# Runs as root at package removal time.

# Disable the tray for all users while its unit file is still present; doing this
# after removal would leave the enablement symlinks behind, pointing at nothing.
systemctl --global disable moonshine-tray.service 2>/dev/null || true
