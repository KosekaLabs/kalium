# Kalium

Kalium is the kernel of [QualiaOS](https://github.com/koseka/qualia-os). It is
written entirely in Rust, runs on bare metal on mobile SoCs, and executes
exactly one kind of program: WebAssembly.

There is no native user space. Every service, every interpreter, every app
above the kernel is a WebAssembly module, isolated by the Wasm sandbox and the
MMU together, and authorized by capabilities rather than by user IDs. The
kernel does not know what JavaScript, HTML, an app or a browser is; those are
built out of WebAssembly modules that the OS ships next to it.

## What the kernel does

- Boots the SoC, sets up memory and the MMU, handles interrupts.
- Schedules WebAssembly instances, with a real-time class for the system
  agent and the audio pipeline.
- Runs WebAssembly: loads, validates, compiles or interprets, and executes
  modules with the sandbox guarantees the format promises.
- Mediates all access to hardware and to other instances through
  capabilities.
- Drives devices: display, touch, audio, storage, radios, and the NPU.

## What the kernel refuses to do

- Load or execute native code from user space.
- Carry a file system, network stack, window system or shell as kernel code
  where a WebAssembly service can do the job.
- Special-case any language, interpreter or application.

## Documentation

- [docs/architecture.md](docs/architecture.md): design, subsystems, the host
  ABI, security model, scheduling, memory, the NPU path, and a proposed
  crate layout.

## Status

Design phase. There is no code yet; the architecture document is the plan.

## License

Apache License 2.0. See [LICENSE](LICENSE).
