# Plan for `core-server` IPC Framework (v1.0 Phase 1)

## Context
We are migrating the Overlay Engine from an in-process DLL (`rust-renderer`) to a standalone server process (`core-server`) using DirectComposition Cross-Process Surface Sharing. The initial technical spikes for IPC named pipes, DuplicateHandle, and DComp surface attachment have succeeded. This plan covers scaffolding the `core-server` workspace member and implementing the robust IPC framework required to support Producers and Consumers.

## Approach
1. **Scaffold `core-server`**
   - Create a new binary crate `core-server` in the workspace.
   - Reuse existing D3D11/DComp initialization from the spike.
   - Ensure the new workspace member has the correct `Cargo.toml` dependencies (e.g., `windows`, `tokio` or standard `std::thread`, `parking_lot`).

2. **Define IPC Protocol (Control Plane)**
   - The named pipe `\\.\pipe\overlay-core` will handle low-frequency events.
   - Message Structure: `Magic (4 bytes) | Version (2 bytes) | Opcode (2 bytes) | Payload Length (4 bytes) | Payload`
   - Initial Control Opcodes:
     - `0x0001`: Register Producer
     - `0x0002`: Register Consumer (returns `consumer_id`)
     - `0x0003`: Create Canvas (Producer -> Core)
     - `0x0004`: Attach Consumer (Producer -> Core) -> Triggers DComp surface creation and handle duplication.
   - Create a `protocol` module defining these structs and serialization/deserialization logic.

3. **Core Server State Management**
   - Implement `ServerState` wrapped in a `Mutex` (or `RwLock`).
   - Track active Producers (Clients that own canvases).
   - Track active Consumers (Monitors that wait for surface handles).
   - Track Canvases (Logical size, render resolution, backing D3D11 texture, DComp surface).

4. **Connection Handling & Lifecycle**
   - Run a dedicated thread/async task to accept incoming Named Pipe connections.
   - Spawn a handler thread per client.
   - Implement heartbeat/ping mechanism (5s ping, 10s timeout).
   - Handle client disconnects:
     - If a Producer disconnects, destroy its canvases and notify attached Consumers.
     - If a Consumer disconnects, remove it from the active monitors list.

5. **Error Handling**
   - Adapt `rust-renderer/src/error.rs` into `core-server/src/error.rs`.
   - Add new IPC-specific errors (e.g., `IPC_READ_FAIL`, `INVALID_MAGIC`, `UNKNOWN_OPCODE`).

## Verification
- Run `cargo build -p core-server` to ensure compilation.
- Write unit tests for the `protocol` module to verify serialization/deserialization.
- Create a mock client script or a small test binary to connect to the named pipe, send a `Register Producer` message, and ensure the server responds and keeps the connection alive.