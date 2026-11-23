Synthetic TLS identity for loopback TURN tests only. The private key is public
fixture data, never an account or production credential. SAN relay.fixture.invalid
uses the reserved .invalid domain. Production trust never includes this certificate.
Self-signed RSA2048/SHA256, valid 2020–2040 for stable fixture execution. Tests load
it into an isolated root store; wrong hostname and untrusted chains must fail.
