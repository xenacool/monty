//! The in-process (thread) transport: same pool behaviour as subprocess workers,
//! with no binary and no process spawn.
#![cfg(feature = "in-process")]

use std::future::ready;

use monty_pool::{Pool, PoolConfig, PoolError, PrintFuture, ReplConfig, TurnEvent};
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

/// A worker survives a resource error (recursion limit) and continues serving.
#[tokio::test(flavor = "multi_thread")]
async fn worker_survives_resource_error() {
    let pool = Pool::new(PoolConfig::in_process()).await.unwrap();
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();

    // Deep recursion raises ResourceError, not a panic — the worker survives
    let err = session
        .feed("def f(): f()\nf()", vec![], vec![], false, &mut no_print)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PoolError::Runtime(_)),
        "expected Runtime error from recursion limit, got {err:?}"
    );

    // The worker should still be alive and able to serve requests
    let event = session
        .feed("1 + 1", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(2));
    session.finish().await.unwrap();
}

/// Multiple feeds in the same session share state across requests.
#[tokio::test(flavor = "multi_thread")]
async fn session_state_persists_across_feeds() {
    let pool = Pool::new(PoolConfig::in_process()).await.unwrap();
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();

    session
        .feed("x = 10", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    session
        .feed("y = 20", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();

    let event = session
        .feed("x + y", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(30));

    // Functions defined in one feed are available in the next
    session
        .feed("def double(n): return n * 2", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    let event = session
        .feed("double(21)", vec![], vec![], false, &mut no_print)
        .await
        .unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(42));

    session.finish().await.unwrap();
}

/// Print output from in-process workers is delivered to the callback.
#[tokio::test(flavor = "multi_thread")]
async fn print_output_is_delivered() {
    use std::sync::{Arc, Mutex};

    let got_print: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let got_print_clone = got_print.clone();

    let mut session = Pool::new(PoolConfig::in_process())
        .await
        .unwrap()
        .checkout(&ReplConfig::default())
        .await
        .unwrap();

    let mut print_callback = move |_stream: PrintStream, _text: &str| {
        let flag = got_print_clone.clone();
        let fut = async move {
            *flag.lock().unwrap() = true;
        };
        Box::pin(fut) as PrintFuture
    };

    session
        .feed("print('hello')", vec![], vec![], false, &mut print_callback)
        .await
        .unwrap();

    assert!(*got_print.lock().unwrap(), "print callback was not invoked");
    session.finish().await.unwrap();
}

/// Worker is returned to the pool and reused for the next checkout.
#[tokio::test(flavor = "multi_thread")]
async fn worker_returned_to_pool() {
    let pool = Pool::new(PoolConfig::in_process()).await.unwrap();
    assert_eq!(pool.idle_workers(), 1);

    {
        let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();
        session.feed("42", vec![], vec![], false, &mut no_print).await.unwrap();
        session.finish().await.unwrap();
    }

    assert_eq!(pool.idle_workers(), 1, "worker should be returned to pool");

    // Reuse the same worker
    let mut session = pool.checkout(&ReplConfig::default()).await.unwrap();
    let event = session.feed("100", vec![], vec![], false, &mut no_print).await.unwrap();
    assert_eq!(expect_complete(event), MontyObject::int(100));
    session.finish().await.unwrap();
}

/// Multiple workers can serve requests concurrently without interference.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_workers_isolated() {
    let mut config = PoolConfig::in_process();
    config.max_processes = 3;
    let pool = Pool::new(config).await.unwrap();

    let mut a = pool.checkout(&ReplConfig::default()).await.unwrap();
    let mut b = pool.checkout(&ReplConfig::default()).await.unwrap();
    let mut c = pool.checkout(&ReplConfig::default()).await.unwrap();

    // Each session sets and reads its own variable — no cross-contamination
    a.feed("val = 100", vec![], vec![], false, &mut no_print).await.unwrap();
    b.feed("val = 200", vec![], vec![], false, &mut no_print).await.unwrap();
    c.feed("val = 300", vec![], vec![], false, &mut no_print).await.unwrap();

    let ea = a.feed("val", vec![], vec![], false, &mut no_print).await.unwrap();
    let eb = b.feed("val", vec![], vec![], false, &mut no_print).await.unwrap();
    let ec = c.feed("val", vec![], vec![], false, &mut no_print).await.unwrap();

    assert_eq!(expect_complete(ea), MontyObject::int(100));
    assert_eq!(expect_complete(eb), MontyObject::int(200));
    assert_eq!(expect_complete(ec), MontyObject::int(300));

    a.finish().await.unwrap();
    b.finish().await.unwrap();
    c.finish().await.unwrap();
}
