# shipd

A local watcher that deploys a site's `main` when every gate passes, and rolls back when the live check fails.

Stage 1: the state machine (`src/step.rs`, one `match`), a config parser, and a dry-run watcher. Personal-site runs in
dry-run: its gates run for real, and deploys are only printed as `would deploy <commit>`.

```sh
cargo build --release
shipd --help
shipd watch --once --dry-run     # one pass over every site
shipd status                     # each site's state in words, and the dry-run count
shipd rollback personal-site     # redeploy the previous live/<n>
shipd mark-live personal-site <commit>
```

Config is `shipd.toml` (`$SHIPD_CONFIG`, or `~/.config/shipd/shipd.toml`). State and history live under
`$SHIPD_HOME`, or `~/.local/share/shipd`. `shipd.service` is a user unit for `shipd watch`, not installed.

## Release and the diff (#2644)

`shipd release SITE [--commit REV]` sends one commit that is on the site's branch through the same machine as `watch`: the gates in a clean
worktree under `$SHIPD_HOME` (never /tmp), then, if the site has a `[site.diff]` section, the live commit and the candidate are both built into
`$SHIPD_OUT` and compared, then the deploy, the live checks and the `live/<n>` tag. The tag's report names the platform's version id (the deploy
output line that starts with `version_line`) and the rollback line.

1. **Diff** (`src/diff.rs`): files added, removed or changed, by kind (pages, stylesheets, scripts, data, images, routing and crawler files, files
   that run). A symlink or an empty folder is an error, never a silent skip. Fingerprints are SHA-256.
2. **Show or hold.** `mode = "show"` prints the diff and goes on. `mode = "hold"` waits for `shipd approve SITE DIGEST`; the digest names this exact
   diff, so a different diff cannot be approved by mistake, and a new commit on the branch replaces the waiting diff. Either way, a diff that changes
   routing or code files waits for `--allow routing,code`.
3. **A page removed with no redirect** in the candidate's `_redirects` blocks the release.
4. **What ships is what was approved.** With a diff step, the candidate folder built for the diff is kept, its tree digest is carried in the
   state, and the deploy command is run with `SHIPD_APPROVED_DIR` set to it. A site's deploy script must ship that folder and not build again. shipd
   hashes the folder once more just before the deploy and refuses if it changed. A deploy script that ignores `SHIPD_APPROVED_DIR` and builds for
   itself would ship a build nobody saw: the site's deploy script is part of the contract. It is held to it two ways. shipd sets
   `SHIPD_APPROVED_DIGEST`, and the script should refuse to upload a folder with another digest. And after the deploy command, shipd requires a line
   `Uploaded digest: <sha256>` in its output equal to the approved digest; a missing line or another digest is a failed deploy that says live may hold a
   build nobody approved. The digest is the one `shipd digest DIR` prints, taken of the folder as the script received it, before the script adds its own files.
5. **Hand deploys.** shipd sets `SHIPD_RELEASE=1` for the gates, the build and the deploy. A site's own deploy script refuses to deploy to production
   without it. This stops the accident, a hand run that builds the working tree; it is an ordinary environment variable and not a lock.
6. **Commands stop.** Any one command (gate, build, deploy, install) is stopped, with its whole process group, after `command_limit_seconds` (1800 when
   left out), with a message. Each build starts from a clean checkout (`git clean -fdx`); a site that needs `node_modules` or the like sets
   `install = ["bun", "install", "--frozen-lockfile"]`, which runs in every fresh checkout.
7. **No remote.** `remote = "none"` makes the local branch the truth.
