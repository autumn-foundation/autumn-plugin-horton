# Verification

These Verus specs are mathematical models of the plugin's pure core. Each
exec fn is equivalent to the runtime fn of the same name in `src/`. Unit
tests and property tests check the runtime fns against the same rules.

| Spec | Runtime | What it proves |
|---|---|---|
| `ttl.rs` | `src/ttl.rs` | A TTL never gives expiry 0. A value lives at least its TTL. The clock never goes back. A purge never removes a value a later read can see, also after a restart |
| `keys.rs` | `src/keys.rs` | App keys and plugin keys are disjoint. Plugin keys sort below app keys and below the scan floor. `check_app_key` and `app_floor` give the modelled results |
| `group.rs` | `src/group.rs` | The group-commit check is exact and does not overflow |

Run all specs:

```sh
scripts/verify-verus.sh                   # uses `verus` on PATH
VERUS_BIN=/path/to/verus scripts/verify-verus.sh
```

CI runs the same script in the `verus` job of `.github/workflows/ci.yml`, with one pinned Verus release.
