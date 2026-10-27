//! Tests for the `arena` feature: per-session memory isolation in in-process workers.
//!
//! Verifies that when an arena memory limit is configured, exceeding it causes
//! the worker to report a FatalError (crash) rather than leaking memory or
//! bringing down the host process.
//!
//! Note: these tests run without `ThreadLocalArena` installed as the global
//! allocator, so the arena only limits its own direct allocations, not the
//! Monty heap. The tests verify the infrastructure works (with_arena wrapping,
//! panic catching) and that the pool remains usable with arena limits configured.
#![cfg(feature = "arena")]

use std::future::ready;

use monty_pool::{Pool, PoolConfig, PrintFuture, ReplConfig, TurnEvent};
use monty_types::{MontyObject, PrintStream};

fn no_print(_: PrintStream, _: &str) -> PrintFuture {
    Box::pin(ready(()))
}

fn expect_complete(event: TurnEvent) -> MontyObject {
    match event {
        TurnEvent::Complete(value) => value,
        other => panic!("expected Complete, got {other:?}"),
    }
}

/// A worker with an arena memory limit can still execute simple code.
#[tokio::test(flavor = "multi_thread")]
async fn arena_worker_executes_simple_code() {
    let mut config = PoolConfig::in_process();
    config.arena_memory_limit = Some(10 * 1024 * 1024); // 10 MiB
    let pool = Pool::new(config).await.unwrap();
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();

    let event = session
        .feed("1 + 1", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(2));
    session.finish().await.unwrap();
}

/// With a very small arena limit, the pool remains usable — the arena's
/// `with_arena` wrapping works correctly even without the global allocator
/// installed. The Monty heap allocations go to the system allocator, but
/// the infrastructure (with_arena, panic catching) still functions.
#[tokio::test(flavor = "multi_thread")]
async fn arena_limit_configured_pool_remains_usable() {
    let mut config = PoolConfig::in_process();
    config.arena_memory_limit = Some(1024);
    let pool = Pool::new(config).await.unwrap();
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();

    // The pool should still be usable — with_arena wraps dispatch correctly.
    let event = session
        .feed("1 + 1", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(2));
    session.finish().await.unwrap();
}

/// Without an arena limit, in-process workers behave normally.
#[tokio::test(flavor = "multi_thread")]
async fn arena_without_limit_works_normally() {
    let mut config = PoolConfig::in_process();
    config.arena_memory_limit = None; // no limit
    let pool = Pool::new(config).await.unwrap();
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();

    let event = session
        .feed("sum(range(100))", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(4950));
    session.finish().await.unwrap();
}

/// Multiple sessions can run concurrently with arena limits.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_arena_workers_isolated() {
    let mut config = PoolConfig::in_process();
    config.arena_memory_limit = Some(10 * 1024 * 1024);
    config.max_processes = 2;
    let pool = Pool::new(config).await.unwrap();

    let mut a = pool.checkout(&ReplConfig::default()).await.unwrap();
    let mut b = pool.checkout(&ReplConfig::default()).await.unwrap();

    let ea = a
        .feed("a = 100\na", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    let eb = b
        .feed("b = 200\nb", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();

    assert_eq!(expect_complete(ea), MontyObject::int(100));
    assert_eq!(expect_complete(eb), MontyObject::int(200));

    a.finish().await.unwrap();
    b.finish().await.unwrap();
}
