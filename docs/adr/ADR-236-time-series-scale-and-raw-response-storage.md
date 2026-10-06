# ADR-236: Time-Series Scale and Raw-Response Storage

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** design 4.3, 4.6, 10.3, 17 (P1 removed); `dataset_chunks.chunk_format` (`db/README.md`); `agent` monitoring ring buffer; `server` acquired-data storage and downsampling

## Context

Design 17 P1 left open the scale of time-series data: the sampling period, how long a recording
runs and how many signals it carries. The answer decides the storage format of raw time-series
data and how display data is downsampled. `dataset_chunks` already carries a `chunk_format`
version so that the format can change later; this ADR fixes the first version and its limits.

The achievable rate is set by the vehicle, not by this system:

- A UDS read is one request and one response per data identifier. With classic CAN at 500 kbit/s,
  a 303-byte response (150 two-byte values in one DID) needs about 44 ISO-TP frames, roughly
  12 ms of bus time alone, and 20-50 ms per read once ECU and VCI latency are added. CAN FD
  brings the same response down to about five frames.
- DoIP removes the bus limit between the tester and the vehicle gateway, but most ECUs still sit
  behind the gateway on CAN or CAN FD. A published scan of a production vehicle over DoIP
  (every DID of 41 ECUs read with ReadDataByIdentifier) averaged about 25 ms per request.
- With ReadDataByPeriodicIdentifier (ISO 14229-1:2026 10.5) the rates and the ECU's scheduler
  are manufacturer-defined, and the effective period grows as more identifiers are scheduled.
- K-Line, J1850 and LIN are slower still: a single 303-byte response does not fit one K-Line
  message.

What is useful for workshop diagnosis is far smaller: freeze frames of a few snapshots at 0.5 s
spacing, live data of a handful of values updated about once a second, and recordings of a few
seconds to a few minutes around a trigger. Engineering measurement (XCP at 10 ms with hundreds of
bytes per raster) is the upper end the maintainer asked to keep in reach.

Storing every decoded value as its own `(timestamp, value)` sample costs about 16 bytes per value.
A multi-value DID is far smaller kept as received: 150 two-byte values cost about 2,400 bytes as
samples but 303 bytes plus one timestamp as the raw response.

The options were: (A) limits sized for monitoring and acquisition jobs, with a row-per-response
format; (B) recordings of hundreds of signals over hours, needing a columnar format and
multi-level downsampling from the start; (C) recording every frame on the bus as a separate kind
of data. The maintainer chose A, with the limits expressed in raw bytes rather than samples.

## Decision

1. **Terms.** A *signal* is one decoded value recorded over time (engine speed, coolant
   temperature); design 17 called it a channel, which is not the J2534 channel. A *record* is one
   response received from the vehicle (or one value read from the VCI, such as battery voltage),
   with its timestamp.
2. **Raw records are what is stored.** Time-series data is stored as records: the response bytes
   as received, a monotonic timestamp and a source identifier naming the ECU and the request.
   Signals are decoded from records with the pre-expanded decode plans of the IR declaration part
   (8.2.2) that produced the recording, identified by the dataset's IR digest. Decoded values are
   derived data and are never the stored original. This keeps raw data immutable (4.3), lets a
   corrected conversion be applied to an old recording, and is about one eighth of the size of
   per-value samples for a multi-value DID.
3. **Limits for the initial implementation**, per monitoring session or acquisition job:
   - fastest period per request: 10 ms (R10);
   - record throughput: up to 64 KiB/s of response bytes in total (for example two 303-byte DIDs
     at 10 ms each); the number of signals is not limited separately, since decoding cost follows
     the bytes;
   - monitoring ring buffer (4.6): look-back of up to 10 minutes, capped by the memory the agent
     allows;
   - time series recorded by one acquisition job: up to 1 hour.

   At session start the agent checks the requested set against the throughput limit along with
   the measured achievable interval (10.3) and reports the reduced intervals it will use when the
   request exceeds it.
4. **Chunk format, version 1** (`chunk_format = "records/1"`): a chunk holds a source table
   (source identifier to ECU and request) and the records of one time window in arrival order,
   with delta-encoded timestamps and length-prefixed response bytes, compressed with zstd. A
   chunk closes after 10 s or 1 MiB of uncompressed records, whichever comes first.
5. **Downsampling.** The server decodes a chunk once and keeps per-signal min/max summaries at a
   small set of fixed bucket widths as derived data; a display request picks the bucket width for
   its zoom level and reads raw records only when zoomed in to record resolution.
6. **Out of scope:** multi-hour recordings of hundreds of signals (option B) and full bus-frame
   logging (option C). Either is added later as a new `chunk_format` version or a new kind of
   acquired data, without migrating existing chunks.

## Consequences

- The upper end is about 38 MiB of agent memory for a full 10-minute ring buffer and about
  230 MiB of uncompressed records for a 1-hour job at the full throughput limit; typical workshop
  recordings are well under 1 MB.
- Reading a stored recording needs the IR the dataset names. The IR versions referenced by
  datasets must be retained as long as the datasets are (4.3 retention).
- A signal-level export (CSV or a measurement format) is produced by decoding, not by reading
  stored values.
- The throughput limit is a starting value from the estimates above. It is revisited with
  measurements from real VCIs (M8) and when design 17 P5 sets non-functional targets.
