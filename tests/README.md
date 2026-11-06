# Integration coverage

cli.rs covers static alias persistence, replacement, removal, and rejection without corrupting existing state. Remaining scenarios below require coverage as the implementation stabilizes.

Required first scenarios:

- Start a child HTTP fixture on an assigned port, request it through the proxy with the registered Host, and assert status, body, and query preservation.
- Register two distinct hosts and verify routing isolation and an unknown-host response.
- Terminate the child and verify route cleanup and exit status propagation.
- Add, list, resolve, and remove a static alias in an isolated state directory.
- Pass child arguments after `--`, including flags and spaces, and verify exact argument preservation.
- Reject invalid ports and names without corrupting existing state.
- Exercise streaming and WebSocket upgrade end to end once proxy support exists.

Use a unique temporary PORCH_STATE_DIR for every test, loopback and ephemeral ports, readiness polling with a deadline, and RAII process cleanup on assertion failures. Never modify hosts files, OS services, or system trust stores. Do not invoke public tunnels in ordinary CI. Avoid assertions that merely duplicate parser implementation. No additional Cargo dependencies are required until their owner authorizes them.
