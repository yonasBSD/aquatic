use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;
use std::thread::Builder;

use crossbeam_channel::unbounded;
use privdrop::PrivDrop;

pub mod common;
pub mod config;
pub mod handlers;
pub mod network;
pub mod tasks;

use config::Config;
use common::State;


pub fn run(config: Config){
    let state = State::new();

    let (request_sender, request_receiver) = unbounded();
    let (response_sender, response_receiver) = unbounded();

    for i in 0..config.request_workers {
        let state = state.clone();
        let config = config.clone();
        let request_receiver = request_receiver.clone();
        let response_sender = response_sender.clone();

        Builder::new().name(format!("request-{:02}", i + 1)).spawn(move ||
            handlers::run_request_worker(
                state,
                config,
                request_receiver,
                response_sender
            )
        ).expect("spawn request worker");
    }

    let num_bound_sockets = Arc::new(AtomicUsize::new(0));

    for i in 0..config.socket_workers {
        let state = state.clone();
        let config = config.clone();
        let request_sender = request_sender.clone();
        let response_receiver = response_receiver.clone();
        let num_bound_sockets = num_bound_sockets.clone();

        Builder::new().name(format!("socket-{:02}", i + 1)).spawn(move ||
            network::run_socket_worker(
                state,
                config,
                i,
                request_sender,
                response_receiver,
                num_bound_sockets,
            )
        ).expect("spawn socket worker");
    }

    if config.statistics.interval != 0 {
        let state = state.clone();
        let config = config.clone();

        Builder::new().name("statistics-collector".to_string()).spawn(move ||
            loop {
                ::std::thread::sleep(Duration::from_secs(
                    config.statistics.interval
                ));

                tasks::gather_and_print_statistics(&state, &config);
            }
        ).expect("spawn statistics thread");
    }

    if config.privileges.drop_privileges {
        loop {
            let sockets = num_bound_sockets.load(Ordering::SeqCst);

            if sockets == config.socket_workers {
                PrivDrop::default()
                    .chroot(config.privileges.chroot_path)
                    .user(config.privileges.user)
                    .apply()
                    .expect("drop privileges");

                break;
            }

            ::std::thread::sleep(Duration::from_millis(10));
        }
    }

    loop {
        ::std::thread::sleep(Duration::from_secs(config.cleaning.interval));

        tasks::clean_connections_and_torrents(&state);
    }
}