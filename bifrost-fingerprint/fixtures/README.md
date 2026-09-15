# Fingerprint golden fixtures

`fingerprint_golden.json` was produced by running the original JavaScript
implementation, not by re-deriving the algorithm from a description.

The generator extracts the fingerprint region of `commandcode-proxy/proxy.mjs`
verbatim, evaluates it, and records the output for a set of API keys and salts.
Executing the original code is the point: a hand-written expectation would encode
the same misunderstanding as a hand-written port.

Regenerate only when the reference implementation changes, and treat any change
to existing expectations as a breaking change — every account using an affected
key would silently move to a different device.
