# Synthetic TLS boundary fixtures

These public certificates are generated test identities, never deployment credentials. No private key is distributed. They test the exact 2048-bit RSA policy boundary: normal WebPKI certificate verification accepts a 2047-bit leaf, while the additional project policy rejects it.

Self-signed 1024/2047/2048-bit certificates exercise the policy parser and root loading. The 2047-bit localhost leaf is signed by the 2048-bit root and has serverAuth usage. Regenerate with OpenSSL `req -x509 -newkey rsa:<bits>` and sign a localhost CSR with the 2048-bit root; retain CA:FALSE, digitalSignature/keyEncipherment, serverAuth and localhost/127.0.0.1 SANs on the leaf. Existing live loopback tests separately verify that untrusted or wrong-name certificates prevent MQTT credentials from being sent.
