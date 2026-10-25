# Security

Report vulnerabilities through this repository's **Security → Report a vulnerability** page. Do not put credentials, device serial numbers, local paths, or unredacted captures in a public issue.

The Varlink socket is local and defaults to mode `0600`. Every application running as the same user is trusted to read telemetry, change settings, request UPS controls, and configure PC shutdown. There is no network listener or separate control authorization layer.

PC shutdown is opt-in and uses logind without interactive authorization. It depends on the user manager, power-off permissions, verified battery detection, and a configured runtime model. Runtime estimates and the shutdown reserve are not hardware guarantees.

UPS output controls do not provide device acknowledgements and remain unverified on physical hardware. Use a development simulator for control tests. Dependency advisories, licenses, and sources are checked with `cargo deny` in CI.
