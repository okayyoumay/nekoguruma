# Keyed Instance Stop Design

Related maintenance note: [implementation-notes.md](implementation-notes.md)

## Current Status (2026-05-20)

This document is historical design context.
The keyed-stop primitives described here are not part of the current source tree and are not wired into the active stdio JSON-RPC lifecycle path used by run_stdio_server().

- Active runtime path today:
    - startup/serve loop: src/jsonrpc.rs
    - local server state and stop: src/jsonrpc/server.rs
    - startup bootstrap: src/startup.rs
- Removed from active codebase during maintainability cleanup:
    - src/jsonrpc/registry.rs
    - src/jsonrpc/stop.rs
    - src/jsonrpc/child.rs
    - src/platform/**

Treat this file as an archived design note, not as current implementation documentation.

## Overview

The ISO22900 service codebase includes a **registry-key-based addressing model** for cross-platform instance identification and graceful termination. The model identifies instances by a stable hash key derived from identifier and library name to avoid PID-reuse risks when remote-instance lifecycle orchestration is enabled.

## Problem Statement

### PID-Based Approach Limitations

The previous Unix-only signal-based termination relied on process IDs:
- **PID reuse risk**: Operating systems recycle PIDs after process termination, risking accidental signals to unrelated processes
- **Platform inconsistency**: Windows used task-based APIs while Unix used signals, complicating multi-platform development
- **Recovery gaps**: Stale registry entries pointing to recycled PIDs could cause unexpected behavior

### Solution: Keyed Addressing

Instead of addressing instances by PID, use a stable registry key (hash of instance identifier + library name):
- **Deterministic**: Same ID + library name always produces the same key on all platforms
- **No reuse**: Hash keys do not collide with system PID pools
- **Platform-agnostic**: Enables unified addressing semantics across Windows, Linux, and other Unix variants

## Implementation Details

### Key Calculation (Historical)

Previously located in src/jsonrpc/registry.rs:

```rust
pub fn registry_key(identifier: &str, library_name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    identifier.hash(&mut hasher);
    library_name.hash(&mut hasher);
    hasher.finish()
}
```

- **Input**: Instance identifier (e.g., `"id-001"`) and library name (e.g., `"D_PDU_API_Bosch_6531_Bosch"`)
- **Output**: 64-bit stable hash (formatted as hex: `key:016x`)
- **Usage (historical path)**: Was computed in registry/stop helper flow; not used by current active stdio dispatch/startup path.

### Platform-Specific Stop Endpoints (Historical)

#### Windows: Named Event (historical path in src/platform/process/windows.rs)

```
Event Name: Local\iso22900-service-stop-<key>
```

**Server-side (listener)**:
- Creates an auto-reset Win32 event with the key-based name
- Spawns a background thread that waits with `WaitForSingleObject(INFINITE)`
- Upon signal, consumes the shutdown oneshot, triggering gRPC drain

**Stopper-side**:
- Computes the same key from registry entry
- Opens the event by name via `OpenEventW`
- Signals it via `SetEvent`

**Benefits**:
- Kernel-managed IPC with no file cleanup needed
- Atomic signaling (no race between process exit and socket deletion)
- Direct escalation path to `TerminateProcess` on timeout

---

#### Unix/Linux: Datagram Socket (historical path in src/platform/process/unix.rs)

```
Socket Path: /tmp/iso22900-service-stop-<key>.sock
```

**Server-side (listener)**:
- Removes any stale socket from previous runs
- Binds a Unix datagram socket with the key-based path
- Spawns a background thread that blocks on `recv()` waiting for one-byte payload
- Upon receipt, cleans up socket and consumes shutdown oneshot

**Stopper-side**:
- Computes the same key from registry entry
- Creates an unbound datagram socket
- Sends a single byte to the known socket path via `send_to()`

**Benefits**:
- No special permissions required (unlike `kill()`)
- File path is deterministic and collision-free
- Built-in cleanup prevents socket reuse attacks

**Error Handling**:
- `ENOENT` (socket doesn't exist) → returns `Ok(false)` (no running instance)
- `ECONNREFUSED` → same treatment (socket bound but no listener)
- Other errors are propagated

---

### Startup Flow (Active Path)

**Location**: `src/startup.rs::spawn_grpc_server`

```rust
pub async fn spawn_grpc_server(
    library_name: &str,
    port: Option<u16>,
) -> Result<RunningGrpcServer, Box<dyn std::error::Error>> {
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // Shutdown is driven by local lifecycle state (watch channel).
    let shutdown = async move {
        while !*shutdown_rx.borrow() {
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }
    };
}
```

**Key points**:
- Active implementation currently uses local watch-channel shutdown semantics.
- Keyed stop listener setup is not invoked in active startup flow.
- tonic's `serve_with_incoming_shutdown()` still handles graceful drain.

---

### Stop Call Flow (Remote, Historical)

**Previous location**: src/jsonrpc/stop.rs::stop_remote_instance

```rust
pub async fn stop_remote_instance(
    registry_path: &Path,
    key: u64,                   // Passed from caller
    pid: u32,                   // Still needed for force termination
    port: u16,
) -> io::Result<bool> {
    // ... validate process is running ...

    // Signal via keyed endpoint
    if !platform::request_graceful_stop(key)? {
        return Err(graceful_signal_error());
    }

    // Wait for port to close (graceful shutdown)
    if wait_for_port_close(port, GRACEFUL_SHUTDOWN_TIMEOUT).await {
        let _ = fs::remove_file(registry_path);
        return Ok(true);
    }

    // Timeout → escalate to force termination
    eprintln!("Graceful shutdown timed out, attempting force termination.");
    match platform::force_terminate(pid) {
        Ok(()) => {
            tokio::time::sleep(POST_FORCE_TERMINATE_SETTLE).await;
            let _ = fs::remove_file(registry_path);
            Ok(true)
        }
        Err(e) => Err(force_terminate_error(&e))
    }
}
```

**Current active status**: module removed; no call site in active stdio JSON-RPC dispatch path.

**Flow**:
1. Lookup registry entry → extract `pid` and `port`
2. Compute same key as server did at startup
3. Attempt graceful stop via keyed endpoint
4. Wait for port closure
5. If timeout, fall back to `force_terminate(pid)` using PID (valid for immediate termination)

---

## Design Rationale

### Why Both Key AND PID?

**Graceful stop** uses the key (cross-platform, collision-proof) because:
- It must work even if PIDs are recycled
- Keyed signaling is deterministic and safe

**Force termination** still uses PID because:
- It's an emergency escalation (process is unresponsive)
- At that point, the registry entry is definitely about the right process (we just tried graceful stop)
- No cross-platform abstraction needed (rarely triggered in normal operation)

### Why Temp Directory on Unix?

**`/tmp/iso22900-service-stop-<key>.sock`** is appropriate because:
- Accessible to all users running the service
- Automatically cleaned by OS on reboot
- Race-condition window is minimal (listen → send is synchronous in protocol)
- No SELinux/AppArmor complications (unlike `/var/run` or socket activation)

### Why Auto-Reset Event on Windows?

The Win32 event is **auto-reset** (not manual) because:
- One signal → one wakeup (no spurious second wakeup)
- Listener thread consumes the event naturally
- Matches Unix socket recv semantics (one datagram consumed per call)

---

## Testing Recommendations (If Reintroduced)

### Windows Validation (when staged path is wired)
```bash
# Build and run
cargo build --package iso22900-service --target i686-pc-windows-gnullvm

# Verify stop event listener creation
# (Enable debug logging in setup_stop_event_listener to confirm event name)
```

### Unix/Linux Validation (when staged path is wired)
```bash
# Build for Linux
cargo build --package iso22900-service --target x86_64-unknown-linux-gnu

# Test socket creation:
ls -la /tmp/iso22900-service-stop-*.sock

# Verify stale socket cleanup on restart
```

### Cross-Platform Tests (when staged path is wired)
1. **Same identifier + library on different platform**:
   - Verify hash is identical (reproducible key)
   - Different IPC endpoints (event vs socket)
   
2. **PID reuse scenario**:
   - Registry points to old PID
   - New unrelated process gets same PID
   - Graceful stop targets keyed endpoint (not old PID)
   - Unrelated process unaffected

3. **Graceful shutdown timeout**:
   - Listener receives stop signal
   - Port still open after timeout
   - Force termination via PID succeeds
   - Registry cleaned up

---

## Historical Files

| File | Change |
|------|--------|
| `src/platform/process.rs` | Generalized signatures to use `u64 key` instead of `u32 pid` for graceful stop |
| `src/platform/process/windows.rs` | Replaced CTRL_BREAK_EVENT with key-based Win32 named event |
| `src/platform/process/unix.rs` | Implemented key-based Unix datagram socket listener |
| `src/startup.rs` | Active path currently uses local watch-channel shutdown |
| `src/jsonrpc/registry.rs` | Exposed `registry_key()` public function |
| `src/jsonrpc/stop.rs` | Updated `stop_remote_instance` signature to accept key |

## Compatibility Notes

- **Backward Compatibility**: Registry file format unchanged (still stores `pid` and `port`)
- **Existing Servers**: Can coexist if on different identifiers or library names (different keys)
- **Migration**: No data migration needed; key computed on-the-fly from registry


## If Keyed Stop Is Reintroduced

The archived design is not runtime behavior. Bringing it back would also need:

- dedicated lifecycle modules split out of the stdio JSON-RPC server, with integration tests of keyed stop through the stdio path;
- keyed descriptor passing for systemd socket activation;
- logging of key-to-PID mappings;
- on Unix, restricting stop-signal senders by OS user or group;
- on Windows, named-event inheritance rules that isolate child instances.
