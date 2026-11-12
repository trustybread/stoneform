# StoneForm

A small native launcher for WoW: Forever on Apple Silicon Macs: sign in once,
then StoneForm installs, updates and starts the game. No Battle.net app, no Rosetta.

Status: milestone 1, sign-in. The design lives in the
[StoneForm Design](https://claude.ai/artifact/RekR3B2e9W6hjJSZ6xYJJ4) doc.

## Run

```sh
cargo run -p stoneform-mac                          # opens StoneForm
RUST_LOG=info,StoneForm=debug cargo run -p stoneform-mac   # with debug logs
```

The first start opens Battle.net's sign-in window. After signing in, the token
is kept in the login keychain (service "StoneForm launch tokens") and the next
start shows "Signed in" without asking. Settings live in
`~/Library/Application Support/StoneForm/settings.toml`.

## Test

```sh
cargo test --workspace
cargo test -p stoneform-adapter-macos -- --ignored   # real Keychain round trip
```

## Layout

Hexagonal: the core crates define ports (traits), adapters implement them, and
`stoneform-mac` wires them together.

| Crate | Role |
| --- | --- |
| `stoneform-domain` | Pure types and rules: launch token, region, login URL, WoW: Forever |
| `stoneform-app` | Use cases (`Session`: sign in, sign out, current) and the driven ports |
| `stoneform-adapter-macos` | `SecretStore` on the Keychain |
| `stoneform-adapter-disk` | `SettingsStore` as a TOML file |
| `stoneform-mac` | The app: window, WebKit `LoginFlow`, composition root |

## Credits

The Battle.net sign-in and hand-off follow
[Waypoint](https://github.com/wowlocal/waypoint-launcher) (0BSD), which
StoneForm ports to Rust.
