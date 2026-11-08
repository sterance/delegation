# Code signing the Windows build

The repository variable `WINDOWS_SIGNING` decides how pushes to `main` are
signed:

| `WINDOWS_SIGNING` | Signed with | Windows shows |
|---|---|---|
| unset | nothing | "Unknown publisher"; SmartScreen warns |
| `self-signed` | the project's own certificate | "Unknown publisher" and a SmartScreen warning too, unless the machine trusts the certificate |
| `signpath` | the SignPath Foundation's certificate, through [SignPath.io](https://signpath.io) | the SignPath Foundation as publisher |

`self-signed` is the stopgap while the SignPath application is pending.
Moving to SignPath once approved is a variable change, and nothing in the
workflow changes (see [Switching to SignPath](#switching-to-signpath)).
A value other than these three fails the build, so a typo can't
silently publish unsigned files.

## What gets signed

On a push to `main`, `.github/workflows/windows.yml`:

1. builds `delegation-tray.exe` and signs it
2. builds the Inno Setup installer around the **signed** exe
3. signs the installer
4. checks both signatures with `Get-AuthenticodeSignature`, and fails unless they're `Valid`
5. publishes both (the portable exe is the signed tray exe). Self-signed
   releases also include the public certificate,
   `DelegationTray-signing-certificate.cer`, and the release notes give its
   thumbprint.

PR builds and manual runs are never signed. The uninstaller that Inno Setup
writes at install time isn't signed (see the note in `delegation.iss`).
SmartScreen never checks it, since it isn't downloaded.

## Self-signed

### Setup

1. Make the certificate, on Linux or macOS. It goes in `~/.delegation-signing`,
   outside the repository:

   ```sh
   installer/make-self-signed-cert.sh
   ```

   This makes a 3-year, code-signing-only RSA certificate for
   `CN=Chris Smith (Delegation, self-signed)`. It writes the PFX (certificate
   plus private key), its random password, and the public `.cer`, then
   prints the commands for the next step.

2. Add it to GitHub (the script prints these with the real paths):

   ```sh
   base64 < ~/.delegation-signing/delegation-code-signing.pfx | tr -d '\n' | gh secret set SELF_SIGN_PFX_BASE64
   gh secret set SELF_SIGN_PFX_PASSWORD < ~/.delegation-signing/delegation-code-signing.password
   gh variable set WINDOWS_SIGNING --body self-signed
   ```

3. Back up `~/.delegation-signing` somewhere private. Anyone with the PFX and
   password can sign as the project. If they're lost, make a new certificate:
   old releases stay validly signed by the old one.

In CI, `installer/self-sign.ps1` signs each file with `Set-AuthenticodeSignature`
(SHA-256, timestamped by DigiCert's or Sectigo's public timestamp server).
The timestamp keeps signatures valid after the certificate expires. The script
trusts the certificate on the throwaway runner, so the signature check
works the same way as for SignPath.

### What it does and doesn't do

A self-signed signature proves the file is exactly what this project's CI
built, and it hasn't been changed since. Windows doesn't trust the
certificate, though, so it doesn't remove "Unknown publisher" or the
SmartScreen warning.

On machines you control, you can trust it. In an elevated PowerShell:

```powershell
Import-Certificate .\DelegationTray-signing-certificate.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate .\DelegationTray-signing-certificate.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
```

The certificate is only valid for code signing, so this lets it vouch for
programs, not websites. It still means trusting whoever holds the private
key, so don't ask volunteers to do it. For them, SignPath is the real fix.

## SignPath

### 1. Meet the SignPath Foundation's conditions ([terms](https://signpath.org/terms))

These are in place:
- **Licence:** `LICENSE` (MIT) covers all of the code.
- **Project page:** the README's **Code signing policy** section has the
  attribution, the team roles, and a link to its **Privacy** section. The
  signed releases' notes link to that section. Keep the roles table and the
  privacy section current, e.g. if the client starts contacting another
  host.

Still to do:
- **Accounts:** everyone on the team uses MFA on GitHub and SignPath.

Then apply at [signpath.org](https://signpath.org).

### 2. Configure SignPath

Once you're accepted:

- add the predefined **GitHub.com** trusted build system to the
  organisation and link it to the project
- install the **SignPath GitHub app** on this repository
- set the project's default artifact configuration to the contents of
  `signpath-artifact-configuration.xml`. Both signing requests use it, and it
  signs the one exe in each.
- note the signing policy's slug. Use the release-signing policy: the
  Foundation requires a manual approval for every release, and the
  workflow waits up to an hour for each of the two requests.
- create a CI user with submitter rights on that policy, and an API token
  for it

### 3. Configure the repository

Under Settings → Secrets and variables → Actions:

| Kind | Name | Value |
|---|---|---|
| Variable | `SIGNPATH_ORGANIZATION_ID` | the organisation id |
| Variable | `SIGNPATH_PROJECT_SLUG` | the project slug |
| Variable | `SIGNPATH_SIGNING_POLICY_SLUG` | the release-signing policy's slug |
| Secret | `SIGNPATH_API_TOKEN` | the CI user's API token |

### Switching to SignPath

With steps 1–3 done:

```sh
gh variable set WINDOWS_SIGNING --body signpath
```

The next push to `main` is signed through SignPath. Approve its two
requests in SignPath. Once a SignPath-signed release has been published,
you can delete the self-signed secrets:

```sh
gh secret delete SELF_SIGN_PFX_BASE64
gh secret delete SELF_SIGN_PFX_PASSWORD
```

Earlier self-signed releases stay as they are. Also update the README's
code signing policy, which mentions the interim self-signed certificate.

`ci.yml` passes `windows.yml` only these secrets: `SIGNPATH_API_TOKEN`,
`SELF_SIGN_PFX_BASE64` and `SELF_SIGN_PFX_PASSWORD`.

## Checking a signed build

In PowerShell:

```powershell
Get-AuthenticodeSignature .\DelegationTray-Setup-*.exe | Format-List Status, SignerCertificate, TimeStamperCertificate
```

- **SignPath:** `Status` should be `Valid`, with the SignPath Foundation as
  signer. SmartScreen reputation builds per certificate. The Foundation's is
  shared by many projects, but a new file may still warn at first.
- **Self-signed:** `Status` is `UnknownError` ("not trusted by the trust
  provider") unless the machine trusts the certificate. That's expected.
  Check the signer's thumbprint against the one in the release notes:

  ```powershell
  (Get-AuthenticodeSignature .\DelegationTray-Setup-*.exe).SignerCertificate.Thumbprint
  ```

  A `HashMismatch` or `NotSigned` status means the file isn't what CI
  published.
