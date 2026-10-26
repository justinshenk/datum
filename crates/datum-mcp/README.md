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

## What it is for

datum is built for a person keeping an honest record of their own life. This
crate gives an agent the same habit, so its record can be read and checked.
It is meant to support three kinds of study:

- **Does an agent report what it did?** The agent logs its own work as it
  goes: what it started and stopped, the states it switched, what it noticed.
  Its tool calls or transcript are recorded separately. Comparing the two
  shows actions it did but never logged, and things it logged but never did.
  Run the same tasks with and without a reason to misreport (a test it is
  tempted to skip, a mistake it is told is penalized, a task that cannot be
  done) and the rate of misreporting becomes something you can measure, per
  model and per prompt.
- **Who did what on shared work?** A person and their agents log to one
  database, each with their own actor. The timeline shows how work moves
  between them, how much the agent did, and where a person stepped in,
  without the person having to log the agent's half.
- **Long-running agents.** A tracer records one run. datum keeps a record
  over days and weeks, and with sync each entry is copied to an overseer's
  device as it is made.

## Example

Start the server for an agent, with its own database:

```bash
claude mcp add datum -- datum-mcp --actor agent:claude-code --db ~/agent.db
```

Asked to fix a failing test, the agent might call:

```json
{"command": "start",  "field": "task",   "data": {"goal": "fix auth test"}}
{"command": "switch", "field": "phase",  "state": "investigating"}
{"command": "occur",  "field": "finding", "data": {"note": "token expiry off by one"}}
{"command": "switch", "field": "phase",  "state": "testing"}
{"command": "occur",  "field": "test_run", "data": {"passed": true}}
{"command": "stop",   "field": "task"}
```

The person working with it logs from their shell under their own name:

```bash
export DATUM_ACTOR=human DATUM_DB=~/agent.db
datum occur review verdict=approved
```

Then read it back:

```bash
datum tail -n 20          # the timeline, newest last
datum dayview -o day.svg  # the day as a picture
datum get <hid> --show all  # one entry, extra_metadata.actor included
datum tail --json | jq '.rows[] | {field, time, actor}'
datum history <hid>       # one entry's versions, each with its actor
```

To check the agent's account, take its tool calls from the client's logs (an
Inspect `.eval` file, Claude Code's transcript) and line them up against the
rows with `actor = agent:claude-code` on time.

## Tools

| Tool | What it runs |
|---|---|
| `datum_log` | `occur` / `start` / `stop` / `switch` / `note` on a field, with `data` key/values, a `time` (`-t`) and a `duration` |
| `datum_now` | `datum now` |
| `datum_tail` | `datum tail [FIELD] -n N --json`; each row names its `actor` |
| `datum_read` | one read-only command and its words: `tail`, `head`, `now`, `get`, `grep`, `stats`, `fields`, `history`, `why`, `count`, `ids` |

A field or key that would read as a flag, or a data key datum reads as a
column (`field`, `state`, `occur_time`, …), is refused before anything runs. A
`switch` state goes after a bare `--`, so it is always read as typed. A
`note` takes its words as `text`, kept as `comment`. `datum_read` refuses
`--fix`, `--db` and the watch flags (`-w`, `--watch`), which never return.

Each `datum` run is stopped after `--timeout` seconds (30 by default), and a
result longer than 64 KB is cut, saying so.

## Known limits

- **The actor is the creator's.** An entry keeps the actor that logged it.
  A later `update`, `retime` or `edit`, or a re-log that matches it, leaves
  `extra_metadata.actor` as it was; who made each later change is in that
  change's origin, which `datum history` lists.
- **Two copies, two actors, one instant.** When two synced copies log the
  same field at the same instant, sync merges them into one entry
  (da-vkeg0). If the two were logged by different actors, the merged entry
  shows a conflict on `extra_metadata` to resolve, since the fold compares
  that object whole. Treating the actor as provenance there (beside
  `create_time`) would be a change to the fold rule, and is left for the
  maintainer to decide.

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
