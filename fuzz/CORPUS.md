# Synthetic certificate corpus

`corpus/certificate_policy/` contains public test certificates only, never private
keys or deployment identities. The RSA PEM files are copied from `tests/fixtures/`;
their DER equivalents decode the same certificate bodies. `truncated.der` and
`trailing.der` derive from the RSA-2048 seed. `chain-16` and `chain-17` select the
structured chain-count boundary cases in the harness.

The EC/Ed25519 certificates are self-signed synthetic `synthetic-fuzz.invalid`
identities generated with OpenSSL `req -x509`, an ephemeral key and these algorithms:

- `-newkey ec -pkeyopt ec_paramgen_curve:prime256v1`
- `-newkey ec -pkeyopt ec_paramgen_curve:secp224r1`
- `-newkey ed25519`

The temporary private keys were not retained. Trust, signatures, names and expiry
are deliberately outside this strength-parser oracle. The existing rustls
handshake tests cover trust separately. Seed contract tests assert the expected
strength verdict, so silently changing a positive seed into a rejection fails CI.
