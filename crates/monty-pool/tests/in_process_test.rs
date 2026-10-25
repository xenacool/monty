//! The in-process (thread) transport: same pool behaviour as subprocess workers,
//! with no binary and no process spawn.
#![cfg(feature = "in-process")]

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

#[tokio::test(flavor = "multi_thread")]
async fn feeds_share_session_state_and_workers_are_reused() {
    let pool = Pool::new(PoolConfig::in_process()).await.unwrap();
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();
    assert!(session.pid().is_none(), "thread workers have no OS pid");

    let event = session
        .feed("x = 40\nx + 2", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(42));
    let event = session
        .feed("x * 2", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(80));
    session.finish().await.unwrap();
    assert_eq!(pool.idle_workers(), 1);

    // the idle thread worker serves the next checkout with a fresh session
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();
    let event = session
        .feed("1 + 1", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(2));
    session.finish().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_checkouts_run_on_separate_threads() {
    let mut config = PoolConfig::in_process();
    config.max_processes = 2;
    let pool = Pool::new(config).await.unwrap();
    let mut a = pool.checkout(&ReplConfig::default()).await.unwrap();
    let mut b = pool.checkout(&ReplConfig::default()).await.unwrap();
    let ea = a.feed("a = 1\na", vec![], vec![], false, &mut no_print).await.unwrap();
    let eb = b.feed("b = 2\nb", vec![], vec![], false, &mut no_print).await.unwrap();
    assert_eq!(expect_complete(ea), MontyObject::int(1));
    assert_eq!(expect_complete(eb), MontyObject::int(2));
    a.finish().await.unwrap();
    b.finish().await.unwrap();
}
