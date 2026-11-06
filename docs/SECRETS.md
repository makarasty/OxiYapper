# Secrets

The repo needs no secrets today. Workflows use only the built-in `GITHUB_TOKEN`, with permissions declared per workflow (`contents: read` by default; `contents: write`, `id-token: write` and `attestations: write` only in the publish job of `release.yml`). Workflows never use `pull_request_target`.

Planned signing secrets, to be added later as GitHub Actions secrets in an environment named `release` (required reviewer on), never in the repo:

- `WINDOWS_CERT_PFX_BASE64`, `WINDOWS_CERT_PASSWORD`: Windows code signing.
- `APPLE_*` (certificate, certificate password, ID, team ID, app-specific password): macOS signing and notarization.

No workflow step uses them yet.

Repo owner: enable in Settings

- Secret scanning and push protection.
- Dependabot alerts.
- Private vulnerability reporting.
- Branch protection on `main` (require the `ci-ok` check, no force push).
- Actions > General > "Require approval for all outside collaborators" for fork PR workflows.
