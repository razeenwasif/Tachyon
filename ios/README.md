# Tachyon for iPhone (`ios/`)

Native SwiftUI iPhone companion connecting directly to your MacBook (`tsukuyomi`), Mac mini (`kagutsuchi`), Linux PC, and Windows PC (`amaterasu`) over Tailscale TCP using Tachyon's 9-byte Mux v2 protocol and 256-bit bearer authentication.

## Components

- **`TachyonCore`** ([`ios/TachyonCore`](TachyonCore/)): Zero-dependency Swift 6 package containing:
  - `MuxCodec`: 9-byte little-endian Mux v2 binary framing + pure-Swift LZ4 snapshot decompressor (`src/mux/proto.rs` wire-compatible)
  - `PairingURI`: `tachyon://pair?...` encoder/decoder matching `src/mux/pairing.rs`
  - `MuxClient`: `Network.framework` (`NWConnection`) async TCP client with bearer auth, event subscriptions, PTY attach/resize/write, `block.send`, and screenshot upload
  - `TerminalBuffer`: VT100/ANSI/xterm/24-bit TrueColor/OSC 7501 state machine with scrollback & alt screen
  - `KeyEncoder`: sticky Ctrl, DECCKM arrows, bracketed paste, snippets, and cross-platform screenshot upload commands
  - `KeychainTokenStore`: `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` token storage
  - `FleetStore`: `@Observable` multi-host store with exponential-backoff reconnect and urgency-ranked Live Agents
- **`TachyonApp`** ([`ios/TachyonApp`](TachyonApp/)): Obsidian dark minimal SwiftUI app (`AgentsInboxView`, `SpacesHostsView`, `TerminalSessionView`, `TerminalCanvasView`, `TouchAccessoryBar`, `PairingView`, `SettingsView`).

## Generating & Running

```sh
xcodegen generate --spec ios/project.yml --project ios
open ios/Tachyon.xcodeproj
```

Or run the `TachyonCore` unit + live Rust server E2E test suite directly:

```sh
swift test --package-path ios/TachyonCore
```
