# Security

Report a vulnerability through GitHub's private reporting: the **Security** tab, then
**Report a vulnerability**. Please do not open a public issue for it.

The daemon holds a boat API key and a GitHub token that can register self-hosted
runners, so reports about credential handling, runner registration, or a job reaching
another job's machine are in scope. Only the latest release is supported; fixes land on `main` and ship in the next
release.
