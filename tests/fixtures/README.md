# Test fixtures

`github-app-test-key.pkcs1.b64` and `github-app-test-key.pkcs8.b64` hold one 2048-bit RSA
key, made with `openssl genrsa` for the tests and used nowhere else. No GitHub App knows
it. The files hold the base64 body of the PEM without its `BEGIN`/`END` lines; the tests
in `src/github_app.rs` add them back, as PKCS#1 (the form GitHub downloads) and PKCS#8.
