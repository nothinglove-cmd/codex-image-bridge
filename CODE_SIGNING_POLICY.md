# Code Signing Policy

Free code signing is provided by [SignPath.io](https://about.signpath.io), with
the certificate provided by the [SignPath Foundation](https://signpath.org).

This policy applies after SignPath Foundation accepts the project. Releases up
to and including `v0.4.6` are unsigned. `v0.4.7` is reserved for the first
SignPath-signed Windows release and must not be published until its signature
and release metadata have been verified.

## Project and team

- Source repository: <https://github.com/nothinglove-cmd/codex-image-bridge>
- Committer and reviewer: [nothinglove-cmd](https://github.com/nothinglove-cmd)
- Signing approver: [nothinglove-cmd](https://github.com/nothinglove-cmd)
- Security policy: [SECURITY.md](SECURITY.md)
- Privacy policy: [PRIVACY.md](PRIVACY.md)

Maintainers must enable multi-factor authentication for both GitHub and
SignPath. Changes from contributors who do not have direct write access must be
reviewed before merge. Build scripts, signing workflows, and SignPath policy
files receive the same review as application source code.

## Signed artifact

Only the Windows x64 `CodexImageFix.exe` built from this repository is signed
under this project. The official Codex executable and its helper programs are
located and validated at runtime; they are not bundled, rebuilt, or signed by
this project.

The signed executable must retain the product name, product version, original
file name, and publisher metadata configured by the repository build. SignPath
artifact configuration restrictions must enforce these attributes.

## Build and approval

1. A versioned commit is tagged with a `v*` tag whose value matches
   `Cargo.toml`.
2. GitHub Actions builds the executable from that tag on a GitHub-hosted Windows
   runner using the locked Rust dependency graph.
3. The unsigned executable is uploaded as a GitHub Actions artifact before it
   is submitted through the official SignPath GitHub Action.
4. Every signing request requires manual approval by the signing approver.
5. The signed executable is downloaded into the same workflow and its Windows
   Authenticode status is required to be `Valid`.
6. SHA256, SBOM, dependency inventory, and build information are generated or
   regenerated from the signed executable.
7. Signed assets may replace assets only in the matching draft GitHub Release.
   Publishing the Release remains a separate manual action.

Private signing keys are held by SignPath and are never stored in this
repository or GitHub Secrets. The SignPath API token is stored only as an
encrypted GitHub Actions secret and must not be printed to logs.

## Verification

Users can verify a downloaded release on Windows with:

```powershell
Get-AuthenticodeSignature .\CodexImageFix.exe | Format-List
$expected = (Get-Content .\CodexImageFix.exe.sha256).Split(' ')[0]
$actual = (Get-FileHash .\CodexImageFix.exe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "CodexImageFix.exe SHA256 mismatch" }
```

The Authenticode status must be `Valid`, and the SHA256 must match the checksum
asset from the same GitHub Release.
