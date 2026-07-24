# AWS GPU benchmark runner

`aws-gpu-orchestrate.sh` starts an existing GPU EC2 instance, grants temporary
SSH access for the caller's current IP, uploads the benchmark bundle and remote
runner, retrieves the result archive, and stops the instance from its `EXIT`
trap. The remote runner also arms independent four-hour and ten-minute shutdown
guards.

Required environment variables:

- `STACKHOUR_GPU_INSTANCE_ID`
- `STACKHOUR_GPU_SECURITY_GROUP_ID`
- `STACKHOUR_GPU_SSH_KEY`
- `STACKHOUR_GPU_BUNDLE`

Optional variables include `AWS_REGION`, `STACKHOUR_GPU_SSH_USER`,
`STACKHOUR_GPU_REMOTE_SCRIPT`, `STACKHOUR_GPU_OUTPUT_ROOT`, and
`STACKHOUR_GPU_KNOWN_HOSTS`.

Run all candidates:

```sh
benchmark/scripts/aws-gpu/aws-gpu-orchestrate.sh
```

Run one candidate:

```sh
benchmark/scripts/aws-gpu/aws-gpu-orchestrate.sh gpui
```

The orchestrator refuses to start an instance that is not initially stopped.
It always waits for the instance to reach `stopped` before removing the
temporary SSH rule and exiting.
