# GitHub Actions Secrets

The following secrets must be configured in the repository before the
CD pipeline can deploy to the GCP server.

Go to: **Repository Settings → Secrets and variables → Actions → New repository secret**

## Required secrets

### `GCP_SERVER_IP`

The public IP address of the GCP Compute Engine instance.

Example value: `34.x.x.x`

Used by: `cd.yml` deploy job

### `GCP_SSH_PRIVATE_KEY`

The full content of the SSH private key used to access the GCP server.
This is the private key whose public key is in the server's
`~/.ssh/authorized_keys` file for the `devops_admin` user.

How to get it: `cat ~/.ssh/your_gcp_key` (the private key file, not `.pub`).
Copy the entire output, including the `BEGIN` and `END` lines.

Used by: `cd.yml` deploy job

## Automatic secrets (no setup needed)

### `GITHUB_TOKEN`

Automatically provided by GitHub Actions. Used for pushing to the
`ghcr.io` container registry. No manual setup required.

## How to verify secrets are working

After setting both secrets, push any commit to `main`. Watch the CD
workflow in the Actions tab. If a secret is wrong or missing, the
deploy job fails at the SSH step with an authentication error — not a
silent failure.
