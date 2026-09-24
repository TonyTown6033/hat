# HAT

A small terminal chat client and server written in Rust.

## Build

```bash
cargo build --release
```

## Run

Start the server:

```bash
cargo run --bin server
```

Start the client in another terminal:

```bash
cargo run --bin client
```

The server prints an access token when it starts. Use that token with `/connect` in the client.

## License

This project is for personal and educational use.
