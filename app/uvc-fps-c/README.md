# uvc-fps - UVC Camera FPS Testing Tool (C version)

C implementation of the UVC (USB Video Class) camera FPS testing tool.

## Dependencies

- **libuvc**: USB Video Class library
- **pthread**: POSIX threads

### Installing libuvc

```bash
# Ubuntu/Debian
sudo apt-get install libusb-1.0-0-dev

# Build and install libuvc
git clone https://github.com/libuvc/libuvc.git
cd libuvc
mkdir build && cd build
cmake .. -DCMAKE_INSTALL_PREFIX=/usr/local
make -j$(nproc)
sudo make install
sudo ldconfig
```

## Building

```bash
make
```

For cross-compilation to ARM64 (Orange Pi 5 Plus):

```bash
make aarch64
```

## Usage

```
uvc-fps [OPTIONS]

OPTIONS:
  --device <INDEX>        Zero-based UVC device index [default: 0]
  --format <FORMAT>       any, mjpeg, or yuyv [default: mjpeg]
  --width <PIXELS>        Frame width [default: 640]
  --height <PIXELS>       Frame height [default: 480]
  --fps <FPS>             Requested frame rate [default: 30]
  --auto-min-data         Select the lowest-data probeable mode
  --list-modes            Print all supported modes and exit
  --interval-sec <SECS>   Reporting interval [default: 1]
  --duration-sec <SECS>   Stop after this many seconds
  --max-frames <N>        Stop after at least N frames
  --save-dir <DIR>        Save frames to DIR
  --save-last             Save only the final frame
  --save-every <N>        Save every Nth frame [default: 1]
  --max-saved <N>         Stop saving after N frames
```

## Examples

```bash
# List all supported modes
./uvc-fps --list-modes

# Test with default settings (640x480 MJPEG @ 30fps)
./uvc-fps

# Test specific resolution and frame rate
./uvc-fps --width 1920 --height 1080 --fps 60

# Save frames to directory
./uvc-fps --save-dir /tmp/frames --save-every 10

# Run for 10 seconds and save only the last frame
./uvc-fps --duration-sec 10 --save-dir /tmp --save-last

# Auto-select minimum data mode
./uvc-fps --auto-min-data
```

## Comparison with Rust Version

| Feature | Rust Version | C Version |
|---------|--------------|-----------|
| Memory Safety | RAII, bounds checking | Manual management |
| Error Handling | Result<T, E> | errno, return codes |
| Concurrency | AtomicU64, Mutex | volatile, pthread_mutex_t |
| File Size | ~40KB | ~25KB |
| Dependencies | libc only | libuvc, pthread |

## Notes

- The C version maintains feature parity with the Rust version
- Thread-safe frame counting using pthread mutex
- Same UVC protocol flow and callback mechanism
- Compatible with Orange Pi 5 Plus USB cameras
