# wowupd

Installs, updates and starts World of Warcraft clients straight from Blizzard's CDN, without the
Battle.net launcher. It's like [WowUp](https://wowup.io), but for the game rather than its addons.

Written for FreeBSD/aarch64 (an Apple M5 running the Windows **ARM64** client under native Wine). It
should work on any Unix with Rust, for either client architecture.

Supported products:

| Code              | Folder           | Game                     |
|-------------------|------------------|--------------------------|
| `wowf`            | `_forever_`      | WoW: Forever             |
| `wow_classic`     | `_classic_`      | WoW Classic              |
| `wow_classic_era` | `_classic_era_`  | WoW Classic Era          |
| `wow_anniversary` | `_anniversary_`  | WoW Classic Anniversary  |
| `wow`             | `_retail_`       | World of Warcraft        |

## Why it works

The game client logs in by itself, including the Battle.net Authenticator prompt. It doesn't need the
launcher, only a current install. If the install is out of date, the client says *no realms available*.

When Blizzard's Agent is installed, the client starts it. `wowupd play` stops it from starting
(`WINEDLLOVERRIDES=agent.exe=d`).

## Installing

```sh
./install.sh              # packages (gtk4, rust), build, install
./install.sh --no-system  # build and install only
```

Run it as your desktop user from the checkout; it asks doas or sudo only for missing packages. It
installs `wowupd` and `wowupd-gtk` in `~/.local/bin` and a desktop entry in
`~/.local/share/applications`, so application launchers list **wowupd**. Run it again after pulling
to update.

Building by hand: `cargo build --release` (needs gtk4; `--no-default-features` builds only the
command).

## The window

`wowupd-gtk` lists each game with its installed and live versions and a **Play**, **Update** or
**Install** button. While it works, a progress bar shows how much is done, the download speed and
the time left. The folder button picks the World of Warcraft folder and saves it to `wowupd.conf`.

## Settings

`~/.config/wowupd/wowupd.conf` (every setting is optional; command-line options override them):

```
dir = ~/Games/wow/drive_c/World of Warcraft   # the folder holding Data/ and _classic_/
wine = ~/wine/bin/wine                         # default: wine from PATH
wineprefix = ~/Games/wow
arch = arm64                                   # default: this machine's architecture
locale = enUS
region = us
env = DXVK_LOG_LEVEL=none                      # may repeat
```

## Use

```sh
wowupd status                     # installed and live versions of every product
wowupd update wowf wow_classic    # install or update
wowupd update wowf --check        # only report what would be downloaded
wowupd update wowf --verify       # check every file even if the version is current; repair
wowupd play wow_classic           # update if needed, then start the game
wowupd play wowf --offline        # start without checking
```

A product whose version and architecture in `.build.info` already match the live build is taken as
current in about a second. `--verify` checks every file, which takes a couple of minutes for a full
install.

An interrupted download resumes where it stopped: run the same command again.

## Taking over a Battle.net install

wowupd keeps the game data (`Data/data`) in a store it builds itself. The client won't accept a
Battle.net store with wowupd's files added to it: it reports *your installation is damaged*. It does
accept a store built whole.

So the first `update` in a folder Battle.net installed (or one copied from a Windows PC):

- builds a new store for every installed product, copying what the old store has and downloading the rest;
- renames the old store to `Data/data.battlenet`;
- writes `.wowupd-store` in the WoW folder.

Delete `data.battlenet` once the games work. To hand the folder back to Battle.net, move it back to
`Data/data`, remove `.wowupd-store`, and run Battle.net's Scan and Repair.

This also covers switching an x86_64 install to `arm64`: the new store gets the ARM64 files.

## How it works

- **Version service.** `https://us.version.battle.net/v2/products/<code>/{versions,cdns}` gives the
  live build config, CDN config and CDN hosts.
- **Manifests.** The build config names the encoding table, the install manifest (loose files such as
  `Wow-ARM64.exe`) and the download manifest (game data). Both manifests are filtered by tags:
  `Windows`, the architecture, the locale, the region, `speech` and `text`. Opt-in content
  (`HighRes`, `Alternate`, ...) is skipped.
- **Finding and fetching.** Missing blobs are looked up in the CDN archive indices, which are cached in
  `Data/indices`. They are fetched with merged HTTP range requests, several at a time. Each blob is
  checked against its MD5 encoding key.
- **Writing the store.** Blobs are appended to `Data/data/data.NNN` segments, each behind the
  30-byte header the client expects, with the Agent's two checksums. A journal makes interrupted runs
  resumable.
- **Indices.** All sixteen `.idx` files are then rewritten as new versions. Entries the client wrote
  itself are kept: they have no header and are bare BLTE blobs. `shmem` is removed, and the client
  rebuilds it.
- **Finishing.** Loose files are written to the product folder, matching existing names without regard
  to case. `.build.info` gets the product's row, and other products' rows are kept.

## Limitations

- Old data is never removed, so the store grows with each patch.
- Downloads use plain HTTP, like the launcher; every blob is verified against its key.

## References

Format details come from [wowdev.wiki](https://wowdev.wiki/TACT) ([CASC](https://wowdev.wiki/CASC)),
[CascLib](https://github.com/ladislav-zezula/CascLib), and files written by Blizzard's Agent.
[WoWForeverywhere](https://github.com/YoruDev-Ryland/WoWForeverywhere), which runs the ARM64 client on
Android, showed that this approach works. wowupd shares no code with it.

## License

BSD 2-Clause; see [LICENSE](LICENSE).
