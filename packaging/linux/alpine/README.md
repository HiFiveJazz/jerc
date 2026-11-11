# Alpine Linux APK

`package-linux-apk.sh` uses nFPM v2.48.0 and the existing static musl Jerc
release archive to create `jerc-VERSION-r0.apk` for Alpine x86-64.
The package includes `/usr/bin/jerc`, docs, license and example config, and
uses `ca-certificates` from Alpine.

Install the unsigned APK from GitHub Releases using:

```sh
sudo apk add --allow-untrusted ./jerc-0.6.3-r0.apk
```

This is a local install, **not** an Alpine repository package. To support
ordinary `apk add jerc` and repository-managed updates, submit a source-based
`APKBUILD` to Alpine's `aports` (typically `testing` first). A signed repository
would be another option.
