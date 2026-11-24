# Verifying releases

Every release publishes two static binaries, `swarm-device-access-linux-amd64` and
`swarm-device-access-linux-arm64`, with a `checksums.txt` over both, on the GitHub release, and a
multi-arch image at `ghcr.io/leinardi/swarm-device-access:<version>` (`linux/amd64`,
`linux/arm64`). Each platform's image binary is that platform's release binary, byte for byte.

Each binary and the image carry a build provenance attestation from the release workflow, and the
image is signed with cosign, keyless, by that workflow's identity. Check them with the GitHub CLI
and cosign:

```bash
gh attestation verify swarm-device-access-linux-amd64 --repo leinardi/swarm-device-access \
  --signer-workflow leinardi/swarm-device-access/.github/workflows/release.yaml
gh attestation verify oci://ghcr.io/leinardi/swarm-device-access:<version> --repo leinardi/swarm-device-access \
  --signer-workflow leinardi/swarm-device-access/.github/workflows/release.yaml
cosign verify ghcr.io/leinardi/swarm-device-access:<version> \
  --certificate-identity https://github.com/leinardi/swarm-device-access/.github/workflows/release.yaml@refs/heads/master \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The workflow signs with cosign v3, which stores the signature as a Sigstore bundle in an OCI 1.1
referring artifact, so verifying it needs cosign v3, or v2.6 or later with `--new-bundle-format`.

The release notes name the Rust toolchain the binaries were built with and the base image digest
each platform image was built on.

## Image tags

`:<version>` names one release. `:<major>` and `:latest` move to each new release that is the
highest released version, so pin a version, or a digest, where an update must be deliberate.
