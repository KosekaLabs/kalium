# Kalium architecture

Kalium is a Rust kernel whose only executable format is WebAssembly. This document describes its design: what is in the kernel, what is deliberately kept out, how WebAssembly instances are isolated and scheduled, how capabilities gate everything, and how the parts of QualiaOS that matter to the kernel (the runtime shelf, the system agent, the NPU) are supported.

The system-level picture, including the services and apps that run on top of Kalium, is in the [QualiaOS documentation](https://github.com/KosekaLabs/qualia-os).

## Goals

1. **Pure.** The kernel contains what needs privileged access to hardware or to other instances, and nothing else. Anything that can be a WebAssembly service is a WebAssembly service.
2. **One format.** The kernel loads WebAssembly and only WebAssembly. No ELF, no native shared objects, no JIT for anything but Wasm.
3. **Capabilities everywhere.** No ambient authority. An instance can do exactly what the capabilities it holds allow, and the set of host functions it can import is derived from those capabilities.
4. **Real-time where it matters.** The system agent runs on a fixed inference cadence and the audio path has hard deadlines. The scheduler is built so that app load cannot break those.
5. **Rust, `no_std`, `aarch64` first.** The kernel is written in Rust with `unsafe` confined to the hardware abstraction layer and the Wasm execution engine's codegen boundary.

## Non-goals

- POSIX compatibility. There is no `fork`, no file descriptors, no signals. Services that want POSIX-like behaviour build it over the host ABI.
- Running on desktops or servers. The target is mobile SoCs; the design does not avoid choices that would be odd elsewhere.
- Being a general WebAssembly runtime library. The execution engine is shaped for this kernel.

## Overview

```text
┌─────────────────────────────────────────────────────────────────────────┐
│ WebAssembly instances                                                   │
│   system services (privileged caps) · runtime shelf · apps              │
├─────────────────────────────────────────────────────────────────────────┤
│ Host ABI: capability-gated imports (kalium:* namespaces)                │
├───────────────┬──────────────┬──────────────┬──────────────┬────────────┤
│ Wasm runtime  │ Capability   │ Scheduler    │ IPC          │ Devices    │
│ load/validate │ table, mint, │ RT + fair    │ channels,    │ display,   │
│ AOT/interp    │ revoke, pass │ classes,     │ shared mem,  │ input,     │
│ traps, fuel   │              │ deadlines    │ cap transfer │ audio, NPU │
├───────────────┴──────────────┴──────────────┴──────────────┴────────────┤
│ Memory manager (physical pages, MMU, guard regions, CoW)                │
├─────────────────────────────────────────────────────────────────────────┤
│ HAL: boot, exceptions, timers, MMU tables, cache, SMP (aarch64)         │
└─────────────────────────────────────────────────────────────────────────┘
```

## Instances

The unit of execution and isolation is an **instance**: one WebAssembly module (or component) instantiated with a linear memory, a table, a set of imports resolved from the capabilities it holds, and one or more threads.

An instance has:

| Field            | Meaning                                                                              |
| ---------------- | ------------------------------------------------------------------------------------ |
| Module           | The validated, compiled code. Shared read-only between instances of the same module. |
| Linear memory    | Its own, backed by physical pages mapped through the MMU.                            |
| Capability table | The handles it holds. Determines which host functions resolve.                       |
| Threads          | One or more execution contexts, each with a Wasm stack and a kernel stack.           |
| Scheduling class | Real-time or fair, with a budget.                                                    |
| Parent           | The instance that created it and may terminate it.                                   |

Instances are created by other instances that hold the `spawn` capability, starting from a root instance (the init service) that the kernel creates at boot from the boot image.

### Modules are shared, instances are not

A module is loaded and compiled once. Its code pages are mapped read-only and execute-only into every instance of it. This is what makes the QualiaOS runtime shelf cheap: a JavaScript engine compiled to Wasm is in memory once however many apps use it, and each app's instance has its own linear memory and its own capabilities. The kernel has no notion of a "shelf"; that is a convention the app manager follows using ordinary module loading.

## WebAssembly runtime

### Execution strategy

Two execution modes, both behind one interface:

- **Interpreter.** Always present. Used for the first boot of a module, for debugging, and as the fallback where compilation is not available. Simple, portable, slow.
- **Ahead-of-time compilation.** Modules are compiled to native `aarch64` code at load or install time, with bounds checks implemented through guard regions and explicit checks where guards cannot apply. Compiled code is cached on storage keyed by module hash and compiler version.

There is no just-in-time compilation at run time. Code pages are never writable while mapped executable (W^X), and the compiler runs in a dedicated kernel context with a bounded budget so that compiling a large module cannot stall real-time work.

Which compiler backend to use is an open question; the candidates are Cranelift (mature, large) and a purpose-built single-pass compiler (smaller, less optimizing). The plan is to start with the interpreter, add the single-pass compiler, and evaluate Cranelift when the surface is stable.

### Supported WebAssembly

- Core Wasm 2.0 plus the proposals the runtime shelf needs: bulk memory, reference types, multi-value, SIMD, threads with shared memory, tail calls, exception handling (for engines compiled from C++).
- The Component Model and WASI Preview 2 world shapes are the intended interface language for services and the host ABI, so that modules built with standard toolchains link without a custom SDK. Kalium exposes its own worlds; it does not promise WASI compatibility for everything.

### Traps and termination

A trap in an instance (out-of-bounds access, unreachable, stack overflow, fuel exhausted) terminates the faulting thread and notifies the parent. It never affects other instances. Memory of a terminated instance is reclaimed after all capabilities referring to it are revoked.

### Preemption

Compiled code is preempted by timer interrupts like any native code; the scheduler saves and restores the native context. The interpreter checks an epoch counter at loop back-edges and calls. Fuel metering is available per-instance for budgeted background work.

## Memory

- Physical memory is managed in pages by a buddy or bitmap allocator; the choice is deferred until the first target SoC is chosen.
- Each instance has its own address space. Linear memory is a contiguous virtual region with guard pages after it so that most bounds checks are free. Growing linear memory maps new pages into the reserved region.
- Module code pages are shared read-only across instances of the module.
- Shared memory between instances (for IPC, for the display pipeline, for the NPU) is explicit: a memory object is a capability, mapped into an instance's address space outside its linear memory and exposed to Wasm as an imported memory or through the host ABI.
- Copy-on-write is used for instance creation from a module with a pre-initialized memory image (a snapshot), which is how the runtime shelf starts a JS engine instance in milliseconds rather than running its initialization each time.

## Capabilities

A capability is an unforgeable kernel handle that names an object and a set of rights on it. Objects include: instances, threads, memory objects, IPC channels, device endpoints, timers, and scheduling budgets. Capabilities live in a per-instance table and are referred to from Wasm by index.

Rules:

- Capabilities are created by the kernel at boot (for the init service) and by instances that hold a capability with the `derive` right, which lets them mint weaker capabilities on the same object.
- Capabilities are passed over IPC. The receiver's table gains a new entry; the sender's is unchanged unless it chooses to drop it.
- Revocation is by the holder of the original capability and takes effect on the next host call that would use a derived handle.
- Host function imports are resolved from capabilities at instantiation. An instance with no display capability does not get a display import that fails; it gets no display import. Host calls take a capability index as their first argument, so even the resolved imports cannot be used on objects the instance does not hold.

There are no users, groups or ambient permissions. QualiaOS's user-facing permissions are implemented by a service that holds broad capabilities and derives narrow ones for apps.

## Scheduling

Two scheduling classes:

- **Real-time (RT).** Threads with a period and a budget. The scheduler admits an RT thread only if the total RT load leaves headroom, and runs RT threads earliest-deadline-first. The system agent's inference loop and the audio service's frame handling are RT. An RT thread that exhausts its budget is demoted to the fair class for the rest of its period rather than allowed to starve others.
- **Fair.** Weighted fair queuing for everything else. Weights express foreground versus background, and background budgets enforce the metering the app manager relies on.

SMP: the kernel runs on all cores. RT threads can be pinned to the core(s) nearest the device they serve. The NPU is driven from an RT thread so that command submission for each inference cycle happens on time.

## IPC

Communication between instances uses typed channels. A channel is a pair of capabilities (send, receive) over a bounded queue of messages. Messages are byte payloads plus an optional list of capabilities to transfer. Large data goes through shared memory objects rather than through the queue.

The Component Model's interface types are the intended way to describe channel protocols so that both ends can be generated from one definition. Synchronous call/return is built on channels with a reply channel; there is no separate RPC primitive in the kernel.

## Devices

Drivers live in the kernel when they need interrupt handling with tight latency or direct control over memory the MMU cannot delegate; otherwise they live in service instances that hold a device-endpoint capability granting MMIO and interrupt delivery for one device. The intended split:

| In kernel                                       | In services                                         |
| ----------------------------------------------- | --------------------------------------------------- |
| Interrupt controller, timers, MMU, SMP bring-up | Storage block devices                               |
| Display controller (scan-out)                   | Network interfaces and the IP stack (open question) |
| NPU command queue and memory mapping            | Touch, sensors, modem control                       |
| Audio DMA (deadline sensitive)                  | Camera pipelines                                    |

Device endpoints for service-side drivers are capabilities like anything else, so a driver crash is a service crash and the device can be re-attached to a fresh instance.

### The NPU path

The QualiaOS agent is the reason this kernel exists, so the NPU gets a named path:

1. At boot the kernel maps the model's weight image as a read-only memory object and reserves a memory object for the model's context state (the sliding window of tokens and their key/value cache).
2. The agent service receives capabilities to both and to the NPU command queue. Nothing else can touch them.
3. Each inference cycle the agent's RT thread submits one layer's work to the queue with pointers into the weight and context objects, waits for completion, runs the decoder, and sleeps until the next period.
4. The kernel enforces the period. If an NPU submission overruns, the next cycle is delayed, counted, and reported; the agent is told it missed a cycle rather than silently drifting.
5. The context memory object is checkpointed to encrypted storage at a configurable interval so that the agent's state survives power loss.

The driver's job is submission, completion, memory mapping and power management. It does not know what a layer is; the agent service does. The vendor-specific parts are isolated behind a trait so that supporting a new NPU means one new HAL module.

## Host ABI

Host functions are grouped by namespace; an instance imports a namespace only if it holds a capability of the matching kind.

| Namespace        | Purpose                                                                   |
| ---------------- | ------------------------------------------------------------------------- |
| `kalium:core`    | Capability table operations, time, random, yield, exit. Always available. |
| `kalium:spawn`   | Instantiate modules, create threads, set scheduling class, terminate.     |
| `kalium:memory`  | Create, map, unmap and share memory objects.                              |
| `kalium:ipc`     | Channels: create, send, receive, wait on sets of channels.                |
| `kalium:device`  | MMIO mapping, interrupt waits, DMA buffers for a held device endpoint.    |
| `kalium:display` | Submit surfaces to the compositor path, vsync.                            |
| `kalium:audio`   | Frame submission and capture with deadlines.                              |
| `kalium:npu`     | Command queue and completion for the model.                               |

The ABI is defined once in a crate shared between the kernel and a user-side SDK, and expressed as Component Model worlds so that services can be built with standard tooling. Stability is per namespace: `kalium:core` is meant to be stable early; the device namespaces will churn until the first SoC is supported.

## Boot

1. The platform firmware loads the kernel image and jumps to it.
2. The HAL sets up exception vectors, the MMU with an identity map for the kernel (write-xor-execute: code read-only and executable, everything else never executable, unmapped guard pages below the kernel stacks, a separate exception stack), timers, and secondary cores.
3. The memory manager takes over the physical map from the firmware's description (device tree); the prototype already sizes its heap from the device tree's memory node.
4. The Wasm runtime initializes and loads the boot image: a set of modules and a manifest saying which to instantiate first and with what capabilities.
5. The init service is instantiated with the root capabilities and starts everything else over IPC.

The boot image is signed; the kernel verifies it before loading. Module hashes in the image are what the AOT cache is keyed on.

## Security model, summarized

- Two sandboxes, not one: the Wasm semantics stop a module from addressing anything outside its linear memory, and the MMU stops a compiler bug or a hardware speculation issue from being enough on its own.
- No ambient authority; every action is through a held capability.
- Code is never writable and executable at once.
- The attack surface of the kernel is the host ABI and the Wasm loader. Both are small by design and written in Rust.
- Devices owned by service instances mean a driver compromise is bounded by that service's capabilities.

## Proposed crate layout

```text
kalium/
├── Cargo.toml                workspace
├── kernel/                   the kernel binary, ties the crates together
├── crates/
│   ├── kalium-hal/           arch-specific: boot, exceptions, MMU, timers (aarch64 first)
│   ├── kalium-mm/            physical allocator, address spaces, memory objects
│   ├── kalium-sched/         threads, RT and fair classes, SMP
│   ├── kalium-cap/           capability objects, tables, derive/revoke
│   ├── kalium-ipc/           channels and capability transfer
│   ├── kalium-wasm/          loader, validator, interpreter, AOT backend trait
│   ├── kalium-wasm-interp/   the interpreter
│   ├── kalium-wasm-aot/      the single-pass compiler (later: Cranelift backend)
│   ├── kalium-abi/           host ABI definitions, shared with the SDK (no_std, no kernel deps)
│   ├── kalium-drivers/       in-kernel drivers: irq, timer, display, audio DMA, NPU
│   └── kalium-boot-image/    boot image format, signature verification
├── sdk/                      user-side crate for writing services against kalium-abi
└── docs/
```

`kalium-abi` is the contract. It must build without the rest of the kernel so that services and the SDK depend on it alone.

## Roadmap

Done in the prototype (see [status.md](status.md) for details and numbers):

1. Boot on QEMU `virt` (aarch64) with a serial console and the interpreter running Wasm services. Done.
2. Memory objects, capabilities (with derivation and revocation), IPC with capability transfer, instances talking. Done.
3. Scheduler with both classes and a real-time load test. Done.
4. Interpreter performance work, cross-instance imports, snapshots for fast start, WASI preview1 so unmodified Rust std and C programs run. Done.

Next:

5. Single-pass AOT compiler for aarch64; run a shelf-candidate JS engine compiled to Wasm.
6. SMP: secondary cores, per-core run loops, cross-core channels.
7. Display and input on QEMU; first compositor service from QualiaOS.
8. First real SoC: HAL, display, audio, NPU driver. Pick the SoC by NPU openness.
9. Agent service running a stub model at the target cadence with the RT guarantees measured.

## Open questions

- Interpreter-only versus AOT at first; and which AOT backend.
- Where the IP stack lives. In a service is purer; in the kernel is faster for the modem path. Default: service, measured against the alternative.
- Component Model as the only ABI surface, or also a flat core-Wasm ABI for minimal modules.
- Target SoC and the openness of its NPU interface, which decides how much of the NPU driver can be open source.
- Checkpoint cadence and encryption scheme for the agent's context state.
