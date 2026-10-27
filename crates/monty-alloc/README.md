# monty-alloc

Memory allocators for [Monty](https://github.com/pydantic/monty) workers: a
process-level global allocator and a per-session bump arena for platforms that
cannot spawn subprocesses.

## Global allocator (`LimitedAllocator`)

The default mode: a `#[global_allocator]` that exposes live-byte usage to the
interpreter and ends the process if the worker's hard memory ceiling or system
allocator refuses an allocation.

Monty executes untrusted Python, so a host has to be able to cap what a session
may allocate. Enforcing that in the allocator catches every byte the worker asks
for, wherever in the process it is asked for. Counting rather than asking the
kernel for `RLIMIT_AS` is what makes the limit portable and its units
meaningful: it bounds bytes the process requested, not virtual address space, so
mapped text, thread stacks and file mappings do not consume the budget. The
tradeoff is that it binds only what reaches this allocator — everything
sandboxed code can allocate, but not a direct `mmap`.

```rust
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

// The caller chooses headroom above the interpreter's soft limit.
let hard_budget = monty_types::memory_limit_with_headroom(Some(8 * 1024 * 1024), false);
monty_alloc::set_hard_limit(hard_budget).unwrap();
```

The interpreter compares live usage with the session's soft limit at execution
checkpoints and raises `MemoryError` after crossing it. The caller gives this
crate a higher hard budget, leaving room for exception machinery and allocations
between checkpoints; this crate adds the worker baseline and ends the worker if
that ceiling is crossed. `None` lifts the hard ceiling. See
`limitations/resource_limits.md` for how each outcome surfaces to a host.

### Ending the process

Exceeding the hard limit cannot raise a Python exception — it happens below the
interpreter — so the worker dies and its host replaces it. Neither a panic
(whose machinery allocates) nor a plain abort will do: `SIGABRT` is also what a
stack overflow produces, and a host that cannot tell those apart cannot report
`MemoryError`.

The `exit-code` feature picks how the process ends:

- **on** — `process::exit(monty_types::OOM_EXIT_CODE)`, the dedicated status a
  parent reads to classify the death. Used by the `monty subprocess` worker,
  whose parent is [`monty-pool`](https://crates.io/crates/monty-pool).
- **off** (default) — `process::abort()`, which on wasm is a trap. A wasm
  module has no exit status to offer, and its host already treats a turn that
  ends without a terminating event as a dead instance.

Only a binary or a wasm module may declare a `#[global_allocator]`: in a native
cdylib it would hijack the allocator of the embedding host process.

## Per-session arena (`arena` feature)

For platforms that cannot spawn subprocesses (iOS, Android, game cartridges),
the `arena` feature provides per-session memory isolation. Each session gets a
[`monty_alloc::arena::Arena`] with a hard memory ceiling enforced by
[bumpalo's built-in allocation limit](bumpalo::Bump::set_allocation_limit);
exceeding the limit panics, which the in-process worker thread catches as a
`FatalError` — the session dies, the host survives.

The limit is enforced at the allocator level — no polling or `LIVE_MEMORY`
tracking is needed. For the arena to limit **all** memory (including the Monty
interpreter's heap), the host binary **must** install
[`monty_alloc::arena::ThreadLocalArena`] as the `#[global_allocator]`:

```rust,ignore
// In the host binary (not a library):
#[global_allocator]
static ALLOC: monty_alloc::arena::ThreadLocalArena = monty_alloc::arena::ThreadLocalArena;

// In monty-pool, configure the arena limit:
let mut config = monty_pool::PoolConfig::in_process();
config.arena_memory_limit = Some(10 * 1024 * 1024); // 10 MiB per session
```

Without the global allocator installation, the arena only limits its own direct
allocations, not the Monty heap — the interpreter's soft `ResourceTracker`
limit still works as a backup.

### Requiring ThreadLocalArena (`require-tla` feature)

By default, when `ThreadLocalArena` is installed as the global allocator but no
arena is active on a thread (i.e. `with_arena` was not called), the first
fallback allocation emits a one-time warning to stderr and uses the system
allocator. Subsequent fallbacks on the same thread are silent.

Enable the `require-tla` feature to make this misconfiguration panic instead.
The in-process worker's `catch_unwind` catches the panic and reports a
`FatalError`, so the host survives. This is the safer default for production
use where untrusted code must not silently bypass memory limits:

```toml
[dependencies]
monty-alloc = { version = "...", features = ["require-tla"] }
```

`require-tla` implies `arena`. Enable it on `monty-pool` the same way:

```toml
[dependencies]
monty-pool = { version = "...", features = ["arena"] }
```

Enable with `features = ["arena"]` on `monty-alloc` and `features = ["arena"]`
on `monty-pool`. Add `require-tla` to `monty-alloc` features to enforce the
global allocator at runtime.

## Only crates in this workspace

Published so the `monty` binary can be, not for direct use. On a 32-bit target
a limit near 4 GiB saturates the arithmetic and leaves the worker uncapped.
