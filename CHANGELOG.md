# Changelog

All notable changes to ridgewarden-library are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-library`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Fetching Kiwix ZIM files over https with resume, checked against the SHA-256 Kiwix publishes next to each file.
- Regional PMTiles maps cut by `pmtiles extract` from the newest Protomaps daily build, or taken from a file the operator supplies.
- Publishing each version as a blob through the core, described by a signed `library.item` event.
- Mirroring at every other site through the core's blob store; each copy is checked again and the old copy keeps serving until the new one has arrived and passed.
- A catalog page showing which site holds which version of what, with fetch dates and "as of" times.
- Wikipedia served through kiwix-serve, which the app can start and restart on new versions.
- Map files served over HTTP with Range requests for PMTiles readers.
- NWS alerts from api.weather.gov with If-None-Match and a required User-Agent, one fetcher per partition chosen by lowest key among live fetchers, and an alerts page where expired, replaced and cancelled alerts drop off.
- Attribution pages for Wikipedia (CC BY-SA) and OpenStreetMap (ODbL).
- Documents (forms, plans, phone lists) as a content kind, with type checks, a Documents page and stable download addresses.
- Tiers and groups: hubs hold Wikipedia without pictures, small sites hold mini.
- Topology-aware fan-out: a site waits for the library site on its best path to a copy (weakest measured link, then hops).
- Each copy is fetched from the best-connected sites that hold it first (ranked by the weakest measured link on the best path, then hops), through the core's preferred-peer list; `core.public_url` tells other sites where to reach this core.
- `mirror.confirmations`: copy a file only once that many different fetchers have published it.
- Disk limits per item, in total and as a free-space floor, counting every copy a mirror makes on the way.
- The manifest's size is checked against the library event before a blob is fetched, and only trusted manifests are fetched.
- Blobs no longer needed (replaced, superseded, refused or failed) are released at the core.
- Failed exports are retried with backoff instead of every 30 seconds; failed fetches back off up to a day.
- An operator's file is published only after it has stopped changing.
- A `pmtiles extract` run is stopped as soon as its output passes max_mb.
- kiwix-serve falls back to the last files that worked if it won't run on new ones; old files are kept until the new set has run for a minute; its log is rotated; the Wikipedia page says when it last answered.
- Served files are read back every scrub_days and copied again if damaged.
- Stable `/files/<id>.<type>` addresses that survive version swaps; Range responses capped at 4 MiB with a shared memory budget.
- Alert delivery time measured at every site against the one-minute goal; an alert drops off only after two polls leave it out; old alerts are pruned.
- NWS failover when the polling site loses its internet, not only when it goes silent.
- `library.copied` records, so sites across a data-mule corridor show what they hold.
- New versions sent to other islands as `mule.msg` for ridgewarden-mule, with trips shown on the catalog.
- Warnings on pages when a copy is older than stale_days.
- `/api/catalog` and `/api/alerts`.

[Unreleased]: https://github.com/Londopy/ridgewarden-library/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-library/releases/tag/v0.1.0
