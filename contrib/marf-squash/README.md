# marf-squash

Offline CLI for producing Genesis State Snapshots (GSS) from a Stacks node's
chainstate. Squashes the three MARFs (Clarity, Index, Sortition), copies
canonical block data and Bitcoin auxiliary files, and generates a self-describing
manifest with SHA-256 checksums for the fixed artifacts plus one aggregate hash
for the epoch-2 block archive.

This crate *produces* a GSS; it does not verify one. Offline verification of a
GSS against a trusted checkpoint is a separate tool, not provided here.

## Build

From the repository root:

```bash
cargo build -p marf-squash --release
```

## Usage

```bash
marf-squash squash \
  --chainstate /data/mainnet \
  --out-dir /tmp/gss \
  --tenure-start-bitcoin-height 880000 \
  --all
```

`--all` squashes all three MARFs, copies canonical block data, copies Bitcoin
auxiliary files, and generates a `GSS_manifest.toml` with SHA-256 checksums for
the fixed artifacts plus one aggregate hash for the epoch-2 block archive under
`chainstate/blocks/`.

Individual MARFs can be squashed selectively with `--clarity`, `--index`, or
`--sortition`. `--blocks` requires `--index` (or `--all`); `--bitcoin` requires
`--sortition` (or `--all`). A node config can be supplied with `--config`.

## GSS output layout

A full GSS (`--all`) mirrors the node's working directory structure:

```
/tmp/gss/
├── chainstate/
│   ├── vm/
│   │   ├── clarity/
│   │   │   ├── marf.sqlite
│   │   │   └── marf.sqlite.blobs
│   │   ├── index.sqlite
│   │   └── index.sqlite.blobs
│   └── blocks/
│       ├── nakamoto.sqlite
│       └── {XX}/{YY}/{hash}... # Epoch 2.x blocks
├── burnchain/
│   ├── sortition/
│   │   ├── marf.sqlite
│   │   └── marf.sqlite.blobs
│   └── burnchain.sqlite
├── headers.sqlite
└── GSS_manifest.toml
```

## The GSS manifest

`GSS_manifest.toml` is a self-describing record of the snapshot: the three MARFs'
squash root node hashes and archival MARF root hashes, the block range, and
SHA-256 checksums (file-level for the fixed artifacts, one aggregate hash for the
epoch-2 block archive). It is written by `squash` for a full GSS (`--all`).

Nothing in this crate reads it back — it is the artifact format consumed by an
external/offline verifier. The squash root node hashes are the intended trust
anchor: a verifier authenticates them against an independently published
checkpoint. The manifest itself is part of the untrusted artifact and is not
authenticated.

## Using a GSS to bootstrap a node

1. Produce or download a GSS directory.
2. (Recommended) Verify it against a trusted checkpoint with the offline verifier
   — a separate tool, not provided by this crate.
3. Set `[node].working_dir` in your Stacks config to the **parent** of the GSS
   directory (e.g. `/data/my-node`).
4. Start the node normally.

The node is unaware it is running from a GSS.
