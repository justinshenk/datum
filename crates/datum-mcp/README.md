# datum-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server over the
`datum` CLI. An agent gets datum's verbs as tools and keeps its own timeline
of what it did, in the same database as the people it works with.

Every write carries the server's **actor** (`--actor`, set as
`DATUM_ACTOR`): the entry holds it as `extra_metadata.actor`, and its signed
op holds it in its origin, which `datum history` shows. So a timeline shared
by a person and their agents says which of them wrote each row.

The server runs the real `datum` binary, one process a call, and never opens
the database itself, so the CLI's lock, spool, migrations and exit codes stay
the one path every write takes.

## Tools

| Tool | What it runs |
|---|---|
| `datum_log` | `occur` / `start` / `stop` / `switch` / `note` on a field, with `data` key/values, a `time` (`-t`) and a `duration` |
| `datum_now` | `datum now` |
| `datum_tail` | `datum tail [FIELD] -n N --json` |
| `datum_read` | one read-only command and its words: `tail`, `head`, `now`, `get`, `grep`, `stats`, `fields`, `history`, `why`, `count`, `ids` |

A field, key or state that would read as a flag or as another key is refused
before anything runs, and `datum_read` refuses `--fix` and `--db`.

## Use

```bash
cargo build --release -p datum-cli -p datum-mcp

# Claude Code
claude mcp add datum -- /path/to/datum-mcp --actor agent:claude-code --db ~/agent.db
```

Options: `--db PATH` (else `DATUM_DB`), `--actor NAME` (else `DATUM_ACTOR`,
else `agent`), `--bin PATH` (else `DATUM_BIN`, else the `datum` beside
`datum-mcp`, else the one on `PATH`).

A person logging to the same database by hand can set `DATUM_ACTOR=human`
in their shell, so both sides say who they are.
