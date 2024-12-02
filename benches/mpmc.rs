use std::time::Instant;

use async_concurrent_primitives::mpmc::bounded;

use criterion::measurement::WallTime;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkGroup, Criterion};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(6)
        .build()
        .unwrap()
}

fn single_bounded_try_api(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("single_bounded_try_api", |b| {
        b.iter(|| {
            let (tx, rx) = bounded::bounded::<i32, 1_000_000>();

            for _ in 0..5 {
                let tx = tx.clone();
                for i in 0..1000 {
                    tx.try_send(i).unwrap();
                }
            }

            for _ in 0..1_000 * 5 {
                let _ = rx.try_recv().unwrap();
            }
        })
    });
}

fn single_bounded_async_channel_try_api(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("single_bounded_async_channel_try_api", |b| {
        b.iter(|| {
            let (tx, rx) = async_channel::bounded(1_000_000);

            for _ in 0..5 {
                let tx = tx.clone();
                for i in 0..1000 {
                    tx.try_send(i).unwrap();
                }
            }

            for _ in 0..1_000 * 5 {
                let _ = rx.try_recv().unwrap();
            }
        })
    });
}

fn single_bounded_tokio_mpsc_try_api(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("single_bounded_tokio_mpsc_try_api", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1_000_000);

            for _ in 0..5 {
                let tx = tx.clone();
                for i in 0..1000 {
                    tx.try_send(i).unwrap();
                }
            }

            for _ in 0..1_000 * 5 {
                let _ = rx.try_recv();
            }
        })
    });
}

fn multi_bounded_try_api(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("multi_bounded_try_api", |b| {
        b.iter_custom(|iter| {
            let start = Instant::now();
            for _ in 0..iter {
                let (tx, rx) = bounded::bounded::<i32, 1_000_000>();
                let jh = std::thread::spawn(move || {
                    for _ in 0..5 {
                        let tx = tx.clone();
                        for i in 0..1000 {
                            tx.try_send(i).unwrap();
                        }
                    }
                });

                for _ in 0..1_000 * 5 {
                    let _ = rx.try_recv();
                }

                jh.join().unwrap();
            }
            start.elapsed()
        })
    });
}

fn multi_bounded_async_channel_try_api(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("multi_bounded_async_channel_try_api", |b| {
        b.iter_custom(|iter| {
            let start = Instant::now();
            for _ in 0..iter {
                let (tx, rx) = async_channel::bounded(1_000_000);
                let jh = std::thread::spawn(move || {
                    for _ in 0..5 {
                        let tx = tx.clone();
                        for i in 0..1000 {
                            tx.try_send(i).unwrap();
                        }
                    }
                });

                for _ in 0..1_000 * 5 {
                    let _ = rx.try_recv();
                }

                jh.join().unwrap();
            }
            start.elapsed()
        })
    });
}
fn multi_bounded_tokio_mpsc_try_api(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("multi_bounded_tokio_mpsc_try_api", |b| {
        b.iter_custom(|iter| {
            let start = Instant::now();
            for _ in 0..iter {
                let (tx, mut rx) = tokio::sync::mpsc::channel(1_000_000);
                let jh = std::thread::spawn(move || {
                    for _ in 0..5 {
                        let tx = tx.clone();
                        for i in 0..1000 {
                            tx.try_send(i).unwrap();
                        }
                    }
                });

                for _ in 0..1_000 * 5 {
                    let _ = rx.try_recv();
                }

                jh.join().unwrap();
            }
            start.elapsed()
        })
    });
}

fn bench_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("single");
    single_bounded_try_api(&mut group);
    single_bounded_async_channel_try_api(&mut group);
    single_bounded_tokio_mpsc_try_api(&mut group);

    multi_bounded_try_api(&mut group);
    multi_bounded_async_channel_try_api(&mut group);
    multi_bounded_tokio_mpsc_try_api(&mut group);
    group.finish();
}

criterion_group!(contention, bench_single);

criterion_main!(contention);
