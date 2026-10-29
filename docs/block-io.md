<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Native asynchronous block I/O

Native block queues separate admission, device submission, completion and
collection. Filesystem workers can prepare later work while an earlier request
is in flight, without changing the synchronous file-write or explicit-sync
contract. This facility does not introduce dirty file-page writeback.

## Ownership and placement

The kernel implementation lives in `kernel/src/block/`. `queue/` owns logical
requests, retained payloads and joined scatter reads; `service/` contains the
capability-bound synchronous and queue entry points. `window/` groups backing
ownership with its token/lease state machine. The device pump, transport request
implementation and pure admission/dispatch planner remain separate modules.
SDK queue bindings live under `stdlib/rust/hyper-os/src/block/`, while extent
records belong to `stdlib/rust/hyper-filesystem/src/protocol/`. Perfetto rendering
is grouped under `app/perf/src/trace/export/` and keeps logical block tracks
separate from scheduler tracks in both Chrome and native protobuf output.

`NativeBlock` identifies one exclusively delegated block export. A derived
`BlockQueue` is a logical producer with its own admitted request and byte limits.
Logical queues are independent of virtio request queues: one device engine
arbitrates their pending work and owns the physical queue indices and buffers.
The I/O VM continues to provide the existing virtio-scsi backend.

The queue belongs to the resource domain which creates it. Pending, executing
and completed-but-uncollected requests remain accounted. A consumer cannot
obtain unbounded storage by declining to collect completions. A device-wide
admission boundary and physical depth apply across logical queues. The initial
limits are 32 retained requests and 8 MiB per public queue, with defaults of
16 requests and 2 MiB. Each request carries at most 16 disjoint ranges totaling
512 KiB. A device retains at most 64 requests and 8 MiB across all queues.

One request and 512 KiB of that device budget are reserved for the internal
synchronous queue. Public queues therefore share at most 63 requests and
7.5 MiB, also subject to remaining total capacity. This reserve lets a
file-backed user copy refill its pages while public queues retain completions;
userspace cannot claim the internal admission class. Synchronous `NativeBlock`
calls use a persistent internal queue on the same engine, so they obey the same
overlap ordering and device flush fences.

Queue I/O rights cannot exceed the parent block capability. A derived queue also
supports duplication so a filesystem transport and its media adapter can retain
the same logical producer; this does not create another exclusive block owner.
Observing completion does not grant access to read data. A request ticket names
an operation inside that queue; it is neither a global identifier nor an
independent authority. A recycled slot cannot reinterpret an older ticket as a
new request.

The initial backend has four physical command slots and a 512 KiB data pool in
the existing dedicated shared-memory grant. Data spans are allocated in 128 KiB
units; one merged command can use up to four contiguous units. Large transfers
therefore trade command concurrency for a larger sequential request. Logical
requests can also span multiple commands. Queue limits are distinct from those
transport limits; the implementation rejects an unsupported depth instead of
silently accepting a larger value.

## Submission and data lifetime

A batch is validated in full before publication. Validation covers the selected
operation, sector alignment, geometry, range overlap, total size, available
quota and all frame lengths. Failed admission publishes no partial request.

Ordinary public write submissions take a private kernel-owned copy before
returning. Ordinary public reads retain kernel-owned result storage until
collection. The caller can reuse its original write buffer after successful
admission; no borrowed userspace pointer outlives a syscall.

Block-backed filesystem writes use a separate source-window submission. Each
such mount binds one
queue and its 512 KiB kernel-owned window. The worker can map that window only
read-only: the exported VMO itself rejects writable mappings, independently of
handle attenuation. The kernel copies application bytes into it, then publishes
a nonzero operation token. `submit_window` accepts only that queue's active token
and bounded `(first_sector, source_offset, byte_length)` descriptors. It copies
descriptors, not payload. The source offsets may be unaligned; device ranges
must still contain complete sectors.

Each accepted request retains a window lease until terminal device completion.
A filesystem reply invalidates the token; a reply with outstanding leases fails
the mount instead of allowing the next write to reuse live source bytes. Failed
transactions retire the window. Successful mounts never recycle its backing
for another mount or provider, even if an old read-only mapping remains alive.
Clean provider adoption transfers the same queue/window authority after draining
its work.

