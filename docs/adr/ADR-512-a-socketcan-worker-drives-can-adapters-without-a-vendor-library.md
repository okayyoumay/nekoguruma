# ADR-512: A SocketCAN Worker Drives CAN Adapters Without a Vendor Library

**Date:** 2026-11-28
**Status:** Accepted
**Affects:** design 2 (Assumptions), 3.4, 7.1, 7.1.3, 7.3, 9.5, and with the worker 8.11 and 13.5; a new worker crate `ngr-socketcan-service`; new
shared crates `ngr-socketcan` and `ngr-isotp`; `j2534-0404-service` (`src/service/isotp.rs`),
`ngr-hw-checks` (`src/guard/socketcan.rs`), `ngr-library-resolver`, `worker-host`, the agent's
discovery and `capabilities` (9.5)

## Context

On Linux the two workers reach a VCI only through a vendor library: `j2534-0404-service` loads a
J2534 v04.04 shared library found through a registration definition (7.1.1), and
`iso22900-service` loads a D-PDU API library found through the root description file (7.1).
Whether vendors ship such libraries for Linux, and with which `unsigned long` width, is still
one of design 17's per-vendor questions. Many USB-CAN adapters, by contrast, have a driver in
the mainline Linux kernel and are reached through SocketCAN, with no vendor library at all. The
hardware checks already use one such adapter as the second end of a T0 loopback (ADR-343), and
the stand-in ECU of T1 is to serve `sim-ecu` over SocketCAN with the kernel's ISO-TP sockets.

The options weighed were: keep the vendor-library path only; write a J2534 v04.04 library of our
own over SocketCAN and load it into `j2534-0404-service`; drive AT-command serial adapters
(ELM327 and similar); adapt vendor-specific CAN SDKs; and add a worker that implements the
common worker interface (`vci-service-interface`) directly on SocketCAN. The serial adapters
give too little control of flow control and timing for write jobs (5.6) or for timing judged on
the VCI's clock (ADR-356). Vendor SDKs are not standard APIs, so each would be one more adapter
in the core (design principle 5), and the adapters they serve mostly have SocketCAN drivers as
well. A J2534 library of our own would reuse all of `j2534-0404-service`, but would keep
SocketCAN behind an API that cannot carry what the kernel offers (its timestamps, CAN FD, the
kernel ISO-TP) and would add a C-ABI crate.

The kernel's ISO-TP sockets (`CAN_ISOTP`) are in the mainline kernel from Linux 5.10. ADR-232
lets the workers run on kernels from 3.2, so a worker that relied on them alone would narrow the
supported hosts.

## Decision

1. **A third worker kind.** `ngr-socketcan-service` is a worker service that implements
   `vci-service-interface` on SocketCAN. The agent starts, authenticates and drives it as it
   does the other two workers (7.3, 7.4): one process per configured VCI, the same startup
   argument with the VCI's name and the operating mode, the same stdin key and bearer tokens.
   The agent does not branch on the kind beyond choosing the binary (design principle 6).
   What a CAN adapter cannot do (other buses such as K-Line or SAE J1850, pin switching, a
   battery voltage reading) the worker reports as absent through the interface, as the other
   workers report what their API cannot give.
2. **Scope first: raw CAN and ISO 15765 on classical CAN.** The worker's ComLogicalLinks are
   raw CAN links and ISO 15765-2 (2024) links: the transport layer protocol of clause 9 and the
   network layer protocol of clause 10, with 11-bit and 29-bit identifiers and the normal and
   extended addressing formats of 10.3.2 and 10.3.4. CAN FD (the larger data lengths of 9.5)
   comes later, as an extension of the same worker.
3. **Registration definitions at a fixed location.** A SocketCAN VCI is described by an
   administrator-written TOML file, one per VCI, in `/etc/nekoguruma/socketcan/` (design
   7.1.3). It names the VCI, the network interface and the bitrate the administrator configured
   on it. The directory is fixed at build time, with a runtime override in debug builds only,
   as for the J2534 definitions (ADR-228, ADR-282). The definition is resolved through
   `ngr-library-resolver`, in a module of its own, and checked with the 7.2 writability rules in
   the device's mode. There is no library to load, so no library or signer check applies.
