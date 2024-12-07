use std::time::Instant;

use async_concurrent_primitives::mpmc::bounded;

fn sync_api() {
    let num_threads = 100;
    let num_task = 50;
    let msg_per_task = 100;

    let (tx, rx) = bounded::bounded::<i32, 1_000_000>();
    // let (tx, rx) = async_channel::bounded(1_000_000);
    // let (tx, rx) = tokio::sync::mpsc::channel(1_000_000);

    let mut jhs = vec![];

    for i in 0..num_threads {
        let tx = tx.clone();
        let jh = std::thread::spawn(move || {
            for _ in 0..num_task {
                let tx = tx.clone();
                for i in 0..msg_per_task {
                    tx.try_send(i).unwrap();
                }
            }
        });

        jhs.push(jh);
    }

    for _ in 0..msg_per_task * num_task {
        let _ = rx.try_recv();
    }

    for j in jhs {
        j.join().unwrap();
    }
}

fn main() {
    let start = Instant::now();
    sync_api();

    let time = start.elapsed();
    println!("time: {:?}", &time);
}
