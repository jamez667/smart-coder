//! `sc-server` — the hosted intake surface.
//!
//! Configured entirely from the environment, because it ships as a Docker image
//! and a Portainer stack editor is where a user configures one.

fn main() {
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