4. **The worker never changes the interface.** Setting a CAN interface's bitrate or bringing it
   up needs `CAP_NET_ADMIN`, which a worker does not get. The administrator configures the
   interface outside this software; the worker reads its state and bitrate, and refuses to
   connect a link whose requested baud rate differs from the interface's, naming both values.
   Opening a raw CAN or ISO-TP socket needs no capability.
5. **ISO-TP has two interchangeable backends.** The worker's links reach ISO-TP through one
   transport trait with two implementations: a user-space engine on a raw CAN socket, and the
   kernel's `CAN_ISOTP` socket. The worker's per-name settings in its `config.toml` select one
   with `isotp_backend = "user" | "kernel" | "auto"`, keyed by the VCI's name as the other
   workers key theirs by the library's name (ADR-281). `auto` is the default: it opens a kernel
   ISO-TP socket and uses the user-space engine when the kernel refuses the protocol. It decides
   by what the kernel offers, not by a link's ComParams. The backend in use is
   reported in `capabilities` (9.5). The user-space engine keeps the worker on every kernel
   ADR-232 supports and gives a timestamp for every frame; the kernel backend keeps the
   separation time and flow control timing under load, but needs Linux 5.10 or later and
   delivers only whole messages, so per-frame times are not available from it and the
   communication log (8.11) records the message's times only. A ComParam the kernel's socket
   options cannot express is refused on that backend with its name, not ignored, under `auto`
   as under `kernel`. Design 8.11 states the coarser times when the worker is built, and design
   13.5 adds the worker as an API a hardware profile can name, with its checks.
6. **Shared crates, not copies.**
   - `ngr-socketcan` holds the SocketCAN access: raw CAN sockets with filters and kernel
     timestamps, reading an interface's state and bitrate, and the kernel ISO-TP socket. The
     worker, the hardware checks' guarded SocketCAN end (ADR-343, which keeps its guard by
     construction on top of it) and the stand-in ECU use it.
   - `ngr-isotp` holds the user-space ISO-TP engine, moved out of `j2534-0404-service`'s
     software ISO-TP mode (ADR-046, ADR-047), which has no J2534 dependency. It plays both the
     sending and the receiving role, and `j2534-0404-service`'s `software-isotp` mode and the
     new worker's user-space backend both use it.
   - The D-PDU API emulation of `j2534-0404-service` (ComPrimitive state machine, ComParam
     defaults and checks, the resource table) is not extracted now: it is bound to J2534 types
     throughout. The new worker holds a smaller one of its own for the CAN family, and whether
     the two are merged is decided once the new worker exists and the duplication can be
     measured.
7. **Host builds only.** The worker loads no vendor library, so there is no library ABI to
   match: one build per supported host architecture (x86_64 and aarch64, ADR-232) is bundled, and the
   32-bit worker builds of 7.3 do not apply to it. Its VCIs are reported with no ABI
   interpretation category.
8. **Milestone.** The worker is part of M5 (real-time monitoring), whose exit criteria measure
   each VCI's achievable interval: an acquisition and a monitoring session run against `sim-ecu`
   served over a virtual CAN interface, with both ISO-TP backends.

## Consequences

- Linux hosts get a diagnostic path that needs no vendor library, with adapters whose drivers
  are in the mainline kernel; the vendor-library workers are unchanged.
- Adding a worker kind adds a discovery chain (7.1), a resolver module, a scheme the agent
  starts and a `capabilities` entry, but no change to the common worker interface.
- With the kernel backend, timing judged per frame is coarser than with the user-space engine;
  the backend in use is visible in `capabilities`, so a coarse result can be traced to it.
- On a kernel older than 5.10, `auto` falls back to the user-space engine, and `kernel` fails to
  connect the link with the reason; ADR-232's kernel floor is unchanged for the worker as a
  whole.
- Two near-duplicate D-PDU emulations exist until the merge decision of item 6 is taken.
