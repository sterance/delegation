# Code signing the Windows build

Releases are signed through [SignPath.io](https://signpath.io) with a
certificate from the [SignPath Foundation](https://signpath.org), which signs
open-source projects for free. Until it's set up, `windows.yml` builds and
publishes unsigned, as before.

## What gets signed

On a push to `main`, `.github/workflows/windows.yml`:

1. builds `delegation-tray.exe` and uploads it as the `unsigned-tray` artifact
2. sends it to SignPath, waits for the signed copy, and checks the signature
3. builds the Inno Setup installer around the **signed** exe
4. sends the installer to SignPath the same way
5. publishes both (the portable exe is the signed tray exe)

PR builds and manual runs are never signed. The uninstaller that Inno Setup
writes at install time isn't signed (see the note in `delegation.iss`).
SmartScreen never checks it, since it isn't downloaded.

`signpath-artifact-configuration.xml` is the artifact configuration: both
requests use it, and it signs the one exe in each.

## One-time setup

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
  `signpath-artifact-configuration.xml`
- note the signing policy's slug. Use the release-signing policy: the
  Foundation requires a manual approval for every release, and the
  workflow waits up to an hour for it.
- create a CI user with submitter rights on that policy, and an API token
  for it

### 3. Configure the repository

Under Settings → Secrets and variables → Actions:

| Kind | Name | Value |
|---|---|---|
| Variable | `SIGNPATH_ORGANIZATION_ID` | the organisation id. Setting it switches signing on |
| Variable | `SIGNPATH_PROJECT_SLUG` | the project slug |
| Variable | `SIGNPATH_SIGNING_POLICY_SLUG` | the release-signing policy's slug |
| Secret | `SIGNPATH_API_TOKEN` | the CI user's API token |

`ci.yml` passes only this secret to `windows.yml`.

## Checking a signed build

In PowerShell:

```powershell
Get-AuthenticodeSignature .\DelegationTray-Setup-*.exe | Format-List Status, SignerCertificate
```

`Status` should be `Valid`, with the signer being the SignPath Foundation.
SmartScreen reputation builds per certificate, and the Foundation's is
shared by many projects. Even so, a new file may still warn at first.
