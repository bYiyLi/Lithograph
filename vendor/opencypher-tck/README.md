# Vendored openCypher TCK data

This directory contains unmodified openCypher TCK feature and graph data from the upstream `2024.3` release, pinned to the commit recorded in `REVISION`.

The vendored upstream material is licensed under Apache License 2.0. `LICENSE` and `NOTICE` are copied from the upstream repository and apply to the material in this directory rather than Lithograph's first-party AGPL-licensed code.

`MANIFEST.sha256` records the exact bytes of the upstream `LICENSE`, `NOTICE`, feature data, and graph data. The repository `.gitattributes` disables text normalization for this directory so upstream line endings remain unchanged across Git checkout. `scripts/check-vendor.py` verifies both the frozen `REVISION` metadata and the manifest in the canonical CI gate.

Lithograph implements its own Rust-side fixture/TCK adapter and does not vendor the upstream JVM runner.
