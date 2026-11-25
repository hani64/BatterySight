# Security policy

## Reporting a vulnerability

Report it privately through GitHub: the repository's **Security** tab → **Report a vulnerability** ([direct link](https://github.com/hani64/BatterySight/security/advisories/new)). Please do not open a public issue.

Include the BatterySight version, your Windows version and build, and the steps or input that trigger it. You will usually get a reply within a week.

## Supported versions

Only the latest release gets fixes.

## In scope

- Parsing of device reports (bytes from any HID device are untrusted input).
- The files BatterySight reads and writes under `%LOCALAPPDATA%\BatterySight`.
- The release binaries and the workflow that builds them.
