<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# lib-block

Bounded userspace partition discovery.

The GPT parser is independent of the transport and filesystem implementation. A consumer
supplies block reads and receives validated partition geometry for subsequent authority-
scoped deployment.

See the [implementation entry](src/gpt.rs) and [component declaration](component.json)
for the public interfaces, build dependencies and delivered artifacts.

## Boundaries

This library neither mounts filesystems nor submits I/O VM requests itself. Its bounds
and validation errors must be handled before constructing a partition-backed device.

## Build and test

Use this checkout inside the [repo-managed workspace](https://github.com/HypeR-Proj/HypeR).
From this repository directory:

```sh
make
make test
```

An absolute `OUT=/path/to/out` selects a shared output directory. The module
entry delegates to build/system, which prepares the installed SDK and declared
dependencies. Native binaries require HypeR; host module checks cover the tests
selected by component.json. A standalone clone does not contain the workspace’s
other source repositories or Native SDK.
