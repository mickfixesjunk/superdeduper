# Cutting a release

This is the maintainer checklist for producing a "safe-EXE" GitHub
release. See [SECURITY.md](SECURITY.md) for what end-users do with the
artifacts.

## Prerequisites (one-time)

1. **Sigstore attestations** — nothing to do. The
   `actions/attest-build-provenance` action works out-of-the-box with
   the `id-token: write` permission already wired up in
   `.github/workflows/release.yml`. Free, no setup.
2. **Authenticode signing (optional, recommended).** Buy an OV
   code-signing certificate from a CA Microsoft trusts (Sectigo /
   DigiCert / GlobalSign). EV certs short-circuit SmartScreen entirely
   but cost more. Once you have a `.pfx` file:
   ```pwsh
   $bytes = [IO.File]::ReadAllBytes("codesign.pfx")
   [Convert]::ToBase64String($bytes) | Set-Clipboard
   ```
   Add two repo secrets at
   `Settings → Secrets and variables → Actions`:
   * `CODE_SIGN_PFX_BASE64` — the base64 string above
   * `PFX_PASSWORD` — the PFX's password
   The release workflow detects these and signs automatically.

## Cutting a release

1. **Update the version** in `Cargo.toml` and run `cargo update -p superdeduper`.
2. **Update `Cargo.lock`** (`cargo build --locked` to verify it's consistent).
3. **Commit:** `git commit -am "Release vX.Y.Z"`.
4. **Tag** with the same `vX.Y.Z`:
   ```bash
   git tag -a vX.Y.Z -m "Release vX.Y.Z"
   git push origin vX.Y.Z
   ```
5. The release workflow runs automatically against the tag, builds
   each target, attests, signs (if cert is configured), and uploads a
   **draft** release with the artifacts. Review the draft, write
   notes, and publish.

## Verifying your own release

After the workflow finishes, sanity-check the published artifacts the
same way an end user would:

```pwsh
gh release download vX.Y.Z --pattern "superdeduper-*.zip" --pattern "*.sha256"
foreach ($z in Get-ChildItem *.zip) {
  $exp = (Get-Content "$($z.Name).sha256").Split(' ')[0]
  $act = (Get-FileHash -Algorithm SHA256 $z).Hash.ToLower()
  if ($exp -ne $act) { throw "MISMATCH on $z" }
}
gh attestation verify superdeduper-x86_64-windows.zip --repo mickfixesjunk/superdeduper
```

If `attestation verify` returns "verification succeeded" and every
SHA-256 matches, the release is good.

## What you must never do

* Sign locally and upload by hand. The whole point of the workflow is
  that nobody is in the trust chain except GitHub Actions + Sigstore.
* Edit a release's artifacts after publishing. If you need to fix
  something, cut a new patch release.
* Commit a `.pfx` file to the repo, even encrypted.
* Lower the `--locked` flag in `release.yml`. Lockstep dependencies
  are how we guarantee the build is reproducible.
