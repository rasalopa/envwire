# envwire

Find the environment variables your services disagree about.

## Why

A `.env` file is never alone. There is the one you run with, the one the project
promises a newcomer in `.env.example`, and the one Compose actually hands to each
container. They drift apart quietly, and you find out at runtime.

The one that costs the most hours looks like this:

```
REDIS_HOST=localhost
```

Correct on your machine, wrong inside a container, because there `localhost` is the
container itself. Nothing in the file is misspelled, so no linter says a word. You
get a connection refused and start reading logs.

envwire reads every place a project states its environment and reports where they
disagree, with the file and line to open.

## Status

Early. It is being built in the open, one small piece at a time.

What works today:

- Finds the env sources in a project: `.env`, `.env.local`, `.env.example`,
  `.env.sample`, `.env.template`, and the Compose file.
- Reads `.env`-shaped files: quoting, escapes, `export`, values that run across
  several lines, and comments. Lines it cannot read are reported, not dropped.

What does not exist yet: the Compose reader, and every check. `envwire check` runs
and exits zero because there is nothing for it to say. It will not tell you your
project is fine — it will tell you it has not looked.

## What it will check

- A variable `.env.example` promises and your `.env` does not have, and the reverse.
- A value that differs between `.env` and the Compose file.
- `localhost` in a service that runs inside a container.
- A secret too short to be one.
- A `.env` tracked by git.
- A variable declared and never used.

## Build

```sh
cargo build --release
```

The binary lands in `target/release/envwire`. Rust 1.85 or newer.

There is no release yet. Packages will come once the checks do.

## Contributing

Issues and pull requests are welcome, including small ones. If you have a project
where env configuration goes wrong in a way envwire would miss, that is worth an
issue on its own — the checks are only as good as the cases they were built from.

Before a pull request:

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## License

MIT