Kernel-only scatter reads retain exclusive destination slices until all submitted
work has joined, including failure and cancellation paths. These destinations
are not mapped into the filesystem worker. Both paths still copy through the
dedicated I/O VM exchange pool. Application, source-window and file-cache pages
are never granted to the I/O VM for DMA.

Requests progress through pending, executing, completed and collected states.
Completion releases physical slots independently of userspace collection.
Collection releases the retained result and logical admission slot. Admission
reserves credits before allocating and copying the payload; a copy fault rolls
back that reservation without publishing device work. A failed
read copyout leaves its completion available for another collection attempt.

## Dispatch, merging and ordering

The engine consumes any available completion and immediately refills vacant
slots. It does not wait for the slowest member of a submission batch before
making progress on another request. Submission notifications are batched after
publishing descriptors and available indices.

Only unsubmitted, adjacent sectors with compatible direction and ownership may
merge. Merging cannot cross a flush boundary or exceed the backend command
limit. Each original logical request retains its completion and accounting
identity. Already device-visible commands are immutable.

Disjoint transfers may execute concurrently. Overlapping transfers retain
submission order whenever either transfer writes: a later read cannot observe
an earlier write before that write completes, and a later write cannot overtake
an earlier read or write to the same range. The initial scheduling policy is
deliberately conservative about bypassing a blocked pending request.

A flush is a device-order fence. Earlier admitted operations must retire before
the flush command is submitted; later operations remain held until the flush
completes. Out-of-order completion of the last earlier command does not prove
that all preceding commands have finished. A successful ordinary write does
not imply stable-storage durability; explicit synchronization includes the
backend's durable-flush completion.

## Waiting and failure

Exact-ticket waits observe durable request state without consuming completion.
Waiting for any completion and generic object waits are also non-consuming.
The queue's readiness is a level condition, not a one-time interrupt edge.
Registration and state observation must close the completion-before-sleep race.
Waiting threads release queue and device locks before blocking.

`COMPLETED` means a finished result is available and not currently claimed by a
reaper. `WRITABLE` conservatively requires one slot and
`min(512 KiB, queue.byte_limit)` free payload bytes in both queue and device
budgets. A smaller request or flush can succeed while this signal is clear.
`PEER_CLOSED` reports closed admission or a failed device; previously completed
results remain collectable.

`wait_space(bytes, deadline)` waits for one slot and the caller's exact payload
size instead of the conservative `WRITABLE` threshold. Zero bytes supports a
flush; nonzero lengths must be complete sectors and fit the 512 KiB request
bound and queue byte limit. Success reserves nothing, so the caller retries
submission and handles competing producers. Completed requests retain credits;
a producer must collect its own completions before waiting if those results
prevent admission. Resource-domain quota and allocation failures remain
separate from queue-space readiness.

Admission waits prepare timer and scheduler resources before taking the device
condition lock, then recheck the predicate and publish the wait under that lock.
Credit return, queue closure and device failure update the same condition before
waking waiters. The absolute monotonic deadline uses zero for polling and
`u64::MAX` for an indefinite wait; timeout, cancellation and closure are returned
to the caller.

A wait deadline bounds the caller's wait, not the device request's ownership.
Timeout does not cancel DMA or make the hardware buffer reusable. Device-command
timeouts and malformed completion records instead fail the connection, wake
affected waiters and quarantine uncertain device-visible storage under the
existing guest-memory lease. No failed write is automatically replayed.

Closing a logical queue withdraws further admission and abandons unsubmitted
work. The engine retains already submitted operations until completion or
terminal connection failure. Device-visible memory remains leased until backend
VM/DMA retirement even when the waiting process has exited. The implementation
must not use a caller's continued execution to drive completion.

Filesystem handoff and clean unmount drain their work before returning the
exclusive block capability. Online provider adoption transfers the original
queue and window alongside the live filesystem transport; recreating a window
on that queue returns the same backing. A new mount creates a new queue/window.
A partially submitted operation is not transferred as an implicit clean mount.

## Policy boundary

Queue admission, dispatch selection and transport execution are separate
boundaries. Per-class priority scheduling and adjustable service quotas are not
implemented in this revision. The public ABI exposes bounded capacity and
depth controls, with no priority or caller-selected attribution fields.

