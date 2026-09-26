#!/bin/bash
# Runs on the EC2 host (through SSM) from an unpacked release directory. Writes the runtime env
# from Secrets Manager (never from arguments), installs units, switches /opt/spotify/current
# atomically, restarts, health-checks, and rolls back on failure.
set -euo pipefail
release="${1:?release directory}"
region="${2:-us-east-1}"
umask 077
exec 9>/var/lock/spotify-deploy.lock
flock -n 9
previous=$(readlink -e /opt/spotify/current || true)
backup=$(mktemp -d)
targets=(/etc/spotify/runtime.env /etc/systemd/system/spotify-api.service /etc/caddy/Caddyfile /etc/systemd/system/caddy.service /etc/systemd/system/spotify-backup.service /etc/systemd/system/spotify-backup.timer)
for target in "${targets[@]}"; do [[ ! -f "$target" ]] || cp -p "$target" "$backup/$(basename "$target")"; done
rollback() {
  result=$?
  trap - EXIT
  if [[ "$result" != 0 && -n "$previous" ]]; then
    for target in "${targets[@]}"; do [[ ! -f "$backup/$(basename "$target")" ]] || cp -p "$backup/$(basename "$target")" "$target"; done
    ln -sfn "$previous" /opt/spotify/current.next && mv -Tf /opt/spotify/current.next /opt/spotify/current
    systemctl daemon-reload; systemctl restart spotify-api || true; systemctl restart caddy || true
  fi
  rm -rf "$backup"
  exit "$result"
}
trap rollback EXIT
python3 - "$region" <<'PY'
import json, pathlib, subprocess, sys
env = json.loads(json.loads(subprocess.check_output(['aws', 'secretsmanager', 'get-secret-value', '--region', sys.argv[1], '--secret-id', 'unlikefraction-spotify/production/runtime']))['SecretString'])
allowed = {k for k in env if k.startswith('SPOTIFY_') or k == 'RUST_LOG'}
def quote(v):
    if any(c in v for c in '\n\r\0'): raise ValueError('multiline runtime value')
    return '"' + v.replace('\\', '\\\\').replace('"', '\\"') + '"'
p = pathlib.Path('/etc/spotify/runtime.env')
p.write_text(''.join(f'{k}={quote(v)}\n' for k, v in sorted(env.items()) if k in allowed and isinstance(v, str)))
p.chmod(0o600)
PY
[[ -x /usr/local/bin/caddy ]] || install -m 0755 "$release/caddy" /usr/local/bin/caddy
install -m 0644 "$release/Caddyfile" /etc/caddy/Caddyfile
for unit in spotify-api.service caddy.service spotify-backup.service spotify-backup.timer; do install -m 0644 "$release/$unit" "/etc/systemd/system/$unit"; done
/usr/local/bin/caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
ln -sfn "$release" /opt/spotify/current.next && mv -Tf /opt/spotify/current.next /opt/spotify/current
systemctl daemon-reload
systemctl enable spotify-api caddy spotify-backup.timer
systemctl restart spotify-api
for attempt in {1..30}; do
  if curl -fsS http://127.0.0.1:8787/healthz >/dev/null; then
    systemctl restart caddy
    for g in {1..20}; do
      if curl -fsS --connect-timeout 2 --max-time 5 --resolve backend.spotify.unlikefraction.com:443:127.0.0.1 https://backend.spotify.unlikefraction.com/healthz; then
        systemctl start spotify-backup.timer
        printf '\nInstalled %s\n' "$release"
        exit 0
      fi
      sleep 3
    done
    printf 'HTTPS check failed (DNS or certificate not ready?)\n' >&2
    exit 1
  fi
  sleep 2
done
journalctl -u spotify-api -n 50 --no-pager >&2 || true
printf 'Health check failed; restoring the previous release.\n' >&2
exit 1
