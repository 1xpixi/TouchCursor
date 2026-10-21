# TouchCursor

TouchCursor is a performance-focused minimal mouse controller that allows you to smoothly control your computer's mouse from your phone's browser, no app required.

It pairs a native Windows host app with a touch-optimized web trackpad served over your local network.

<p align="center">
  <img src="assets/screenshot.png" alt="TouchCursor desktop window with phone address and pairing PIN redacted" width="380" />
  <br />
  <sub>Phone address and pairing PIN redacted.</sub>
</p>

## Quick Start

### Portable Windows build

Download the **TouchCursor-windows-x64** artifact from a successful [Windows build](https://github.com/1xpixi/TouchCursor/actions/workflows/windows.yml) (GitHub sign-in required). Extract it, then extract `TouchCursor-windows-x64.zip` and run `touchcursor.exe` or `start.bat`. Rust is not required.

Tagged [releases](https://github.com/1xpixi/TouchCursor/releases) include the same ZIP and its SHA-256 checksum. Windows may warn about the unsigned executable.

### Build from source

Requires Windows x64, [Rust](https://www.rust-lang.org/) with the MSVC toolchain, and the Windows SDK resource compiler (`rc.exe`).

Double-click `start.bat`, or run:

```powershell
cargo run --release --locked
```

The source launcher rebuilds before starting, so changes are picked up. If the app is already running, choose **Quit app** from the tray before rebuilding.

Closing the window keeps TouchCursor running in the system tray. To exit, right-click the tray icon and select **Quit app**.

## Pairing

1. Join the same Wi-Fi network on your phone and PC.
2. Open the address shown in the desktop window (for example, `http://192.168.1.15:8765`).
3. Enter the 4-digit PIN.

The browser saves an authorization token on first connection, letting you reconnect later without entering the PIN again.

If your phone goes offline, control stops and the page shows a connection notice. Restore Wi-Fi and tap **Reconnect**; your saved pairing is kept.

For firewall and Wi-Fi checks, open **Can't connect?** below the pairing form.

## Gestures

- **One finger**: Drag to move the cursor; tap for left-click, double-tap for double-click, or press and hold for right-click.
- **Two fingers**: Drag vertically to scroll.
- **Click bar**: Tap the bottom buttons for direct left and right clicks.
- **Settings**: Pointer speed (saved in this browser), gesture hints, and disconnect.

## Input & Networking

- Touch inputs stream over a persistent WebSocket rather than individual HTTP requests.
- Motion updates sample once per display frame to align with your device's refresh rate.
- Congested networks drop unsent motion frames instead of buffering stale cursor events.

## Security

Traffic travels over unencrypted HTTP and WebSockets for local network use. Only run TouchCursor on trusted private networks, and never forward port 8765 to the internet.
