# Event output

The coordinator [run](src/lib.rs#L12) passes values to [the writer](src/events.rs).
The protocol belongs to `events::emit`; scheduling belongs to `run`.

## Design rationale

Keep encoding independent from lifecycle management so a caller can choose when to emit.
