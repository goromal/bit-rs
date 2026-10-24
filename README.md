# bitd and bitctl

Rust machine built-in tests: continuous lightweight health checks and manually
triggered regressions, monitored through a gRPC CLI and terminal UI.

```sh
bitctl                       # live TUI (failed checks first): arrows, Enter, r, q
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

A regression check may set `"exclusive": true` when it needs the machine's
resources to itself (a GPU, a shared job queue). It then never runs alongside
another regression: it waits for running ones to finish, and regressions
requested after it wait for it, in request order. A waiting check reports
`RUNNING` with a `queued: ...` detail, and its timeout and start time begin
when it is admitted. Health checks are lightweight by contract and keep their
schedule regardless.

Plugin ABI v2 is executable + stdin JSON + stdout logs + exit status. All
plugins implement `plugin::BitCheck`, including `remediation()`: invoked with
`--remediation` and the same stdin JSON, a plugin prints plain-text
suggestions for when the check fails and exits 0 without checking anything.
bitd queries every plugin once at startup (5 s deadline, 4 KiB cap) and
exposes the text as `Check.remediation`; a plugin that cannot answer just
leaves it empty. bitctl shows it under the highlighted check, and
`list --json` includes it. Independent checks run concurrently;
one check never overlaps itself. Health schedules skip missed ticks. All
processes in the plugin's process group are killed on timeout or daemon stop.
Run IDs and latest results are in memory and reset on daemon restart; journals
retain logs. `run --wait` reports an error if a subsequent run supersedes the
one it was waiting for, rather than claiming a result from another run.

Initial plugins:

* `internet`: tries configurable `urls` for HTTPS 204 responses (Google and
  Cloudflare by default), checking DNS/TLS/HTTP and rejecting captive portals.
* `avahi`: checks the system service, fails when Avahi serves this machine
  under a name other than the configured `.local` `hostname` (a name conflict
  renames it to `<name>-2.local` etc., so mDNS peers such as Nexus list it
  twice), and resolves `hostname` through Avahi. This establishes local
  responder health, not reachability from every other LAN host. `systemctl`,
  `busctl`, and `avahi_resolve_host_name` override the commands used.
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
  inspection. It never clears the shared queue.
* `cozy_imggen_quantized`: requires `url` and `token_file`. Requires an idle
  Cozy queue, queues one `workflow` (default `imggen-quantized`) generate job at
  `width`×`height` (default 500×500), and passes when a result of that size (or
  floored to the model's 8-px latent grid, e.g. 500 → 496), optionally times a
  whole upscale factor (imggen-quantized upscales 4×: 1984×1984), and not blank arrives within `max_seconds` (default 60). Otherwise it
  fails and cancels its own job.
* Both Cozy checks take an optional `restart_command` argv. Once their job has
  ended, pass or fail, they run it (20 s limit) so ComfyUI frees the RAM its
  loaded models hold; a failed restart fails the check. Nothing is restarted
  when the check stops before submitting a job, e.g. on a busy queue.
* `ardupilot`: connects to a MAVLink TCP `address` (e.g. mavlink-router's GCS
  port) and passes when a CRC-valid HEARTBEAT from an autopilot (not a GCS),
  optionally from `system_id`, arrives within `max_seconds` (default 3).
* `ros2_topics`: given the `ros2` CLI and `topics` (`name`, optional `type`),
  passes when each topic delivers a message within `max_seconds` (default 8),
  via `ros2 topic echo --once --no-daemon` so no ros2 daemon outlives the check.
  ROS state and logs go to a private temporary `ROS_HOME`.
* `ssh_known_hosts`: requires `hosts` and `files`; passes when every host has
  an entry (via `ssh-keygen -F`, so hashed entries count) in at least one
  existing file. Unattended git-over-SSH jobs cannot prompt to trust a missing
  key, so a deleted `~/.ssh/known_hosts` otherwise breaks them silently.
  Optional `seeds` names backup known_hosts files that remediation suggests
  restoring from.

## NixOS

`anixpkgs` provides `services.bitd.enable`, a merged list
`services.bitd.checks`, and plugin definitions under `services.bitd.plugins`.
It also installs bitctl with the shell alias `b`.
Only selected plugin outputs enter the service configuration. Internet runs
at 0.2 Hz on pc-base, Avahi at 0.2 Hz on personal/Jetson, Cozy (exclusive)
on personal-dell and the Jetson, and `ardupilot` (0.2 Hz) and `ros2_topics`
on the drone closures. Cozy's total daemon deadline defaults to 900 seconds;
its polling budget is 870 seconds to leave time for setup and cleanup. Adjust
both `timeoutMs` and `settings.max_seconds` for a different bound.

Cozy expects the existing `~/secrets/flask/cozy-api-token` file and matching
`api_token_hash` in its server secrets. Missing credentials fail the regression
check without preventing bitd or health checks from starting. Secrets do not
enter the Nix store. bitd runs as the primary user with a read-only filesystem
view; plugins that need local writes should use their private temporary dir.
