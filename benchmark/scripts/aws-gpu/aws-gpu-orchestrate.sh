#!/usr/bin/env bash
set -Eeuo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(git -C "$script_dir" rev-parse --show-toplevel)
region=${AWS_REGION:-eu-central-1}
instance_id=${STACKHOUR_GPU_INSTANCE_ID:?set STACKHOUR_GPU_INSTANCE_ID}
security_group=${STACKHOUR_GPU_SECURITY_GROUP_ID:?set STACKHOUR_GPU_SECURITY_GROUP_ID}
ssh_user=${STACKHOUR_GPU_SSH_USER:-ubuntu}
ssh_key=${STACKHOUR_GPU_SSH_KEY:?set STACKHOUR_GPU_SSH_KEY}
bundle=${STACKHOUR_GPU_BUNDLE:?set STACKHOUR_GPU_BUNDLE}
remote_script=${STACKHOUR_GPU_REMOTE_SCRIPT:-"$script_dir/aws-gpu-remote.sh"}
output_root=${STACKHOUR_GPU_OUTPUT_ROOT:-"$repo_root/benchmark/results"}
known_hosts=${STACKHOUR_GPU_KNOWN_HOSTS:-"$output_root/aws-gpu-known-hosts"}
run_stamp=$(date -u +%Y%m%dT%H%M%SZ)
only_candidate=${1-}
local_archive="$output_root/stackhour-aws-gpu-results-$run_stamp.tar.gz"
current_ip=
ssh_cidr=
ssh_rule_added=0
must_stop=0
cleanup_started=0
public_ip=

mkdir -p "$output_root"
touch "$known_hosts"

cleanup() {
  original_status=$?
  trap - EXIT INT TERM
  if ((cleanup_started)); then
    exit "$original_status"
  fi
  cleanup_started=1
  set +e

  if ((must_stop)); then
    echo "Stopping $instance_id..."
    aws ec2 stop-instances --region "$region" --instance-ids "$instance_id" >/dev/null
    aws ec2 wait instance-stopped --region "$region" --instance-ids "$instance_id"
    final_state=$(aws ec2 describe-instances --region "$region" --instance-ids "$instance_id" \
      --query 'Reservations[0].Instances[0].State.Name' --output text)
    echo "Final EC2 state: $final_state"
  fi

  if ((ssh_rule_added)); then
    echo "Removing temporary SSH rule $ssh_cidr..."
    aws ec2 revoke-security-group-ingress \
      --region "$region" \
      --group-id "$security_group" \
      --protocol tcp \
      --port 22 \
      --cidr "$ssh_cidr" >/dev/null
  fi
  exit "$original_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

initial_state=$(aws ec2 describe-instances --region "$region" --instance-ids "$instance_id" \
  --query 'Reservations[0].Instances[0].State.Name' --output text)
if [[ "$initial_state" != stopped ]]; then
  echo "Refusing to start: expected $instance_id to be stopped, found $initial_state" >&2
  exit 2
fi
must_stop=1

current_ip=$(curl -fsSL https://checkip.amazonaws.com | tr -d '[:space:]')
ssh_cidr="$current_ip/32"
existing_rule=$(aws ec2 describe-security-groups --region "$region" --group-ids "$security_group" \
  --output json | jq -r --arg cidr "$ssh_cidr" \
  '[.SecurityGroups[0].IpPermissions[] | select(.FromPort == 22 and .ToPort == 22) | .IpRanges[] | select(.CidrIp == $cidr)] | length')
if [[ "$existing_rule" == 0 ]]; then
  aws ec2 authorize-security-group-ingress \
    --region "$region" \
    --group-id "$security_group" \
    --ip-permissions "[{\"IpProtocol\":\"tcp\",\"FromPort\":22,\"ToPort\":22,\"IpRanges\":[{\"CidrIp\":\"$ssh_cidr\",\"Description\":\"temporary Stackhour GPU benchmark\"}]}]" >/dev/null
  ssh_rule_added=1
fi

echo "Starting $instance_id..."
aws ec2 start-instances --region "$region" --instance-ids "$instance_id" >/dev/null
aws ec2 wait instance-running --region "$region" --instance-ids "$instance_id"

for _ in $(seq 1 30); do
  public_ip=$(aws ec2 describe-instances --region "$region" --instance-ids "$instance_id" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
  [[ "$public_ip" != None && -n "$public_ip" ]] && break
  sleep 2
done
if [[ "$public_ip" == None || -z "$public_ip" ]]; then
  echo "Instance has no public IP" >&2
  exit 3
fi
echo "Public IP: $public_ip"

ssh_options=(
  -i "$ssh_key"
  -o BatchMode=yes
  -o ConnectTimeout=8
  -o ServerAliveInterval=30
  -o ServerAliveCountMax=4
  -o StrictHostKeyChecking=accept-new
  -o UserKnownHostsFile="$known_hosts"
)

for attempt in $(seq 1 36); do
  if ssh "${ssh_options[@]}" "$ssh_user@$public_ip" true; then
    break
  fi
  if ((attempt == 36)); then
    echo "SSH did not become ready" >&2
    exit 4
  fi
  sleep 5
done

scp "${ssh_options[@]}" "$bundle" "$ssh_user@$public_ip:/tmp/stackhour-gpu-bench.bundle"
scp "${ssh_options[@]}" "$remote_script" "$ssh_user@$public_ip:/tmp/aws-gpu-remote.sh"

set +e
if [[ -n "$only_candidate" ]]; then
  ssh "${ssh_options[@]}" "$ssh_user@$public_ip" \
    "chmod +x /tmp/aws-gpu-remote.sh && ONLY_CANDIDATE='$only_candidate' /tmp/aws-gpu-remote.sh"
else
  ssh "${ssh_options[@]}" "$ssh_user@$public_ip" \
    "chmod +x /tmp/aws-gpu-remote.sh && /tmp/aws-gpu-remote.sh"
fi
remote_status=$?
set -e

set +e
scp "${ssh_options[@]}" "$ssh_user@$public_ip:/tmp/stackhour-gpu-results.tar.gz" "$local_archive"
copy_status=$?
ssh "${ssh_options[@]}" "$ssh_user@$public_ip" \
  "sudo systemctl stop stackhour-benchmark-finish-stop.timer >/dev/null 2>&1 || true"
set -e

if ((copy_status != 0)); then
  echo "Failed to retrieve remote results" >&2
  exit 5
fi
echo "Results saved to $local_archive"
if ((remote_status != 0)); then
  echo "Remote benchmark returned $remote_status; partial results were preserved" >&2
  exit "$remote_status"
fi
