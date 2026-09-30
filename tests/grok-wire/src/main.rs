// CI fetches the unmodified Grok production wire types at the documented revision.
#[path = "../vendor/messages.rs"]
#[allow(dead_code)]
mod messages;

use std::io::{self, BufRead};

fn main() {
    for line in io::stdin().lock().lines() {
        let line = line.expect("read client input");
        let value: serde_json::Value = serde_json::from_str(&line).expect("read check envelope");
        let result = if value["kind"] == "message" {
            serde_json::from_value::<messages::MessagesResponse>(value["data"].clone())
                .map(|_| ())
        } else {
            serde_json::from_value::<messages::MessageStreamEvent>(value["data"].clone())
                .map(|_| ())
        };
        match result {
            Ok(()) => println!("ok"),
            Err(error) => println!("{error}"),
        }
    }
}
