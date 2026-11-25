#!/bin/sh
# Allowlist = gymclaw's fixed integrations + EGRESS_EXTRA_HOSTS (comma/space separated,
# e.g. the personal calendar feed host, kept out of the public repo).
set -eu
{
  printf '%s\n' api.telegram.org openrouter.ai www.mysports.com
  printf '%s\n' ${EGRESS_EXTRA_HOSTS:-} | tr ',' '\n'
} | sed '/^$/d' | sort -u > /tmp/allowed-hosts
echo "egress allowlist: $(tr '\n' ' ' < /tmp/allowed-hosts)"
exec squid -N -f /etc/squid/squid.conf
