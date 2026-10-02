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

The server prints an access token when it starts. In the client, connect with
the token as the third argument:

```text
/connect 127.0.0.1 6969 <token>
```

Use `/nickname <name>` to change your nickname. See [PROTOCOL.md](PROTOCOL.md)
for the full line protocol.

## License
MIT
