# ridgewarden-library

[![CI](https://github.com/Londopy/ridgewarden-library/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-library/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-library)](https://github.com/Londopy/ridgewarden-library/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-library/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-library)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Wikipedia, regional maps, documents and NWS alerts at every site of the mesh, still there when the internet is down.**

A ridgewarden app for AREDN meshes. One site with internet (a fetcher) downloads the content its operator lists, checks it against the publisher's own hash, and publishes it through its ridgewarden core. Every other site running this app copies it over the mesh through the core's blob store, checks it again, and only then swaps it in for the copy it had. Weather alerts travel as small signed events, so they reach every site within a sync round.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box. Spec sheet 04 of the AREDN app suite.

## Features

- One catalog page: every item, its newest version and when it was fetched, which site holds which version, what each site is still copying and why it is waiting, each site's free disk, and the time each site last reported in.
- The old copy keeps serving until the new one has fully arrived and passed its checks. If kiwix-serve won't start on a new ZIM file, it goes back to the files that worked, and those are kept until a new set has stayed up for a minute.
- Wikipedia (and any other Kiwix ZIM file) served by kiwix-serve, which this app can start, restart and check on.
- PMTiles map files served over HTTP with Range requests, so any PMTiles reader fetches only the tiles it shows. Each map has an address that always serves its newest version.
- Documents (forms, plans, phone lists) in PDF, text, CSV, Markdown, office and image formats, copied like everything else and listed on their own page.
- Tiers: small sites hold Wikipedia mini, hubs hold Wikipedia without pictures, from one config line per item.
- Topology-aware fan-out along the best-connected tree: a site waits while a library site on its best path to a copy is still fetching, then has its core fetch from the best-connected sites that hold the version first, so a version usually crosses each radio hop once.
- Optional independent confirmation: copy a file only once two (or more) fetchers have published the same bytes.
- Every served file is read back once a month (configurable); one damaged on disk stops being served and is copied again from the blob store.
- NWS alerts from api.weather.gov, deduplicated by alert id, fetched by one site per network partition, with another site taking over when it goes quiet or loses its internet. Expired, replaced and cancelled alerts drop off; one empty reply doesn't clear them.
- Alert delivery time measured at every site against the one-minute goal.
- New versions handed to the data-mule app (ridgewarden-mule) for other islands, with each trip shown on the catalog.
- Disk limits: per item, in total, and a free-space floor checked before each copy.
- Attribution pages for Wikipedia (CC BY-SA) and OpenStreetMap (ODbL).
- `/api/catalog` and `/api/alerts` return the same data as JSON.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-library/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-library/releases/latest): `sudo apt install ./ridgewarden-library_<version>_arm64.deb` (`_amd64` on a PC). Either way the download is checked against the release key your core already trusts, `/etc/ridgewarden/library.toml` is written if missing, and the service starts once the core is set up. The pages are on port 8795.

The app runs as the core's user and writes into the core's import folder (`/var/lib/ridgewarden/imports`) and takes files from its export folder (`/var/lib/ridgewarden/exports`). If your core's `data_dir` is somewhere else, change `core.imports_dir` and the `ReadWritePaths` line of the systemd unit.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-library`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-library ridgewarden-library --config /etc/ridgewarden/library.toml <command>`.

## Usage

```sh
ridgewarden-library init --config /etc/ridgewarden/library.toml
ridgewarden-library run  --config /etc/ridgewarden/library.toml
```

The pages are on `http://<pi>:8795/`: the catalog, Wikipedia, Maps, Documents, Alerts and About. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

A site with no `[[item]]` entries is a mirror: it copies whatever trusted stations publish (subject to `[mirror]`), and needs nothing else. Mirroring starts on its own; the core's `[blobs]` settings decide how fast and in which hours bulk files move, so they never crowd out other traffic.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`. Add each fetcher's key there at every site.

## Configuration

`/etc/ridgewarden/library.toml` (`init` writes a commented one). Nothing is ever downloaded unless it is listed as an `[[item]]`, and nothing bigger than that item's `max_mb`.

### Wikipedia on a fetcher

```toml
[[item]]
id = "wikipedia-en-mini"            # stable across versions
kind = "zim"
title = "Wikipedia (English, mini)"
# download.kiwix.org redirects this short name to the newest dated file
url = "https://download.kiwix.org/zim/wikipedia_en_all_mini.zim"
# Kiwix publishes "<sha256>  <file name>" next to every ZIM; {name} is the dated file name
sha256_url = "https://download.kiwix.org/zim/wikipedia/{name}.sha256"
max_mb = 15000
check_days = 30
group = "wikipedia-en"

[[item]]
id = "wikipedia-en-nopic"
kind = "zim"
title = "Wikipedia (English, no pictures)"
url = "https://download.kiwix.org/zim/wikipedia_en_all_nopic.zim"
sha256_url = "https://download.kiwix.org/zim/wikipedia/{name}.sha256"
max_mb = 55000
tiers = ["hub"]                     # only sites with mirror.tier = "hub" copy it
group = "wikipedia-en"              # and they hold it instead of mini
```

Try it first with the one-article test file in the example config (`wikipedia_en_ray-charles_mini.zim`, under 1 MB). English Wikipedia mini is about 13 GB, without pictures about 49 GB. Downloads go over https with connect and stall timeouts, from wherever the redirects end, and resume with a Range request if they are interrupted. A failed fetch is tried again after 15 minutes, then 30, and so on up to once a day.

The version shown is the date in Kiwix's file name (`..._2026-08.zim` is "2026-08"), or the file's Last-Modified date.

### A regional map on a fetcher

Protomaps' [`pmtiles` command-line tool](https://github.com/protomaps/go-pmtiles) cuts a region out of their daily planet build, reading only the parts it needs. Install it on the fetcher and let the app run it:

```toml
[[item]]
id = "map-region"
kind = "pmtiles"
title = "Regional map"
# {build} is the newest daily build from build-metadata.protomaps.dev/builds.json,
# {out} is where the app wants the file
command = ["pmtiles", "extract", "{build}", "{out}", "--bbox=-121.35,34.89,-119.47,35.80", "--maxzoom=15"]
max_mb = 2000
check_days = 30
timeout_min = 240
```

The command is stopped as soon as its output grows past `max_mb`, or when it runs longer than `timeout_min`. Or make the file yourself (any way you like) and point the app at it:

```toml
[[item]]
id = "map-region"
kind = "pmtiles"
title = "Regional map"
file = "/var/lib/ridgewarden-library/region.pmtiles"
max_mb = 2000
```

An operator's file is published once it has looked the same for two rounds in a row and hasn't been touched for two minutes, so a file still being copied in is never sent out half-written. It is published again whenever it changes.

Protomaps publishes BLAKE3 hashes for its whole planet builds only; a regional extract is a new file, so there is no publisher hash for it. Copies are checked against the hash the fetcher signs (see below).

### Documents

```toml
[[item]]
id = "shelter-phone-list"
kind = "doc"
title = "Shelter phone list"
file = "/var/lib/ridgewarden-library/docs/shelter-phones.csv"   # or url = "https://..."
max_mb = 5
```

A document keeps its file type, which must be one of pdf, txt, csv, md, odt, ods, docx, xlsx, png or jpg; HTML is refused, since a page served from here could run script. Each file is checked for the type it claims. Documents are served from `/files/<id>.<type>` with `X-Content-Type-Options: nosniff` and a sandboxing Content-Security-Policy. Anyone on the mesh can read them; don't publish anything private.

### What sites copy

```toml
[mirror]
enabled = true
items = []            # item ids to copy here; empty means everything trusted stations publish
max_mb = 16000        # never copy one item bigger than this
total_mb = 40000      # everything served and being copied here stays under this
keep_free_mb = 2000   # and never start a copy that would leave less free disk than this
tier = ""             # e.g. "hub"; items with tiers = [...] go only to those sites
fanout = true         # wait for the library site on the best path to a copy
fanout_wait_min = 180 # ...but no longer than this after the version was published
confirmations = 1     # different fetchers that must have published the same file
```

Disk: a copy lands in the core's blob store, is exported next to it, and is then moved to the served folder. With the core and this app on one file system that is two copies' worth; under the packaged systemd unit each folder is its own mount, so the move is a copy and the peak is three. The free-space check (from `df`) counts that, and counts the copies other items still have on their way. Once a version is replaced or no longer wanted, the app tells the core to let its blob go, so its chunks are freed.

### Copying from the nearest site

Each copy is fetched from the best-connected sites that already hold it, with the fetcher usually last: the app hands its core that list (`blob_want_from`), and the core asks those peers first and others only for chunks they don't have. As more sites finish their copies, the list is updated.

"Best-connected" is worked out from the links every core polls. Each link is weighted by its latest sample: the worse of its two directions' link quality, or its SNR scaled so 30 dB or more counts as perfect, and a link with nothing measured counts as weak (10%), never strong. A holder's path is as good as its weakest link, because a transfer relayed over several hops runs no faster than the slowest of them and one bad link loses chunks for the whole path; adding up per-link costs instead would let a short path through one terrible link beat a longer path of solid ones. Holders are ranked by that weakest link, then by fewer hops. The same paths decide whom a site waits for: a library site still copying the version that sits on its best path to a copy, the nearest of them if there are several. So the copy comes down the best-connected tree, not merely the shortest one.

Only cores that this site's core already syncs with are named, so a station's heartbeat can reorder the peers the operator set up but never send the core anywhere new. A site is matched to one of those peers by the `core.public_url` it sends in its heartbeat (set it to the address other cores have in their `[sync] peers` for it, e.g. `http://tst-cuesta-pi.local.mesh:8790`), or else by callsign, which the core only learns for peers it discovered from the node's service list. A site that can't be matched is left out of the list, and the core falls back to its usual peers.

The wait for a closer site gives up after `fanout_wait_min`. English Wikipedia without pictures takes about 6 hours per hop at 20 Mbps, so for big content raise it, or a far site starts while the middle one is still filling and that first hop carries the chunks twice.

How fast each link carries bulk content is the core's business: `[blobs] rate_kbps` caps everything, and a slow link gets a lower cap of its own in the core's `ridgewarden.toml`:

```toml
[blobs]
rate_kbps = 2000
window = "1-6"                 # UTC hours for bulk transfers, if you want them off-peak
[blobs.peer_rate_kbps]
"http://10.1.2.3:8790" = 500   # the peer behind the weak link
```

Before a copy starts the app also checks that the core has the trusted fetcher's manifest and that its size matches the `library.item` event. An item that doesn't fit, is refused, or is waiting shows the reason on the catalog page.

### NWS alerts

```toml
[nws]
enabled = true
area = "CA"                                  # or zone = "CAZ034", or point = "35.28,-120.66"
user_agent = "(county mesh library, ops@example.org)"
poll_s = 60
quiet_s = 900
```

api.weather.gov requires a User-Agent that says who you are and how to reach you, so there is no default: the app refuses to start with alerts enabled and no `user_agent`. It sends If-None-Match with the last ETag, but at the time of writing the API answers with the whole list anyway (about 130 KB for a state at 60 s, around 180 MB a day), so a zone or point keeps the replies small.

Turn `[nws]` on at every site with internet. Only one of them polls at a time: among sites polling the same place that are live, the one whose core has the lowest public key does it. Live means its heartbeat (every 5 minutes, and at once when its NWS state changes) arrived within `quiet_s`, and its last good poll was within `quiet_s` or it hasn't tried yet. A fetcher that is powered off, cut off by a split, or has lost its internet stops counting once `quiet_s` passes, and the next lowest takes over. Each side of a split mesh ends up with its own fetcher. Two sites briefly both polling is harmless: alerts are deduplicated by their NWS id.

Each new alert is logged once as a `library.alert` event on the main log. The GeoJSON response carries the alert's CAP fields (event, severity, urgency, sent, expires, area, description, instruction, references), so the app doesn't fetch the CAP XML separately. The alerts page shows what is in force: not past `expires`, not replaced or cancelled by a later message, and still on the NWS list the last two times a live fetcher looked. It always says when alerts were last fetched and by whom, and warns when nobody has fetched for longer than `quiet_s`. Alerts that expired a month ago are pruned.

Each site measures how long alerts take to reach it (from the fetcher logging one to this site's app having it) and reports the slowest of the last week; the alerts page and the catalog show it against the one-minute goal.

### Wikipedia through kiwix-serve

Reading ZIM files needs zstd and xz decompression, which this app doesn't carry, so Wikipedia is served by [kiwix-serve](https://kiwix.org/) (in Debian and Raspberry Pi OS as `kiwix-tools`). Either let the app run it on the current files, or run it yourself and give its address:

```toml
[kiwix]
command = ["kiwix-serve", "--port=8796", "{zims}"]   # {zims} becomes the current ZIM files
url = "http://tst-cuesta-pi.local.mesh:8796/"         # linked from the Wikipedia page, and checked every 5 minutes
```

When a new version takes over, the app restarts kiwix-serve on it. If kiwix-serve exits within a minute, the app starts it again on the last set of files that worked and tries the new set again after 10 minutes; the old files are not deleted until a new set has run for a minute. Don't add `--skipInvalid`: it keeps kiwix-serve up with the bad file left out, which hides the failure from this fallback. Its output goes to `kiwix-serve.log` in the data directory (rotated at 10 MB).

kiwix-serve runs as the app's own user. To run it as a separate user instead, give it its own systemd unit pointed at `/var/lib/ridgewarden-library/content/`, leave `command` empty and set `url`; you then restart it yourself after an update.

### Maps

Map files are at `http://<pi>:8795/files/<id>.pmtiles` (always the newest version) and under their versioned names; the Maps page lists both. A Range request gets at most 4 MiB per response; files up to 32 MiB can be fetched whole, and whole downloads of bigger files are refused to keep big transfers off the radio links. All responses together hold at most 48 MiB of file data in memory; a request that can't get its share within 20 seconds gets a 503. Any PMTiles reader that works over HTTP reads them, for example `pmtiles show http://<pi>:8795/files/<id>.pmtiles`.

### Carrying updates to other islands

With `[mule]` set on a fetcher, every new version is also sent as a `mule.msg` (bulk class, with the version's blob as its file) to each island in `to`, for [ridgewarden-mule](https://github.com/Londopy/ridgewarden-mule) cars to carry:

```toml
[mule]
island = "north"      # this site's island, as the mule app names it
to = ["south"]
```

The mule app plans each update like any bulk message and loads its file onto a car for that trip; don't add `"library/"` to a car core's `[blobs] auto_want` (it would fetch every version for every island, or fetch back what the car just let go). At the far island the `library.item` event and the blob arrive with the car, the library there copies it like any other version, and the mule app receipts it without keeping a second copy. The far library must trust the fetcher's core, the fetcher's core must be pinned to its island under the mule app's `[[islands]]` at the cars and the far sites, and big content needs a trip with that much room: English Wikipedia without pictures is about 49 GB. The catalog shows each trip's last record (picked up, handed over, delivered), and sites known only from `library.copied` records carried back by a car are listed as "(record)".

## What "verified" means

Every chunk of every blob is checked by the core against its BLAKE3 hash, and the blob's id covers the whole chunk list, so the bytes a mirror ends up with are exactly the bytes the fetcher published.

On top of that, the fetcher checks each download against the publisher's own SHA-256 where the publisher has one (Kiwix does, for every ZIM file) and puts that hash, and where it came from, in its signed `library.item` event. Each mirror checks its finished copy against the hash in that event before serving it; a copy that fails is thrown away and the old one keeps serving.

A site with no internet cannot ask Kiwix itself. Its "checked against the publisher's SHA-256" means: against the hash the fetcher says Kiwix published, carried in an event the fetcher signed. That proves the copy wasn't damaged or altered on the way across the mesh, and ties it to a named, trusted station, but a fetcher that lied about the hash would not be caught by one mirror on its own. Setting `mirror.confirmations = 2` with two fetchers closes that gap: a copy is made only once two different trusted stations have each fetched the same file from the internet and signed the same hash. Anyone with internet can also compare the hash on the catalog page with the publisher's file. Maps cut by `pmtiles extract`, and documents, have no publisher hash at all; the catalog says so.

## Limits

These parts of spec sheet 04 are not done, or are done differently:

- Per-link rates set from each link's measured quality: the rate per link is the operator's `[blobs.peer_rate_kbps]` in the core; nothing sets it from link quality on its own.
- A chunk a nearby site lacks can still come from a farther peer: the app asks its core for the nearest holders first rather than only, so a copy never stalls on one site being down.
- Kiwix's per-piece hashes as the chunk manifest: the core's chunks are 1 MiB BLAKE3 pieces and Kiwix's are 4 MiB SHA-1 pieces, so they can't be one manifest; the whole-file SHA-256 is checked at both ends instead.
- CAP XML per alert: not fetched, since the GeoJSON already carries the CAP fields.
- A map viewer in the browser: it would need MapLibre GL, the pmtiles plugin, a style, fonts and sprites served locally; none is included.
- Which sites can host an SSD, and what other content served agencies want, are questions for the people running the mesh, not software.

Tested with small local files, a local HTTP server standing in for Kiwix and api.weather.gov, and two cores on the simulator; nothing has been downloaded from the real services by the tests.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-library
cd ridgewarden-library
cargo build --release
cargo test
```

The tests use small local files and a local HTTP server; nothing is downloaded from the internet. `nws.api_url` points the NWS poller at a local stand-in for testing.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
