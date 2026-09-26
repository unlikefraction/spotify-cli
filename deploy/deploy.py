#!/usr/bin/env python3
"""Build-verified release of the backend to the EC2 host through S3 + SSM, with rollback.

  cargo zigbuild --release -p silicon-spotify --bin spotify-api --target aarch64-unknown-linux-musl
  python3 deploy/deploy.py --caddy /path/to/caddy-linux-arm64 [--region us-east-1] [--profile default]
"""
import argparse, hashlib, json, pathlib, shlex, subprocess, tarfile, tempfile, time
p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
p.add_argument('--binary', type=pathlib.Path, default=pathlib.Path('target/aarch64-unknown-linux-musl/release/spotify-api'))
p.add_argument('--caddy', type=pathlib.Path, required=True, help='Caddy linux/arm64 binary (https://caddyserver.com/download)')
p.add_argument('--region', default='us-east-1')
p.add_argument('--profile', default='default')
p.add_argument('--stack', default='unlikefraction-spotify-production')
a = p.parse_args()
def aws(*parts): return subprocess.check_output(['aws', '--profile', a.profile, '--region', a.region, *parts], text=True)
binary = a.binary.read_bytes()
if binary[:4] != b'\x7fELF' or binary[18:20] != b'\xb7\x00': raise SystemExit(f'{a.binary} is not a Linux ARM64 ELF binary')
caddy = a.caddy.read_bytes()
if caddy[:4] != b'\x7fELF' or caddy[18:20] != b'\xb7\x00': raise SystemExit(f'{a.caddy} is not a Linux ARM64 ELF binary')
o = {x['OutputKey']: x['OutputValue'] for x in json.loads(aws('cloudformation', 'describe-stacks', '--stack-name', a.stack))['Stacks'][0]['Outputs']}
base = pathlib.Path(__file__).resolve().parent
with tempfile.TemporaryDirectory() as tmp:
    archive = pathlib.Path(tmp) / 'release.tar.gz'
    with tarfile.open(archive, 'w:gz') as t:
        t.add(a.binary, arcname='spotify-api'); t.add(a.caddy, arcname='caddy')
        for name in ('install-on-host.sh', 'spotify-api.service', 'Caddyfile', 'caddy.service', 'backup.py', 'spotify-backup.service', 'spotify-backup.timer'):
            t.add(base / name, arcname=name)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest(); release = checksum[:16]
    uri = f"s3://{o['ArtifactBucket']}/releases/{release}.tar.gz"; remote = '/opt/spotify/releases/' + release
    aws('s3', 'cp', str(archive), uri, '--sse', 'AES256', '--only-show-errors')
    command = '\n'.join(['set -e', 'umask 022', f'mkdir -p {remote}', f'aws s3 cp {shlex.quote(uri)} {remote}.tgz --region {a.region} --only-show-errors',
                         f"echo '{checksum}  {remote}.tgz' | sha256sum -c -", f'tar --no-same-owner -xzf {remote}.tgz -C {remote}', f'chown -R root:root {remote}', f'chmod 755 {remote}/spotify-api {remote}/caddy',
                         f'bash {remote}/install-on-host.sh {remote} {a.region}'])
    request = pathlib.Path(tmp) / 'request.json'
    request.write_text(json.dumps({'DocumentName': 'AWS-RunShellScript', 'InstanceIds': [o['InstanceId']], 'Parameters': {'commands': [command]}, 'Comment': 'spotify-cli release ' + release}))
    cid = json.loads(aws('ssm', 'send-command', '--cli-input-json', 'file://' + str(request)))['Command']['CommandId']
print(json.dumps({'release': release, 'instance': o['InstanceId'], 'command_id': cid}), flush=True)
for _ in range(200):
    time.sleep(3)
    r = json.loads(aws('ssm', 'get-command-invocation', '--command-id', cid, '--instance-id', o['InstanceId']))
    if r['Status'] in ('Pending', 'InProgress', 'Delayed'): continue
    print(r['Status'], r['StandardOutputContent'], r['StandardErrorContent'])
    raise SystemExit(0 if r['Status'] == 'Success' else 1)
raise SystemExit('Still running; inspect the SSM command before retrying.')
