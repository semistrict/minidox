#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
project=${MINIDOX_GCE_PROJECT:-$(gcloud config get-value project 2>/dev/null)}
zone=${MINIDOX_GCE_ZONE:-us-east1-b}
machine_type=${MINIDOX_GCE_MACHINE_TYPE:-n2-standard-4}
instance=${MINIDOX_GCE_INSTANCE:-minidox-dax-$(date +%Y%m%d-%H%M%S)-$$}
remote_work=/var/tmp/$instance
archive=$(mktemp /tmp/minidox-gce-source.XXXXXX.tar.gz)
created=0

cleanup() {
    status=$?
    trap - EXIT INT TERM
    rm -f -- "$archive"
    if [ "$created" -eq 1 ]; then
        if ! gcloud compute instances delete "$instance" \
            --project="$project" --zone="$zone" --quiet; then
            status=1
        fi
        if gcloud compute instances describe "$instance" \
            --project="$project" --zone="$zone" >/dev/null 2>&1; then
            echo "temporary GCE VM still exists: $instance" >&2
            status=1
        fi
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

if [ -z "$project" ] || [ "$project" = '(unset)' ]; then
    echo "set MINIDOX_GCE_PROJECT or configure a gcloud project" >&2
    exit 1
fi

COPYFILE_DISABLE=1 tar --no-xattrs --exclude=.git --exclude=target \
    -C "$repo_root" -czf "$archive" .

gcloud compute instances create "$instance" \
    --project="$project" \
    --zone="$zone" \
    --machine-type="$machine_type" \
    --image-family=debian-12 \
    --image-project=debian-cloud \
    --boot-disk-size=50GB \
    --boot-disk-type=pd-balanced \
    --enable-nested-virtualization \
    --max-run-duration=2h \
    --instance-termination-action=DELETE \
    --no-service-account \
    --no-scopes
created=1

ready=0
for attempt in $(seq 1 36); do
    if gcloud compute ssh "$instance" --project="$project" --zone="$zone" \
        --command=true --ssh-flag='-o ConnectTimeout=5' >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 5
done
if [ "$ready" -ne 1 ]; then
    echo "GCE VM did not become reachable over SSH" >&2
    exit 1
fi

gcloud compute scp "$archive" "$instance:/tmp/minidox-source.tar.gz" \
    --project="$project" --zone="$zone" --quiet
gcloud compute ssh "$instance" --project="$project" --zone="$zone" \
    --command="mkdir -p '$remote_work' && tar -C '$remote_work' -xzf /tmp/minidox-source.tar.gz && sudo '$remote_work/scripts/setup-gce-host.sh' \"\$USER\""
gcloud compute ssh "$instance" --project="$project" --zone="$zone" \
    --ssh-flag='-o ServerAliveInterval=30' \
    --command="cd '$remote_work' && ./scripts/test-linux-kvm.sh"
