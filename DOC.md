# General Software Architecture

## Events

Each instance of lan-mouse can emit and receive events, where
an event is either a mouse or keyboard event for now.

The general Architecture is shown in the following flow chart:
```mermaid
graph TD
    A[Wayland Backend] -->|WaylandEvent| D{Input}
    B[X11 Backend] -->|X11Event| D{Input}
    C[Windows Backend] -->|WindowsEvent| D{Input}
    D -->|Abstract Event| E[Emitter]
    E -->|Udp Event| F[Receiver]
    F -->|Abstract Event| G{Dispatcher}
    G -->|Wayland Event| H[Wayland Backend]
    G -->|X11 Event| I[X11 Backend]
    G -->|Windows Event| J[Windows Backend]
```

### Input
The input component is responsible for translating inputs from a given backend
to a standardized format and passing them to the event emitter.

### Emitter
The event emitter serializes events and sends them over the network
to the correct client.

### Receiver
The receiver receives events over the network and deserializes them into
the standardized event format.

### Dispatcher
The dispatcher component takes events from the event receiver and passes them
to the correct backend corresponding to the type of client.


## Requests

// TODO this currently works differently

Aside from events, requests can be sent via a simple protocol.
For this, a simple tcp server is listening on the same port as the udp
event receiver and accepts requests for connecting to a device or to
request the keymap of a device.

```mermaid
sequenceDiagram
    Alice->>+Bob: Request Connection (secret)
    Bob-->>-Alice: Ack (Keyboard Layout)
```

## Problems
The general Idea is to have a bidirectional connection by default, meaning
any connected device can not only receive events but also send events back.

This way when connecting e.g. a PC to a Laptop, either device can be used
to control the other.

It needs to be ensured, that whenever a device is controlled the controlled
device does not transmit the events back to the original sender.
Otherwise events are multiplied and either one of the instances crashes.

To keep the implementation of input backends simple this needs to be handled
on the server level.

## Device State - Active and Inactive
To solve this problem, each device can be in exactly two states:

Either events are sent or received.

This ensures that
- a) Events can never result in a feedback loop.
- b) As soon as a virtual input enters another client, lan-mouse will stop receiving events,
which ensures clients can only be controlled directly and not indirectly through other clients.

## File transfer (QUIC side channel)

Input events and clipboard text (≤ 64 KB) travel over the DTLS/UDP channel
described above. File/folder transfers and clipboard payloads that don't
fit that cap take a separate QUIC connection on `file_transfer_port`
(default: main port + 1).

Layout:

* One QUIC connection per transfer.
* A bidirectional control stream carries CBOR `FileCtrl` messages —
  `Offer { root, entries, total_bytes }`, `Accept`, `Decline`, `Abort`.
* One unidirectional data stream per filesystem entry, carrying CBOR
  `FileFrame` messages: `Entry { rel_path, size, mode, kind, compressed }`,
  zero or more `Chunk { data }` frames, and a final
  `EntryDone { blake3: [u8; 32] }` hash.

Trust reuses the DTLS identity: `rustls` is configured with a custom
`ClientCertVerifier`/`ServerCertVerifier` that checks the peer's SHA-256
cert fingerprint against the same `authorized_fingerprints` map that DTLS
consults. No PKI, no SNI check, same on-disk PEM loaded once.

Each chunk is compressed with zstd level 3 unless the entry's first 16
bytes match a known already-compressed magic (PNG, JPEG, MP4, zip, …) in
which case compression is skipped. The receiver verifies BLAKE3 per file
and writes to `<name>.lanmouse.part` until the hash matches, then
atomically renames.

Drop-detection (source of `FileDropEvent`) is a separate per-OS trait
`input_capture::file_drop::FileDropSource`; backends:

* `layer_shell_data_device` — wlroots Wayland, creates 1px edge surfaces
  and listens on `wl_data_device`.
* `dummy` — no events, used as fallback and on compositors without
  layer-shell support.

A GTK per-client drop target in the main window acts as the universal
fallback: drag a file onto any client row to send it even without a
working edge-drop backend.

