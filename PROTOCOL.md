# HAT line protocol

The server speaks a small, line-based text protocol over TCP. Every frame is a
single line terminated by `\n` (a trailing `\r` is accepted and stripped).

## Connecting and authentication

1. The client opens a TCP connection.
2. The server sends `SYS authenticating`.
3. The client sends the access token printed by the server as its first line.
4. On success the server sends `SYS connected; type /help to see available commands`.
   On failure it sends `ERR invalid access token` and closes the connection.

The token line is compared exactly (after trailing newline removal). A wrong or
missing token ends the connection.

## Client to server

| Command | Meaning |
| --- | --- |
| `MSG <text>` | Send a chat message. `<text>` must not be empty. |
| `NICK <name>` | Request a nickname change. |

Any other line is answered with `ERR unknown protocol command`. Blank lines are
ignored. Lines longer than 4 KiB are rejected with `ERR message is too long`.

### Nickname rules

- 1 to 16 characters.
- ASCII letters, digits, `_` and `-` only.
- Case-insensitive uniqueness: a nickname in use by another client is rejected.

On success the server replies with `YOU <name>` and tells everyone else
`NICK <old> <new>`. On failure it replies with `ERR <reason>`.

## Server to client

| Message | Meaning |
| --- | --- |
| `SYS <text>` | Informational/server event (join, leave, welcome). |
| `MSG <nick> <text>` | A chat message from `<nick>`. |
| `NICK <old> <new>` | Another client changed nickname. |
| `YOU <nick>` | The recipient's own (possibly new) nickname. |
| `ERR <reason>` | A rejected request or rate-limit notice. |

Nicknames are assigned as `user-<port>` on connect until changed.

## Rate limiting and bans

- At most one accepted message per 250 ms per client.
- Messages sent faster get `ERR you are sending messages too quickly` and count
  as a violation. The violation counter resets after an accepted message.
- Reaching 10 violations bans the client's IP for 10 minutes with
  `ERR too many rapid messages; you are banned for 10 minutes`, then the
  connection is closed.
- A connection from a banned IP receives
  `ERR temporarily banned; try again in <seconds> seconds` and is closed.

## Out of scope

This document covers only the stable text protocol migrated to `main`. File
transfer (`PUT`/`GET`/`LS`), remote execution (`EXEC`) and LLM (`LLM`) commands
are not part of this protocol on `main`.
