# bitd and bitctl

Rust machine built-in tests: continuous lightweight health checks and manually
triggered regressions, monitored through a gRPC CLI and terminal UI.

```sh
bitctl                       # live TUI: arrows, Enter, r, q
bitctl list --json           # machine-readable snapshot
bitctl run --wait            # all configured regressions; nonzero on failure
bitctl run internet --wait   # one explicit check
journalctl -u bitd -f        # plugin stdout/stderr and run outcomes
```

The server defaults to loopback port 40041 in NixOS. Override the client with
`--endpoint` or `BITD_ENDPOINT`. Remote use is through SSH forwarding, e.g.
`ssh -L 40042:127.0.0.1:40041 host` and
`bitctl --endpoint http://127.0.0.1:40042`. Any local user can trigger installed
checks; the API deliberately cannot install commands or supply configurations.

## Build

Clone [aapis](https://github.com/goromal/aapis) next to this repository. During
initial bring-up both use branch `dev/bitd`. Install Rust, a C compiler, and
protoc, then run `cargo test --locked` and `cargo build --locked`.
`AAPIS_PROTO_DIR` overrides the default `../aapis/protos`. The contract is owned
by aapis, not copied here. NixOS packaging supplies the locked aapis source.

## Runtime configuration

`bitd --config /path/to/config.json` loads a configuration once at startup.
`--check-config` validates without starting checks. Restart to load changes.

```json
{
  "listen": "127.0.0.1:40041",
  "checks": [
    {
      "name": "internet",
      "kind": "health",
      "command": "/absolute/path/to/bit-check-internet",
      "rate_hz": 0.2,
      "timeout_ms": 4500,
      "config": {}
    }
  ]
}
```

Plugin ABI v1 is executable + stdin JSON + stdout logs + exit status. All
plugins implement `plugin::BitCheck`. Independent checks run concurrently;
one check never overlaps itself. Health schedules skip missed ticks. All
processes in the plugin's process group are killed on timeout or daemon stop.
Run IDs and latest results are in memory and reset on daemon restart; journals
retain logs. `run --wait` reports an error if a subsequent run supersedes the
one it was waiting for, rather than claiming a result from another run.

Initial plugins:

* `internet`: tries configurable `urls` for HTTPS 204 responses (Google and
  Cloudflare by default), checking DNS/TLS/HTTP and rejecting captive portals.
* `avahi`: checks the system service and resolves a configured `.local`
  `hostname` through Avahi. This establishes local responder health, not
  reachability from every other LAN host.
* `cozy_imgedit2`: requires `url`, `token_file`, and `png_command` (the
  anixpkgs `png` tool); `max_seconds` defaults to 900. Requires an idle Cozy
  queue, generates a random `width`×`height` (default 512×512) PNG with
  `png <seed>-<w>-<h>.random` (a fresh `seed` per run unless configured; it is
  logged so a failure can be reproduced), and asks `imgedit2` to add bright red
  markings. It tracks its job ID and passes when the result decodes, keeps the
  input's aspect ratio within 2%, and has at least `min_red_fraction` (default
  0.005) more strongly red 8×8 blocks than the input. Noise averages to grey per
  block, so its baseline is near zero. It tests workflow execution, not
  general image quality. If no valid output arrives within `max_seconds` (or the job fails),
  the check fails immediately and cancels its own job through Cozy's
  `/api/queue/cancel`, which removes it if pending or interrupts it in ComfyUI
  if running. Uploaded inputs and completed outputs remain in Cozy for
  inspection. It never clears the shared queue or restarts ComfyUI.
* `cozy_imggen_quantized`: requires `url` and `token_file`. Requires an idle
  Cozy queue, queues one `workflow` (default `imggen-quantized`) generate job at
  `width`×`height` (default 500×500), and passes when a result of that size (or
  floored to the model's 8-px latent grid, e.g. 500 → 496), optionally times a
  whole upscale factor (imggen-quantized upscales 4×: 1984×1984), and not blank arrives within `max_seconds` (default 60). Otherwise it
  fails and cancels its own job.

## NixOS

`anixpkgs` provides `services.bitd.enable`, a merged list
`services.bitd.checks`, and plugin definitions under `services.bitd.plugins`.
Only selected plugin outputs enter the service configuration. Internet runs
at 0.2 Hz on pc-base, Avahi at 0.2 Hz on personal/Jetson, and Cozy is installed
only on personal-dell. Cozy's total daemon deadline defaults to 900 seconds;
its polling budget is 870 seconds to leave time for setup and cleanup. Adjust
both `timeoutMs` and `settings.max_seconds` for a different bound.

Cozy expects the existing `~/secrets/flask/cozy-api-token` file and matching
`api_token_hash` in its server secrets. Missing credentials fail the regression
check without preventing bitd or health checks from starting. Secrets do not
enter the Nix store. bitd runs as the primary user with a read-only filesystem
view; plugins that need local writes should use their private temporary dir.
