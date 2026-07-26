# SignPath Foundation Setup

This document tracks the one-time maintainer setup required before the first
signed release. Do not place API tokens, credentials, form responses, or email
addresses in this repository.

## Application

1. Make the GitHub repository public and enable multi-factor authentication on
   the maintainer account.
2. Open <https://signpath.org/apply> and submit the embedded application form.
3. Use these public project details:
   - Project: `Comidea Codex Image Bridge`
   - Repository: <https://github.com/nothinglove-cmd/codex-image-bridge>
   - Current release: <https://github.com/nothinglove-cmd/codex-image-bridge/releases/tag/v0.4.6>
   - License: MIT
   - Artifact: Windows x64 PE executable, `CodexImageFix.exe`
   - Build system: GitHub Actions on a GitHub-hosted Windows runner
   - Code signing policy: [CODE_SIGNING_POLICY.md](CODE_SIGNING_POLICY.md)
   - Privacy policy: [PRIVACY.md](PRIVACY.md)
4. Provide the maintainer's own contact name and email, review the SignPath
   Foundation conditions, and submit the form. These legal confirmations must
   be completed by the maintainer, not an automation account.

SignPath Foundation decides whether to accept the project. The project is new,
so acceptance is not guaranteed and may require additional public maintenance
history or clarification.

## GitHub configuration after acceptance

Create this encrypted Actions secret:

- `SIGNPATH_API_TOKEN`

Create these Actions repository variables using the exact values supplied or
configured in SignPath:

- `SIGNPATH_ORGANIZATION_ID`
- `SIGNPATH_PROJECT_SLUG`
- `SIGNPATH_SIGNING_POLICY_SLUG`
- `SIGNPATH_ARTIFACT_CONFIGURATION_SLUG`

The token must belong to a SignPath user with submitter permissions for the
configured project and signing policy. Do not pass the token as a command-line
argument or print it while configuring the repository.

## First signed release

1. Confirm the SignPath artifact configuration accepts only
   `CodexImageFix.exe` and enforces the product metadata described in
   [CODE_SIGNING_POLICY.md](CODE_SIGNING_POLICY.md).
2. Push the prepared `v0.4.7` tag. The normal release workflow creates a draft
   Release with unsigned build artifacts.
3. Run the `Sign draft release with SignPath` workflow manually and enter
   `v0.4.7`.
4. Approve the signing request in SignPath when prompted.
5. Confirm the workflow replaces the draft assets with the signed executable
   and regenerated metadata.
6. Download the draft executable, verify Authenticode and SHA256 on a clean
   Windows machine, complete the release test matrix, and then publish the
   GitHub Release manually.
