# Retrovert channels

Everything on the publishing side of Retrovert's update channels: the trust
anchor of each channel, the [`retrovert-publish`](crates/retrovert-publish) CLI
that signs them, and the scheduled job that keeps their metadata valid.

Nothing here ships to users. Clients depend on
[`retrovert-updater`](https://github.com/RetrovertApp/retrovert-updater) — the
download-and-verify side — and this repository takes `retrovert-tuf` from it as
a pinned dependency so signer and verifier agree on the metadata format. The
channels themselves live somewhere else again, on releases in the repository
each one names below.

Keeping the anchors out of the channel host is the point rather than tidiness: a
channel's trust anchor must not come from the repository being signed, or
whoever can write to the channel host can also swap the anchor its metadata is
checked against.

A channel is a self-contained TUF repository hosted on GitHub releases. Two
kinds of release make it up:

- `<channel>/channel-metadata` — one rolling release whose flat asset namespace
  *is* the channel's base URL. It carries the TUF metadata and the release-set
  manifest each generation is named by. `timestamp.json` is replaced on every
  publish and is the commit point; everything else is named by version or
  digest and never changes.
- `<channel>/vN` — one release per generation, carrying that release set's
  immutable assets: the manifest and, once the gather workflow lands, the
  plugin artifacts it lists.

## `dev`

Disposable test root, hosted in `RetrovertApp/playback_plugins`.

| | |
| --- | --- |
| Base URL | `https://github.com/RetrovertApp/playback_plugins/releases/download/dev/channel-metadata/` |
| Root | [`dev/root.json`](dev/root.json) |
| Root key id | `b56c9549951284ca51285a1ea866510dfa1d3251a8e4847c8a0118cb95cf8081` |

`dev/root.json` is public key material and the only thing a client needs to
trust beyond the base URL. Its private keys are disposable and are *not* the
production root: `stable` is born under the real root at Gate P and never
migrates roots.

Verify what the channel is serving right now:

```console
$ retrovert-publish verify \
    https://github.com/RetrovertApp/playback_plugins/releases/download/dev/channel-metadata/ \
    dev/root.json
```

## Publishing

The signing keys live outside this repository, in the publisher's workspace
(`keys/` beside the `repository/` tree that gets uploaded). A publish that fails
part-way is recovered by publishing again, never by deleting what landed.

**Pull before you publish.** The workspace decides the version each role is
written at, and since the re-sign job below signs the same channel it is no
longer the only writer of that sequence:

```console
$ retrovert-publish pull    <workspace> --repo RetrovertApp/playback_plugins --channel dev
$ retrovert-publish publish <workspace> <manifest> \
    --repo RetrovertApp/playback_plugins --channel dev
```

Skipping the pull publishes a *different* `N.targets.json` and `N.snapshot.json`
over names the channel already serves at those versions — breaking the promise
above that a versioned name never changes, and stranding clients that already
trust the re-signed set until the version after next. The pull costs one round
trip and makes the workspace current; nothing else reconciles the two writers.

`GH_TOKEN` or `GITHUB_TOKEN` supplies the credential. `--stop-after N` ends a
run short of its commit point, which is how the failed-publish drill is run
against the real channel.

## Keeping metadata valid

A channel expires on its own clock — timestamp 14 days after each signature,
snapshot and targets 60 — so one that publishes nothing eventually stops
verifying, and clients see a dead channel rather than a quiet one. Re-signing
advances every online role's version and expiry while leaving the generation the
channel names byte-for-byte alone.

[`resign-dev.yml`](.github/workflows/resign-dev.yml) does this twice weekly
without anyone present. It assembles a workspace from the online keys, takes the
chain the channel currently serves, re-signs it, publishes, and reads the result
back as a client:

```console
$ retrovert-publish pull <workspace> --repo RetrovertApp/playback_plugins --channel dev
$ retrovert-publish resign <workspace> --repo RetrovertApp/playback_plugins --channel dev
```

`pull` replaces the workspace's channel metadata with what the live channel
serves, refusing to write anything the workspace's own `root.json` does not
authenticate — the bytes it writes are the bytes `resign` then signs over, so
taking the host's word for them would let a host put its choices into metadata
every client accepts. The root is an input to that check and is never replaced
by it.

Two operational consequences follow:

- **Nothing recovers a lapsed channel unattended.** Verification includes
  expiry, so once the timestamp is past its 14 days `pull` refuses and the job
  can no longer help. Twice weekly leaves three missed runs of slack; past that,
  re-signing needs the publisher's own workspace. Watching for this is the
  external liveness alert the map still lists as unresolved — GitHub also
  disables scheduled workflows after 60 days of repository inactivity.
- **A re-sign publishes metadata only.** The generation's assets stay on the
  `<channel>/vN` release they were published to and are never re-uploaded.

### Which signer runs

The job builds `retrovert-publish` from this repository at the revision it is
running from, with `--locked`. The binary handed the channel's private keys is
therefore the one reviewed alongside the anchor it uses, down to the dependency
versions in `Cargo.lock`. Adopting a metadata format change means bumping the
`retrovert-tuf` revision in `Cargo.toml` — a deliberate, reviewed commit.

### What the protected environment holds

The workflow runs in the `channel-signing` environment, which is where every
credential it needs lives and the only place in this repository that can reach
them:

| Secret | What it is |
| --- | --- |
| `CHANNEL_TARGETS_KEY` | `targets` private key, PKCS#8 PEM |
| `CHANNEL_SNAPSHOT_KEY` | `snapshot` private key, PKCS#8 PEM |
| `CHANNEL_TIMESTAMP_KEY` | `timestamp` private key, PKCS#8 PEM |
| `CHANNEL_TOKEN` | a credential with write access to the channel's host repository |

These are the three keys `init` wrote to the workspace's `keys/online/`. The
root key, in `keys/offline/`, is not among them and never goes to CI: a re-sign
touches only the online roles. An absent secret arrives as an empty variable
rather than an error, so the assemble step checks each one and fails there —
before the channel is contacted — rather than at the signing step.
