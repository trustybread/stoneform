# StoneForm

A small native launcher for WoW: Forever on Apple Silicon Macs: sign in once,
then StoneForm installs, updates and starts the game. No Battle.net app, no Rosetta.

Status: sign-in, fresh install and launch work; updates (milestone 4) and a
signed release (milestone 5) are next. The design lives in the
[StoneForm Design](https://claude.ai/artifact/RekR3B2e9W6hjJSZ6xYJJ4) doc.

## Run

```sh
cargo run --release -p stoneform-mac                          # opens StoneForm
RUST_LOG=info,StoneForm=debug cargo run --release -p stoneform-mac   # with debug logs
```

Use release builds: an install checks about 50 GB of downloads.

Opening StoneForm runs the autopilot:

1. If WoW is already running, it comes to the front and StoneForm quits.
2. Sign in, once, in Battle.net's own page. The token is kept in the login
   keychain (service "StoneForm launch tokens").
3. Install, once: choose a folder and language. The download resumes after a
   pause, a quit or a crash. Battle.net must not be running meanwhile.
4. Launch: the token goes to the game the way Battle.net hands it over, WoW
   starts signed in, and StoneForm quits.

Settings live in `~/Library/Application Support/StoneForm/settings.toml`,
cached manifests and CDN indexes in `~/Library/Caches/StoneForm`.

### Developer CLI

```sh
cargo run --release -p stoneform-dev -- plan                   # what a fresh install downloads
cargo run --release -p stoneform-dev -- install "<folder>"     # install from a terminal
```

`--region`, `--locale` and `--cache` work for both; `install --skip-storage`
installs everything but local storage, to test the rest quickly.

## Test

```sh
cargo test --workspace
cargo test -p stoneform-adapter-macos -- --ignored   # real Keychain and preferences
```

## Layout

Hexagonal: the core crates define ports (traits), adapters implement them, and
`stoneform-mac` wires them together.

| Crate | Role |
| --- | --- |
| `stoneform-domain` | Pure types and rules: launch token, region, locale, autopilot steps, WoW: Forever |
| `stoneform-tact` | Blizzard's formats, sans I/O: BLTE, configs, manifests, indexes, local CASC storage |
| `stoneform-app` | Use cases (`Session`, `Installer`, `Launcher`) and the driven ports |
| `stoneform-adapter-blizzard` | Version service and CDN over HTTPS, with a disk cache |
| `stoneform-adapter-casc` | The game's local storage: blobs, journal, index files |
| `stoneform-adapter-disk` | Settings, files outside storage, `.build.info` and other records |
| `stoneform-adapter-macos` | Keychain, the `net.battle` token hand-off, processes |
| `stoneform-mac` | The app: window, WebKit `LoginFlow`, composition root |
| `stoneform-dev` | Developer CLI on the same use cases; never shipped |

## Credits

The Battle.net sign-in, hand-off and file formats follow
[Waypoint](https://github.com/wowlocal/waypoint-launcher) (0BSD), which
StoneForm ports to Rust.
