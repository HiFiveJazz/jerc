# Windows package managers

Jerc's `x86_64-pc-windows-msvc` release ZIP is the input for all three
package-manager formats. `scripts/generate-windows-packages.py` validates the
ZIP, checks its internal path and Windows executable header, computes its SHA-256,
and generates:

- **Scoop:** `scoop/jerc.json` for a bucket (download + executable shim).
- **WinGet:** `winget/manifests/h/HiFiveJazz/Jerc/VERSION/` for an upstream PR.
  Uses `InstallerType: zip` and `NestedInstallerType: portable`.
- **Chocolatey:** a self-contained `jerc.VERSION.nupkg` with an automatically
  shimmed `tools/jerc.exe`. No installation PowerShell script is required.

The manual `test-windows-packages.yml` workflow takes an existing GitHub Release
tag and exercises independent Scoop, WinGet, and Chocolatey jobs. The tagged
`release.yml` generates and checks release-ready metadata before uploading it.

Scoop/WinGet manifests are **not discoverable** until submitted to their
respective repositories or a bucket. Chocolatey packages need Chocolatey
Community Repository submission for `choco install jerc` by default.
WinGet on GitHub's Windows Server runners may be missing; the CI job clearly
reports that native WinGet validation/installation is unverified in that case,
while still enforcing portable-manifest checks.

Until the release is published, the public GitHub URLs in Scoop and WinGet
manifests are not reachable, so a normal tag-triggered release can validate the
manifests and Chocolatey package, but full Scoop and WinGet installs require a
post-release test workflow.
