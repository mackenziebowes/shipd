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
4. **Hand deploys.** shipd sets `SHIPD_RELEASE=1` for the gates, the build and the deploy. A site's own deploy script refuses to deploy to production
   without it.
5. **No remote.** `remote = "none"` makes the local branch the truth.
