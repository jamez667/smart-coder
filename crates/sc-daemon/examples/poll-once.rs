//! Drive the real transport against a real server, without a model.
//!
//! ```text
//! cargo run -p sc-daemon --example poll-once -- <base-url> <api-key> [spec-text]
//! ```
//!
//! **Why this exists.** `queue serve` needs a local model before it will do
//! anything — the preflight refuses at the terminal rather than at 3am on the
//! first work item, which is right. But that makes the *transport* untestable
//! whenever the model is down: poll, claim, and push-back never run, and those
//! are the three steps that talk to the hosted server.
//!
//! This does one round trip through [`HttpTransport`] — the same type
//! `queue serve` uses, not a copy — and supplies the spec text itself instead of
//! drafting one. So it proves everything about the chain except the drafting,
//! and it proves it against the real container rather than a fake.
//!
//! Nothing here writes to a repository. It is a probe, not a runner.

use sc_daemon::{HttpTransport, Transport};
use sc_proto::wire::{DraftFailed, DraftedSpec};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(url), Some(key)) = (args.next(), args.next()) else {
        eprintln!("usage: poll-once <base-url> <api-key> [spec-text]");
        std::process::exit(2);
    };
    let spec = args.next();

    let transport = HttpTransport::new(&url, &key);
    println!("polling {url}");

    let item = match transport.poll() {
        Ok(Some(item)) => item,
        // A `None` is the server holding the poll open and timing out with
        // nothing queued. That is the healthy idle path, not a failure.
        Ok(None) => {
            println!("no work — the server had nothing queued");
            return;
        }
        Err(e) => {
            eprintln!("poll failed: {e}");
            std::process::exit(1);
        }
    };

    println!("claimed  {} [{}] {}", item.id, item.kind.slug(), item.repo);
    println!("text     {}", item.text);
    if let Some(note) = &item.send_back_note {
        println!("sent back: {note}");
    }

    // With no spec argument this reports a failure instead, which is the other
    // half of the protocol and just as worth exercising — a run that can only
    // report success leaves the unhappy path unproven.
    let outcome = match spec {
        Some(text) => {
            let drafted = DraftedSpec::new(&item.id, &text, "specs/probe");
            transport.push_drafted(&drafted).map(|()| "drafted")
        }
        None => {
            let failed = DraftFailed::new(&item.id, "poll-once probe: no spec text was given");
            transport.push_failed(&failed).map(|()| "reported failed")
        }
    };

    match outcome {
        Ok(what) => println!("{what} — pushed back to the server"),
        Err(e) => {
            eprintln!("push failed: {e}");
            std::process::exit(1);
        }
    }
}
