# StoneForm

A small native launcher for WoW: Forever on Apple Silicon Macs: sign in once,
then StoneForm installs, updates and starts the game. No Battle.net app, no Rosetta.

Status: sign-in, install, updates and launch work; releases are built on
GitHub and update themselves with Sparkle. Releases are ad-hoc signed until a
Developer ID is available. The design lives in the
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
4. Update, when Blizzard serves a newer build: only what changed is
   downloaded. Offline, the installed build is launched.
5. Launch: the token goes to the game the way Battle.net hands it over, WoW
   starts signed in, and StoneForm quits.

Hold ⌥ (Option) while opening StoneForm to stop before step 5: the window
stays open with a Play button, and the StoneForm menu offers Verify and
Repair, Download Speed, Sign In Again and Sign Out.

Settings live in `~/Library/Application Support/StoneForm/settings.toml`,
cached manifests and CDN indexes in `~/Library/Caches/StoneForm`. Opened
from Finder, StoneForm logs to `~/Library/Logs/StoneForm/StoneForm.log`.

### Developer CLI

```sh
cargo run --release -p stoneform-dev -- plan                   # what a fresh install downloads
cargo run --release -p stoneform-dev -- install "<folder>"     # install or update from a terminal
cargo run --release -p stoneform-dev -- check "<folder>"       # installed build vs live build
cargo run --release -p stoneform-dev -- launch "<folder>"      # start WoW with the saved sign-in
```

`--region`, `--locale` and `--cache` work for all of them. `install
--skip-storage` installs everything but local storage, to test the rest
quickly. `--build-config HEX --cdn-config HEX --version NAME` pins an older
build (wago.tools lists them), which is how updates are tested without
waiting for Blizzard: switch a folder to an older build, then update it.

## Release

```sh
scripts/bundle.sh                     # target/release/bundle/StoneForm.app, no update feed
scripts/test-self-update.sh           # an old copy updates itself from a local feed
git tag v0.2.0 && git push origin v0.2.0   # GitHub Actions builds and publishes
```

The release workflow builds `StoneForm-<version>.zip`, signs it for Sparkle and
publishes it with `appcast.xml` as a GitHub release. Installed copies read
`releases/latest/download/appcast.xml`, download new versions in the
background and install them when StoneForm quits.

One-time setup:

1. Create the update signing key; the private half stays in your login keychain:
   ```sh
   scripts/fetch-sparkle.sh
   target/sparkle/2.10.1/bin/generate_keys --account stoneform
   ```
   Put the printed public key into `macos/sparkle-public-key` and commit it.
2. Give the release workflow the private key, then delete the file and keep a
   backup somewhere safe (a password manager): without it, installed copies
   reject every future update.
   ```sh
   target/sparkle/2.10.1/bin/generate_keys --account stoneform -x sparkle-private-key
   gh secret set SPARKLE_PRIVATE_KEY --env release < sparkle-private-key && rm sparkle-private-key
   ```
3. In the repository's Settings → Environments, give `release` yourself as a
   required reviewer, so no release job can read the key without you.

Until releases carry a Developer ID signature, macOS blocks the first open of
a downloaded copy: open it once from System Settings → Privacy & Security →
Open Anyway.

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
