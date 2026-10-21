# ADR-343: The Guarded SocketCAN End of a T0 Loopback

**Date:** 2026-10-21
**Status:** Accepted
**Affects:** `ngr-hw-checks` (`src/guard/socketcan.rs`, `src/checks/t0_loopback.rs`),
`j2534-0404-service` (`tests/hw_t0_loopback.rs`, `tests/grpc_mock/t0_loopback_check.rs`),
`crates/ngr-hw-checks/docs/hardware-profile.md`, design 13.5, ADR-275, ADR-333

## Context

Design 13.5's T0 class lets a loopback run between a VCI channel and a second CAN interface, a
USB-CAN adapter with a SocketCAN driver in the mainline Linux kernel. The hardware profile already
accepts such an endpoint (`socketcan = "can0"`). The T0 check, `hw_t0_two_endpoint_loopback`,
refused it, because nothing in the workspace could drive SocketCAN.

The guard of design 13.5 and ADR-333 decides worker requests: every RPC a check sends to the VCI's
worker. A SocketCAN end is not on a worker, so that guard never sees its traffic. A check that
opened a raw socket itself could send any frame on the bus. That would break the rule that a check
reaches the hardware only through what the harness hands it, and that on T0 only traffic between
the two loopback endpoints passes.

The questions were:
- what guards the SocketCAN end;
- how its frames relate to the VCI end's;
- how CI tests it without hardware;
- what happens on an operating system without SocketCAN.

## Decision

1. **A guard by construction.**
   - `guard::GuardedSocketCan` is the SocketCAN end a check gets. The harness opens it for a T0
     profile's SocketCAN endpoint, and only for that.
   - Its only send call takes a payload of 1 to 7 bytes (6 under extended or mixed addressing). It
     builds the frame from the endpoint's link: the `request_id`, as a 29-bit identifier when
     `id_bits` is 29; the address byte when the addressing has one; and the `padding` byte up to 8
     bytes when the profile pads.
   - No call takes a whole frame or another identifier, so the end can send nothing but its own T0
     frames, the same frames the worker guard lets a VCI end send in RawMode (ADR-333). There is no
     decision function to keep in step with the VCI guard, and nothing for an unclassified frame to
     slip through.
2. **The same bytes as the VCI end.**
   - The end writes and compares a frame as the J2534 worker's RawMode data is: the identifier in
     four bytes, most significant first, then the CAN data. `GuardedSocketCan::frame` and
     `GuardedHandle::raw_frame` give the same bytes for the same link, so the check computes the
     expected frame the same way whichever end sends.
   - Padding is added on the bus, as the VCI's frame-padding flag adds it. A SocketCAN receiver
     therefore matches the other end's frame padded as that end's link pads.
   - The end receives every frame other nodes send, which is the raw socket's default, and none of
     its own. It skips error and remote frames.
3. **The check takes a VCI end and a SocketCAN end.**
   - `t0_loopback::Ends::VciAndSocketCan` gives the check one guarded handle and the SocketCAN end.
     The profile says which endpoint the SocketCAN end is.
   - The check drives both kinds of end through one internal `End`, with the same steps: listen,
     drain until quiet, send, then receive the exact frame within the timeout.
   - Its result message names the interface.
   - A SocketCAN end given for a profile without one, or none given for a profile with one, is
     refused with nothing sent.
4. **The bus is a trait.**
   - `GuardedSocketCan` sends and receives through `socketcan::CanBus`. On Linux the only
     implementation is a raw CAN socket (`PF_CAN`, `SOCK_RAW`, `CAN_RAW`, bound to the interface
     by its index), through `libc`, which the workspace already uses.
   - `GuardedSocketCan::with_bus` takes another bus, so the guard runs against a fake in
     `ngr-hw-checks`' unit tests. The whole check also runs in `j2534-0404-service`'s mock suite
     against a bus wired to the mock's channel: what the VCI end writes arrives at the fake bus,
     and what the SocketCAN end sends is injected into the channel's receive queue.
   - No CI test needs a `vcan` interface. The hosted runners cannot be relied on to load the
     kernel module.
5. **Linux only.** On any other operating system `GuardedSocketCan::open` refuses. The hardware
   test opens the SocketCAN end before any device, so the check is refused there before anything
   opens or is sent.
6. **No interface settings.** The harness neither brings the interface up nor sets its bit rate.
   Both need privileges a test should not hold, and the operator sets them up with the profile's
   `baud_rate` (`ip link set can0 up type can bitrate 500000`). An interface that is down fails the
   check through the socket, as an unwired bus does.

## Consequences

- A T0 loopback no longer needs two VCI channels or two VCIs: a J2534 VCI and a USB-CAN adapter on
  Linux are enough.
- The SocketCAN end checks only what it sends. The bus may carry other traffic, which the end skips
  while it waits, and which makes the drain fail when it does not stop, as on a VCI end.
- The receive and drain calls block. The check runs them under `tokio::task::block_in_place`, so it
  needs a multi-threaded runtime, which the hardware test and the mock suite already use.
- The interface name appears unmasked in the result message: the report masks VCI serial numbers,
  and an interface name such as `can0` is none.
- T1's SocketCAN front end of `sim-ecu` (ADR-275) is separate work. It serves the simulated ECU on
  an interface, which this end does not do.
