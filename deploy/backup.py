#!/usr/bin/env python3
"""Online SQLite snapshot of the backend database (reports, webhook dedupe, encrypted OBO credentials) to encrypted S3."""
import datetime, os, pathlib, sqlite3, subprocess, tempfile
bucket = os.environ['SPOTIFY_BACKUP_BUCKET']
source = pathlib.Path('/var/lib/spotify/spotify.sqlite')
if source.exists():
    with tempfile.TemporaryDirectory() as tmp:
        prefix = datetime.datetime.now(datetime.timezone.utc).strftime('%Y/%m/%d/%H%M%S')
        snapshot = pathlib.Path(tmp) / source.name
        with sqlite3.connect(f'file:{source}?mode=ro', uri=True) as db, sqlite3.connect(snapshot) as target:
            db.backup(target)
        snapshot.chmod(0o600)
        subprocess.run(['aws', 's3', 'cp', str(snapshot), f's3://{bucket}/backups/{prefix}/{source.name}', '--region', 'us-east-1', '--sse', 'AES256', '--only-show-errors'], check=True)
