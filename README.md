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
