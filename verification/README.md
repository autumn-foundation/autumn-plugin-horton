# Verification

These Verus specs are mathematical models of the plugin's pure core. Each
exec fn has the same body as the runtime fn of the same name in `src/`.
Unit tests and property tests check that the runtime fns agree.

| Spec | Runtime | What it proves |
|---|---|---|
| `ttl.rs` | `src/ttl.rs` | A TTL never gives expiry 0. The store clock never goes back. Purged entries stay invisible |
| `keys.rs` | `src/keys.rs` | App keys and plugin keys are disjoint. Plugin keys sort below the app scan floor |
| `group.rs` | `src/group.rs` | The group-commit check is exact and does not overflow |

Run all specs:

```sh
scripts/verify-verus.sh                   # uses `verus` on PATH
VERUS_BIN=/path/to/verus scripts/verify-verus.sh
```

CI runs the same script (`.github/workflows/verus.yml`).
