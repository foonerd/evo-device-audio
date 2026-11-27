// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Send a file of raw AAC-LC frames to an AirPlay 2 receiver, as a sender
//! would, and report each step. Test tooling for bench work against a real
//! receiver; the frames file is length-prefixed (a big-endian `u32`, then the
//! frame), as in `tests/data/tone_lr.frames`.
//!
//! ```text
//! airplay_sender HOST PORT FRAMES [--password PW] [--pause] [--seek]
//! ```
//!
//! `--pause` pauses two seconds in for three seconds, then resumes; `--seek`
//! flushes to the middle of the stream two seconds in.

use std::net::ToSocketAddrs;
use std::time::Duration;

use evo_airplay::sender::{parse_frames, Reply, Sender};

fn step(name: &str, reply: &Reply) -> std::io::Result<()> {
    println!("{name:<28} {}", reply.status);
    if reply.is_ok() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("{name} failed")))
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: airplay_sender HOST PORT FRAMES [--password PW] [--pause] [--seek]"
        );
        std::process::exit(2);
    }
    let flag = |name: &str| args.iter().any(|a| a == name);
    let password = args
        .iter()
        .position(|a| a == "--password")
        .and_then(|i| args.get(i + 1))
        .map_or("3939", String::as_str);
    let addr = format!("{}:{}", args[1], args[2])
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("no address"))?;
    let raw = std::fs::read(&args[3])?;
    let frames = parse_frames(&raw);

    let mut sender = Sender::connect(addr, password).await?;
    println!("{:<28} paired", "pair-setup");
    sender.fp_setup().await?;
    step("SETUP 1", &sender.setup_event().await?)?;
    let key = [0x5au8; 32];
    let (reply, ports) = sender.setup_stream(103, &key).await?;
    step("SETUP 2 (buffered)", &reply)?;
    let ports =
        ports.ok_or_else(|| std::io::Error::other("no stream ports"))?;
    step("RECORD", &sender.record().await?)?;
    let mut data = sender.open_data(ports, &key).await?;
    for frame in &frames {
        data.send_frame(frame).await?;
    }
    println!("{:<28} {} frames", "sent", frames.len());
    step("SETRATEANCHORTIME 1", &sender.set_rate(1, 1_000_000).await?)?;

    let seconds = frames.len() as f64 * 1024.0 / 44_100.0;
    if flag("--pause") {
        tokio::time::sleep(Duration::from_secs(2)).await;
        step("pause", &sender.set_rate(0, 1_000_000).await?)?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        step("resume", &sender.set_rate(1, 1_000_000).await?)?;
        tokio::time::sleep(Duration::from_secs_f64(seconds - 2.0 + 1.5)).await;
    } else if flag("--seek") {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let half = u64::from(data.next_seq()) / 2;
        step("FLUSHBUFFERED", &sender.flush_until(half).await?)?;
        tokio::time::sleep(Duration::from_secs_f64(seconds / 2.0 + 1.5)).await;
    } else {
        tokio::time::sleep(Duration::from_secs_f64(seconds + 1.5)).await;
    }
    step("TEARDOWN", &sender.teardown().await?)
}