A future priority policy must preserve authenticated request attribution when a
filesystem acts for another client. Assigning all filesystem-worker requests
the worker's own privileged priority would let ordinary applications bypass
that policy. It must also preserve per-owner charges through request merging,
enforce a device-wide budget and distinguish service-critical work from
delayable kernel work. These requirements do not imply priority control over
guest requests which bypass the Native initiator through a different I/O VM
export.

## Filesystem integration

For aligned bulk file writes, the FAT adapter translates the read-only source
window into sector ranges and coalesces adjacent disk and source offsets. It can
submit these references while continuing FAT traversal. Metadata, partial-sector
read-modify-write and other copied writes retain the existing bounded buffers.
Read-after-write hazards are ordered by the same queue. Every operation boundary
waits for all accepted work, including cleanup after a recoverable filesystem
error. Explicit sync then issues and waits for the durable device flush.

For bulk reads, FAT and uncompressed EROFS return bounded physical extent plans,
not payload. The kernel validates the complete plan against the mount-bound
volume before issuing I/O. Cold file-cache pages are private fill destinations;
the exchange pool copies directly into them before cache publication. Warm cache
reads copy cached bytes directly to the application. Partial sectors and EROFS
inline tails can require a small bounce buffer; workers which cannot describe
raw extents can use the buffered RPC fallback.

The bulk data paths have these boundary copies, excluding backend/device work:

| Operation | Data copies |
| --- | --- |
| Aligned file write | Application → kernel source window → I/O VM exchange pool |
| Page-aligned cold cached read | I/O VM exchange pool → private cache pages → application |
| Page-aligned warm cached read | Existing cache pages → application |

There is no intermediate FAT payload window, SDK payload packing buffer or
kernel read-result buffer on these bulk paths. Metadata, read-modify-write and
format transforms can still use copied fallbacks. This is not end-to-end
zero-copy I/O and does not expose Native memory to device DMA.

Sequential reads can submit multiple block commands through the same engine.
A single-threaded application issuing synchronous 128 KiB writes still cannot
merge its next syscall before that syscall exists. Async block transport does
not remove the mount's ordered filesystem RPC or promise a measured throughput
gain for every workload. See [FAT semantics](https://hyper-proj.github.io/fs-fat/docs/fat.html) and
[filesystem services](https://hyper-proj.github.io/lib-fs-service/docs/filesystem-services.html).

## Validation

Host tests exercise the production geometry and admission helpers, data-span
allocator, merge cursor, overlap rules and flush eligibility. They do not execute
kernel object lifetimes, blocking syscalls or the Linux backend. ABI tests check
wire bounds, rights metadata and completion-status conventions.

The opt-in `block-queue-probe` runs inside the exclusive FAT worker before mount.
It exercises the installed SDK and real backend: aggregate merging, 512 KiB
commands, four concurrently published command heads, exact and concurrent
waits, retained-completion quota, failed reap retry, stale tickets, rights
attenuation, flush ordering and queue abandonment. It also verifies source-window
read-only authority, denial of writable mappings, rejection of unissued tokens
without request admission, and mapping lifetime after the queue handle closes.
Its pressure case retains the full 7.5 MiB public allowance, verifies that an
internal synchronous 512 KiB read can still complete, then releases only 64 KiB
and checks exact-size admission while a 512 KiB wait still cannot proceed.
Thread start and completion can race in this probe; it does not deterministically
force every scheduler interleaving.

`make test-board-storage ARCH=aarch64` builds the probe into both FAT providers
for the disposable acceptance image and requires its durable journal result.
Production profiles do not enable the probe. `make test-boot-system ARCH=aarch64`
also covers unmount/remount, bootstrap ownership transfer and worker failures.

Filesystem acceptance separately exercises changed payloads, clean handoff,
restart and durable content validation. QEMU establishes protocol and lifecycle
behavior, not physical-device timing or cache-coherence performance.

Hardware performance comparisons retain the same 1 GiB payload, image options
and medium. Record write-plus-final-sync time, cold and warm reads, actual
device-command count, merged ranges and achieved depth. Do not present lower
submission time alone as higher durable throughput.
