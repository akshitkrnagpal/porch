# Release size

Porch 0.4.0 archives were 6.7 to 14.8 MB. Three changes make them smaller
without changing what users install or how it runs:

1. **macOS archives store each executable once.** `Porch.app` carries its own
   `porch` and `porch-tray`, identical to the ones at the archive root. They are
   now tar hard links to those files instead of second copies
   ([packaging README](../packaging/README.md) explains what users get).
2. **A size-focused release profile** in `Cargo.toml`: `opt-level = "s"`,
   `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`. Panics still unwind.
3. **Fewer dependency features.** `reqwest` no longer enables its default
   `native-tls` backend next to rustls (on Linux that linked OpenSSL), and
   `hyper-util` enables only the parts Porch uses instead of `full`.

## Archive and executable sizes

### All four targets, built by the Release workflow

<!-- CI-RESULTS -->

### aarch64-apple-darwin, built locally

Same machine and toolchain for both sides (Apple M4, macOS 27.2, rustc 1.95.0,
`cargo build --release --bin porch` and
`cargo build --release --features tray --bin porch-tray`). Baseline is
`origin/main` at `d697112` (Release 0.4.0); each side packaged with its own
`scripts/package.py`, compared by `scripts/release_size.py`.

| Item | Baseline bytes | After bytes | Change |
| --- | ---: | ---: | ---: |
| `porch` | 15,695,936 | 6,092,256 | -9,603,680 (-61.2%) |
| `porch-tray` | 2,340,480 | 1,258,848 | -1,081,632 (-46.2%) |
| `Porch.app/Contents/MacOS/porch` | 15,695,936 (copy) | 6,092,256 (hard link) | -61.2% |
| `Porch.app/Contents/MacOS/porch-tray` | 2,340,480 (copy) | 1,258,848 (hard link) | -46.2% |
| Executable bytes stored in the tar | 36,072,832 | 7,351,104 | -28,721,728 (-79.6%) |
| **Archive (.tar.gz)** | **14,641,521** | **3,850,193** | **-10,791,328 (-73.7%)** |

How much each change contributes to that archive:

| Binaries | Packaging | Archive bytes | vs baseline |
| --- | --- | ---: | ---: |
| baseline | copies (0.4.0) | 14,641,521 | |
| baseline | hard links | 7,409,071 | -49.4% |
| new profile + features | copies (0.4.0) | 7,529,148 | -48.6% |
| new profile + features | hard links | 3,850,193 | -73.7% |

### Release profile candidates

`porch` only, baseline source, aarch64-apple-darwin, same toolchain:

| Profile | `porch` bytes | vs baseline | Clean build |
| --- | ---: | ---: | ---: |
| default release (baseline) | 15,695,936 | | |
| + `strip = "symbols"` | 12,904,256 | -17.8% | 84 s |
| + thin LTO | 13,475,984 | -14.1% | 83 s |
| + fat LTO, `codegen-units = 1` | 8,358,464 | -46.7% | 184 s |
| + fat LTO, `codegen-units = 1`, `opt-level = "s"` (**chosen**) | 6,194,672 | -60.5% | 163 s |
| + fat LTO, `codegen-units = 1`, `opt-level = "z"` | 4,873,088 | -69.0% | 159 s |

The dependency feature changes take `porch` from 6,194,672 to 6,092,256 bytes
on macOS. On Linux they also remove the `libssl.so.3`/`libcrypto.so.3` runtime
dependency.

## Speed

Size settings can cost speed, so the proxy was measured with
`scripts/proxy_cpu.py`, which records the proxy process's own CPU time (not
requests per second, which the load generator and backend dominate). Binaries
alternate between rounds; 15 samples each, medians shown, same machine.

| Binary | CPU s / 8,000 keep-alive HTTP requests | CPU s / 4,000 HTTPS handshakes |
| --- | ---: | ---: |
| baseline | 0.91 | 1.52 |
| after (this change) | 0.90 | 1.57 (+3%, about 12 µs per handshake) |
| `opt-level = "s"`, baseline features | 0.90 | 1.56 |

An earlier `ab`-driven run with 100,000 requests per sample put
`opt-level = "z"` at 15-20% more proxy CPU per request and about 25% more per
TLS handshake, while `opt-level = "s"` stayed within noise of baseline. That is
why `s` was chosen over the smaller `z`. Release builds take longer (one
codegen unit and fat LTO); the Release workflow's test step keeps the faster
settings for test binaries only.

## Behavior

- Installed files and paths are unchanged. Extracting a macOS archive still
  gives `porch`, `porch-tray` and a complete `Porch.app` (checked with `tar`,
  Finder's Archive Utility, the new `install.py` and the 0.4.0 `install.py`).
  `porch self-update` and Homebrew extract with `tar`, which recreates the
  hard links. The installed app and `~/.local/bin` still hold separate copies.
- HTTPS clients that used `native-tls` (the GitHub release check in
  `porch self-update`, and the `porch hosts sync` call to a local HTTPS proxy)
  now use rustls with the operating system's trust store, which is what
  `native-tls` used as well. One difference: when `SSL_CERT_FILE` or
  `SSL_CERT_DIR` is set, rustls loads roots from there instead of the
  platform store. On Linux OpenSSL honored these too; on macOS and Windows a
  CA trusted only in the system store is then not used for these two calls. Proxy environment variables and system proxy
  settings are honored as before. The GitHub client may now use HTTP/2; the
  local hosts-sync call is pinned to HTTP/1.1 as before. Discovery and pairing
  already used rustls with pinned certificates and are unchanged.

## Reproduce

```sh
# Archives for every target, from any branch (uploads artifacts, never publishes)
gh workflow run release.yml --ref BRANCH
gh run download RUN_ID --dir after/        # likewise a run on main into before/
python3 scripts/release_size.py before/ after/   # searches artifact subfolders

# Proxy CPU on this machine
python3 scripts/proxy_cpu.py path/to/old/porch path/to/new/porch --rounds 5 \
    --requests 8000 --handshakes 4000
```

## Limits

- Speed was measured on one Apple Silicon Mac only; the x86_64 macOS, Linux and
  Windows builds were measured for size, not speed.
- The tray UI was not exercised interactively on any platform; tray unit tests
  and packaging tests passed.
- Release builds now take roughly twice as long to compile.
- `scripts/proxy_cpu.py` ignores inherited `PORCH_*`/`PORTLESS_*` settings and
  disables hosts-file sync, so it only uses its temporary state directories.
