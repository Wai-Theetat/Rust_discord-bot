# Rust Discord Bot

A Discord bot written in Rust using [serenity](https://github.com/serenity-rs/serenity) and [tokio](https://tokio.rs). Besides a few utility commands, it includes a **multithreaded prime-factorization engine** (Miller–Rabin + parallel Pollard's rho) and a command that benchmarks the engine with different thread counts.

## Highlights

- **Slash commands** registered for your server on startup
- **Fast u64 factorization**: deterministic Miller–Rabin primality test plus Pollard's rho, run on up to 4 threads that cancel each other once one finds a factor
- **Benchmark command** that compares 1, 2 and 4 worker threads and reports the speedup
- **Non-blocking**: CPU-heavy work runs in `tokio::task::spawn_blocking`, so the bot stays responsive
- **Safe cleanup**: confirm/cancel buttons that only the person who ran the command can press; deletes only the bot's own messages
- **Mention-safe replies**: user text never pings `@everyone`, roles or users
- **31 unit tests** covering the math and button parsing, with code review and test design assisted by Claude
- **Docker** multi-stage build with dependency caching

## Commands

| Command | Description |
| --- | --- |
| `/ping` | Replies with a friendly message. |
| `/echo text:<text>` | Repeats your text (up to 2000 characters). |
| `/roll dice:<MdN>` | Rolls dice such as `2d6` or `d20` (1–100 dice, 2–20 faces) and shows each roll and the total. |
| `/factor n:<integer>` | Prime-factorizes any integer from 2 up to `u64::MAX`, e.g. `Factor of 1024 is 2¹⁰`. |
| `/benchmark n:<integer> runs:<1-5>` | Times the factorization of `n` with 1, 2 and 4 threads and shows average time and speedup. |
| `/cleanup amount:<1-500>` | Scans the last `amount` messages and deletes the bot's own, after you confirm with a button. Hidden by default from members without **Manage Messages**. |

`/benchmark` replies with a table like this (1 thread is the baseline):

```text
Workers   Average       Speedup
1          <time> ms      1.00x
2          <time> ms     <x.xx>x
4          <time> ms     <x.xx>x
```

## How factorization works

```text
n ──► strip small primes (2…37)
   └► split(n):
        prime?  (Miller–Rabin, 7 fixed witnesses: exact for all u64)  ──► done
        perfect square?  ──► split(√n) twice
        otherwise ──► Pollard's rho on N threads, each with a different (seed, c)
                      first thread to find a factor d cancels the others
                      ──► split(d), split(n / d)
```

- Modular arithmetic uses `u128` intermediates, so `a * b mod m` never overflows.
- Pollard's rho uses Floyd's cycle detection with an iteration budget. If a round fails, it retries with fresh parameters (up to 4 rounds) and then returns an error instead of hanging.

## Requirements

- [Docker](https://www.docker.com/) (recommended), **or**
- [Rust](https://www.rust-lang.org/tools/install) 1.85+ (the crate uses edition 2024)
- A Discord bot token and your server (guild) ID

## Discord setup

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications) and create an application.
2. In **Bot**, copy the bot token.
3. In **OAuth2 → URL Generator**, select:
   - Scopes: `bot`, `applications.commands`
   - Permissions: `View Channels`, `Send Messages`, `Read Message History`
   - Optional: `Manage Messages`, which lets `/cleanup` bulk-delete instead of deleting one by one
4. Open the generated URL to invite the bot.
5. Turn on Developer Mode in Discord, right-click your server, and choose **Copy Server ID**.

## Configuration

Create a `.env` file in the project root (it is git-ignored):

```env
DISCORD_TOKEN=your_bot_token
GUILD_ID=your_server_id
```

## Run with Docker

```bash
git clone https://github.com/Wai-Theetat/Rust-Practice_discord-bot.git
cd Rust-Practice_discord-bot
docker compose up -d --build
docker compose logs -f bot
```

You should see:

```text
<Bot Name> is connected!
Registered 6 commands
```

Then refresh Discord (`Ctrl+R`) and type `/`.

Other useful commands: `docker compose restart bot`, `docker compose down`, `docker compose ps`.

## Run locally

```bash
export DISCORD_TOKEN=your_bot_token
export GUILD_ID=your_server_id
cargo run --release
```

On PowerShell use `$env:DISCORD_TOKEN="..."` and `$env:GUILD_ID="..."` instead.

Run the tests:

```bash
cargo test
```

## Project structure

```text
src/
├── main.rs              # reads env vars, starts the client
├── handler.rs           # ready event (registers commands) + interaction routing
├── commands/
│   ├── mod.rs           # command list and dispatcher
│   ├── ping.rs
│   ├── echo.rs
│   ├── roll.rs
│   ├── factor.rs
│   ├── benchmark.rs
│   └── cleanup.rs       # confirm/cancel buttons, bulk delete with fallback
└── math/
    ├── gcd.rs
    ├── modular_arithmetic.rs   # add_mod, mul_mod, pow_mod (u128-safe)
    ├── miller_rabin.rs         # deterministic primality test
    ├── pollard_rho.rs          # Floyd cycle, cancellable
    ├── factorization.rs        # parallel driver, error types, tests
    └── formatting.rs           # 2¹⁰ × 3 style output
```

## Troubleshooting

| Problem | Fix |
| --- | --- |
| `Token Not Found!` | Make sure `.env` exists and has `DISCORD_TOKEN`. |
| `GUILD_ID Not Found!` / `GUILD_ID must be a number` | Add a valid numeric `GUILD_ID` to `.env`. |
| Commands don't appear | Refresh Discord (`Ctrl+R`) and check the Guild ID. |
| Can't see `/cleanup` | It is hidden from members without Manage Messages. A server admin can change this under **Server Settings → Integrations**. |
| `/cleanup` deletes slowly | Give the bot **Manage Messages** so it can bulk-delete. Messages older than 14 days are always deleted one at a time (Discord limit). |

## Development workflow

I wrote the bot and the factorization engine myself. I used [Claude](https://claude.ai) (Anthropic's AI assistant) as a reviewer and testing partner, and for writing commit messages:

- **Code review**: Claude reviewed the code for bugs, edge cases and Rust idioms, and I applied the fixes I agreed with (see the *Apply code review fixes* commit).
- **Testing**: Claude helped design the unit tests, such as known factorizations, `u64::MAX`, large semiprimes and prime squares, a sweep of every `n` below 20,000, and button ID parsing. It also helped me run and check `cargo test`.
- **Commit messages**: Claude helped me write clear, descriptive commit messages and readme.

## Security

Keep your token private and never commit `.env`. If the token leaks, regenerate it in the Developer Portal.
