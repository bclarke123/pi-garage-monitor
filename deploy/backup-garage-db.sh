#!/bin/sh
# Nightly backup of the garage-monitor database to another machine.
#
# Uses SQLite's online-backup API for a consistent snapshot while the
# daemon keeps writing, refuses to ship a snapshot that fails an
# integrity check, and keeps a self-rotating week of dailies
# (garage-Mon.db .. garage-Sun.db) plus one archival copy per month.
#
# Setup (run as the user that owns the database, e.g. jamin):
#   sudo apt install -y sqlite3 rsync
#   ssh-keygen -t ed25519          # if no key yet; accept defaults
#   ssh-copy-id <user>@<backup-host>
#   ssh <user>@<backup-host> mkdir -p garage-backups
#   sudo install -m 755 backup-garage-db.sh /usr/local/bin/
#   crontab -e   # add:
#   17 3 * * * /usr/local/bin/backup-garage-db.sh 2>&1 | logger -t garage-backup

set -eu

DB=/var/lib/garage-monitor/garage.db
DEST=jamin@basil:garage-backups

STAGE=$(mktemp /tmp/garage-backup.XXXXXX)
trap 'rm -f "$STAGE"' EXIT

sqlite3 "$DB" ".backup '$STAGE'"
sqlite3 "$STAGE" 'PRAGMA integrity_check;' | grep -qx ok

rsync -a "$STAGE" "$DEST/garage-$(date +%a).db"

# First of the month: keep an archival copy that the ring never overwrites.
if [ "$(date +%d)" = 01 ]; then
    rsync -a "$STAGE" "$DEST/garage-$(date +%Y-%m).db"
fi
