# Privacy Policy

Comidea Codex Image Bridge is a local compatibility tool. It does not include
telemetry, analytics, advertising, automatic update checks, or a Comidea-hosted
data collection service.

This program will not transfer any information to other networked systems
unless specifically requested by the user or the person installing or
operating it.

## Local data

The Windows application may read Codex session JSONL files and Codex SQLite
state databases to locate generated images. SQLite databases are opened
read-only. The application does not modify original session files. Restored
images, installation state, configuration backups, and operational state are
written locally under the current user's Codex or local application data
directories.

When the user explicitly saves a model-service configuration, the server URL,
headers, feature setting, and API credential are written to that user's Codex
configuration files. API credentials are not written to application logs or
diagnostic bundles.

## Network access

The application contacts a network service only when the user explicitly tests
a configured model-service connection. That test sends authenticated requests
to the server URL selected by the user. Normal prompts, images, and model
requests are sent by the official Codex client to the provider configured by
the user as part of the user's requested Codex operation.

The macOS bridge exposes a temporary interface bound to `127.0.0.1` and keeps
the API credential in process memory. It passes the user-selected provider
configuration to the Codex process and does not persist that credential.

The privacy terms of GitHub, OpenAI, and any user-selected compatible model
provider apply independently when the user accesses those services.

## Diagnostics

Diagnostic and support-bundle commands produce local files. They exclude API
credentials, authorization headers, prompts, session bodies, and Base64 image
content. No diagnostic file is uploaded automatically; the user decides
whether and where to share it.

## Contact

Privacy questions can be submitted through the repository's GitHub issue
tracker without including credentials, prompts, images, or session content.
