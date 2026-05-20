# Security & release verification

`superdupe` ships as a single `.exe` per architecture, distributed
exclusively through this repository's GitHub Releases. Every release
artifact is **reproducibly built in public CI** and verifiable via at
least one of three independent mechanisms:

1. **SHA-256 manifest.** Every release includes a `SHA256SUMS` inside
   the zip plus a sidecar `.sha256` next to it. Always present, easy
   to spot-check.
2. **GitHub artifact attestation (Sigstore-backed).** A tamper-evident
   bundle in the public transparency log says: "these exact bytes were
   produced by the `release.yml` workflow running on commit `<sha>` at
   `<timestamp>`." No shared secret, no signing key to compromise,
   free, verifiable by anyone with the GitHub CLI.
   **Caveat:** GitHub restricts attestations to public repositories
   and organization-owned private repositories. User-owned private
   repos will not have attestations on their releases — for those,
   rely on the SHA-256 manifest plus Authenticode below.
3. **Authenticode (when a code-signing cert is configured).** Windows
   SmartScreen / EDR will recognise the binary as coming from the
   `superdupe` publisher, suppress the unknown-publisher prompt, and
   refuse to launch if anything's been modified post-signing.

You only need to verify **one** of these to be confident the binary is
genuine, but verifying all three is the gold standard.

## Verifying a downloaded release

Pick a release at <https://github.com/mdreeling/superdupe/releases>,
download the per-architecture zip plus its `.sha256` file, and:

### 1. Verify the SHA-256 (one-liner)

```pwsh
# PowerShell
$expected = (Get-Content superdupe-x86_64-windows.zip.sha256).Split(' ')[0]
$actual   = (Get-FileHash -Algorithm SHA256 superdupe-x86_64-windows.zip).Hash.ToLower()
if ($expected -ne $actual) { throw "SHA-256 MISMATCH — DO NOT TRUST THIS FILE." }
"OK: $actual"
```

### 2. Verify the Sigstore attestation (public / org-private repos only)

```pwsh
gh attestation verify superdupe-x86_64-windows.zip --repo mdreeling/superdupe
```

The CLI fetches the attestation from the Sigstore transparency log and
checks that it was produced by **this** repository's `release.yml`
workflow. Expected output: `verification succeeded`.

> **If this repo is a user-owned private repo** the attestation step
> is skipped by the workflow (GitHub doesn't persist attestations for
> those). The release will note "no attestation" in its body. Fall
> back to the SHA-256 manifest and Authenticode signature for trust.

### 3. Verify the Authenticode signature (if present)

```pwsh
Get-AuthenticodeSignature .\superdupe.exe | Format-List *
```

Status should read `Valid`. Subject name will be the publisher
configured in the cert.

## What we sign / what we don't

| Artifact                                | SHA-256 | Sigstore attestation | Authenticode |
| --------------------------------------- | :-----: | :-----: | :-----: |
| `superdupe-x86_64-windows.zip`          | ✅ | ✅ | ✅ when cert configured |
| `superdupe-aarch64-windows.zip`         | ✅ | ✅ | ✅ when cert configured |
| Individual `.exe`s inside the zip       | indirect | indirect | ✅ when cert configured |

## Reporting vulnerabilities

If you find a vulnerability, especially one that could let a malicious
file fool a scan or trick `dedupe` into removing the wrong file,
please report it privately via GitHub's "Report a vulnerability"
button on this repo's Security tab. We aim to acknowledge within 48
hours.

Please don't open public issues for security bugs.

## Things we will never do

* Distribute binaries through any channel other than this repository's
  Releases page.
* Ask you to disable SmartScreen, Defender, or any AV to run the tool.
* Bundle unrelated software, telemetry, or update-checks.

If you see a "superdupe" binary that doesn't pass the verification
steps above, please report it.
