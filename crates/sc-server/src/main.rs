//! `sc-server` — the hosted intake surface.
//!
//! Configured entirely from the environment, because it ships as a Docker image
//! and a Portainer stack editor is where a user configures one.

fn main() {
    // `sc-server --health` — the container's own healthcheck.
    //
    // **The binary checks itself** rather than the image carrying curl. The
    // runtime layer is Alpine plus one static binary, and adding an HTTP client
    // to it would be a package and its dependencies on the one component facing
    // the internet, to answer a question the binary can answer in ten lines.
    //
    // Why it matters: without a healthcheck Docker reports the container
    // *running* the instant the process is spawned, which is before the port is
    // listening. A proxy in front then forwards to a socket nobody is on yet,
    // and the deploy shows a 502 for the second or two in between. Swarm's
    // start-first rollout needs this too — it is how it decides the new task is
    // ready before stopping the old one.
    if std::env::args().nth(1).as_deref() == Some("--health") {
        std::process::exit(match sc_server::health() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("unhealthy: {e}");
                1
            }
        });
    }

    let cfg = match sc_server::Config::from_env() {
        Ok(cfg) => cfg,
        Err(msg) => {
            // Refusing to start is the point: an unauthenticated intake surface
            // on the public internet is the failure this design exists to
            // prevent, so a misconfiguration must not degrade into running open.
            sc_server::log::error("cannot start")
                .text("err", msg)
                .emit();
            std::process::exit(2);
        }
    };

    if let Err(e) = sc_server::run(&cfg) {
        sc_server::log::error("stopped").text("err", e).emit();
        std::process::exit(1);
    }
}
