# Captured registry signing fixtures for #3928

These responses and Sigstore bundles were captured from the production Carina
development registry. Certificate-shape tests parse their real Fulcio leaf
certificates and feed the extracted identities into the pure signer decision
function. This covers the stable tag rule, independent release-channel pins,
and the shared source gate without storing multi-megabyte provider artifacts.

Full offline verifier tests use the small Sigstore conformance fixtures in the
parent directory instead:

- `../a.txt` is the signed artifact.
- `../bundle.sigstore.json` is its real conformance bundle, verified against
  Carina's embedded production trust root.

The captured JSON files remain byte-for-byte registry responses. Tests adjust
only listing expiry where needed so the captured anti-freeze timestamp does not
expire as a test input. No test emulates a Fulcio, Rekor, or CT chain.
