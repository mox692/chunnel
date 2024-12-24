use async_concurrent_primitives::mpmc::bounded;
use std::time::Instant;

fn sync_api() {
    let num_threads = 5;
    let num_task = 10;
    let msg_per_task = 1_000;

    let (tx, rx) = bounded::bounded::<i32, 10_000_000>();
    // let (tx, rx) = crossbeam_channel::bounded(10_000_000);
    // let (tx, rx) = async_channel::bounded(100_0000);
    // let (tx, rx) = tokio::sync::mpsc::channel(100_000);

    let mut jhs = vec![];

    let start = Instant::now();

    for _ in 0..num_threads {
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

    let mut i = 0;
    loop {
        if let Ok(_) = rx.try_recv() {
            i += 1;
        }
        if num_threads * num_task * msg_per_task <= i {
            break;
        }
    }

    for j in jhs {
        j.join().unwrap();
    }

    let time = start.elapsed();
    println!("time: {:?}", &time);
}

fn main() {
    sync_api();
}
