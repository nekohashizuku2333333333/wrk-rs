# wrk-rs - HTTP benchmarking tool (Rust rewrite)

A Rust rewrite of `wrk` with TLS + HTTP/2 support and real-time traffic updates.

## Build

```sh
cargo build --release
```

## Basic usage

```sh
cargo run --release -- -t12 -c400 -d30s https://127.0.0.1:8443/index.html
```

Real-time traffic updates print every 5 seconds during the run.

## Command line options

- `-c, --connections` total number of HTTP connections to keep open
- `-d, --duration` duration of the test, e.g. `2s`, `2m`, `2h`
- `-t, --threads` total number of threads to use (defaults to CPU count)
- `-H, --header` HTTP header to add to request, e.g. `"User-Agent: wrk"` (repeatable)
- `--latency` print detailed latency statistics
- `--timeout` record a timeout if a response is not received within this amount of time
- `--http2` request HTTP/2 (HTTPS only; negotiates via ALPN)

## Notes

- LuaJIT scripting has been removed in this rewrite.
- HTTP/2 support requires HTTPS. h2c (cleartext HTTP/2) is not supported.

