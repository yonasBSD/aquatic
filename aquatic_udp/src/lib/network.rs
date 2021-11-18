use std::collections::BTreeMap;
use std::io::{Cursor, ErrorKind};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::vec::Drain;

use crossbeam_channel::Receiver;
use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use rand::prelude::{Rng, SeedableRng, StdRng};

use aquatic_common::access_list::create_access_list_cache;
use aquatic_common::access_list::AccessListCache;
use aquatic_common::AHashIndexMap;
use aquatic_common::ValidUntil;
use aquatic_udp_protocol::*;
use socket2::{Domain, Protocol, Socket, Type};

use crate::common::*;
use crate::config::Config;

#[derive(Default)]
pub struct ConnectionMap(AHashIndexMap<(ConnectionId, SocketAddr), ValidUntil>);

impl ConnectionMap {
    pub fn insert(
        &mut self,
        connection_id: ConnectionId,
        socket_addr: SocketAddr,
        valid_until: ValidUntil,
    ) {
        self.0.insert((connection_id, socket_addr), valid_until);
    }

    pub fn contains(&self, connection_id: ConnectionId, socket_addr: SocketAddr) -> bool {
        self.0.contains_key(&(connection_id, socket_addr))
    }

    pub fn clean(&mut self) {
        let now = Instant::now();

        self.0.retain(|_, v| v.0 > now);
        self.0.shrink_to_fit();
    }
}

pub struct PendingScrapeResponseMeta {
    num_pending: usize,
    valid_until: ValidUntil,
}

#[derive(Default)]
pub struct PendingScrapeResponseMap(
    AHashIndexMap<TransactionId, (PendingScrapeResponseMeta, PendingScrapeResponse)>,
);

impl PendingScrapeResponseMap {
    pub fn prepare(
        &mut self,
        transaction_id: TransactionId,
        num_pending: usize,
        valid_until: ValidUntil,
    ) {
        let meta = PendingScrapeResponseMeta {
            num_pending,
            valid_until,
        };
        let response = PendingScrapeResponse {
            transaction_id,
            torrent_stats: BTreeMap::new(),
        };

        self.0.insert(transaction_id, (meta, response));
    }

    pub fn add_and_get_finished(&mut self, response: PendingScrapeResponse) -> Option<Response> {
        let finished = if let Some(r) = self.0.get_mut(&response.transaction_id) {
            r.0.num_pending -= 1;

            r.1.torrent_stats.extend(response.torrent_stats.into_iter());

            r.0.num_pending == 0
        } else {
            ::log::warn!("PendingScrapeResponses.add didn't find PendingScrapeResponse in map");

            false
        };

        if finished {
            let response = self.0.remove(&response.transaction_id).unwrap().1;

            Some(Response::Scrape(ScrapeResponse {
                transaction_id: response.transaction_id,
                torrent_stats: response.torrent_stats.into_values().collect(),
            }))
        } else {
            None
        }
    }

    pub fn clean(&mut self) {
        let now = Instant::now();

        self.0.retain(|_, v| v.0.valid_until.0 > now);
        self.0.shrink_to_fit();
    }
}

pub fn run_socket_worker(
    state: State,
    config: Config,
    token_num: usize,
    request_sender: ConnectedRequestSender,
    response_receiver: Receiver<(ConnectedResponse, SocketAddr)>,
    num_bound_sockets: Arc<AtomicUsize>,
) {
    let mut rng = StdRng::from_entropy();
    let mut buffer = [0u8; MAX_PACKET_SIZE];

    let mut socket = UdpSocket::from_std(create_socket(&config));
    let mut poll = Poll::new().expect("create poll");

    let interests = Interest::READABLE;

    poll.registry()
        .register(&mut socket, Token(token_num), interests)
        .unwrap();

    num_bound_sockets.fetch_add(1, Ordering::SeqCst);

    let mut events = Events::with_capacity(config.network.poll_event_capacity);
    let mut connections = ConnectionMap::default();
    let mut pending_scrape_responses = PendingScrapeResponseMap::default();

    let mut local_responses: Vec<(Response, SocketAddr)> = Vec::new();

    let timeout = Duration::from_millis(50);

    let cleaning_duration = Duration::from_secs(config.cleaning.connection_cleaning_interval);

    let mut iter_counter = 0usize;
    let mut last_cleaning = Instant::now();

    loop {
        poll.poll(&mut events, Some(timeout))
            .expect("failed polling");

        for event in events.iter() {
            let token = event.token();

            if (token.0 == token_num) & event.is_readable() {
                read_requests(
                    &config,
                    &state,
                    &mut connections,
                    &mut pending_scrape_responses,
                    &mut rng,
                    &mut socket,
                    &mut buffer,
                    &request_sender,
                    &mut local_responses,
                );
            }
        }

        send_responses(
            &state,
            &config,
            &mut socket,
            &mut buffer,
            &response_receiver,
            &mut pending_scrape_responses,
            local_responses.drain(..),
        );

        if iter_counter % 32 == 0 {
            let now = Instant::now();

            if now > last_cleaning + cleaning_duration {
                connections.clean();

                last_cleaning = now;
            }
        }

        iter_counter = iter_counter.wrapping_add(1);
    }
}

#[inline]
fn read_requests(
    config: &Config,
    state: &State,
    connections: &mut ConnectionMap,
    pending_scrape_responses: &mut PendingScrapeResponseMap,
    rng: &mut StdRng,
    socket: &mut UdpSocket,
    buffer: &mut [u8],
    request_sender: &ConnectedRequestSender,
    local_responses: &mut Vec<(Response, SocketAddr)>,
) {
    let mut requests_received: usize = 0;
    let mut bytes_received: usize = 0;

    let valid_until = ValidUntil::new(config.cleaning.max_connection_age);

    let mut access_list_cache = create_access_list_cache(&state.access_list);

    loop {
        match socket.recv_from(&mut buffer[..]) {
            Ok((amt, src)) => {
                let res_request =
                    Request::from_bytes(&buffer[..amt], config.protocol.max_scrape_torrents);

                bytes_received += amt;

                if res_request.is_ok() {
                    requests_received += 1;
                }

                let src = match src {
                    SocketAddr::V6(src) => {
                        match src.ip().octets() {
                            // Convert IPv4-mapped address (available in std but nightly-only)
                            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, a, b, c, d] => {
                                SocketAddr::V4(SocketAddrV4::new(
                                    Ipv4Addr::new(a, b, c, d),
                                    src.port(),
                                ))
                            }
                            _ => src.into(),
                        }
                    }
                    src => src,
                };

                handle_request(
                    config,
                    connections,
                    pending_scrape_responses,
                    &mut access_list_cache,
                    rng,
                    request_sender,
                    local_responses,
                    valid_until,
                    res_request,
                    src,
                );
            }
            Err(err) => {
                if err.kind() == ErrorKind::WouldBlock {
                    break;
                }

                ::log::info!("recv_from error: {}", err);
            }
        }
    }

    if config.statistics.interval != 0 {
        state
            .statistics
            .requests_received
            .fetch_add(requests_received, Ordering::SeqCst);
        state
            .statistics
            .bytes_received
            .fetch_add(bytes_received, Ordering::SeqCst);
    }
}

pub fn handle_request(
    config: &Config,
    connections: &mut ConnectionMap,
    pending_scrape_responses: &mut PendingScrapeResponseMap,
    access_list_cache: &mut AccessListCache,
    rng: &mut StdRng,
    request_sender: &ConnectedRequestSender,
    local_responses: &mut Vec<(Response, SocketAddr)>,
    valid_until: ValidUntil,
    res_request: Result<Request, RequestParseError>,
    src: SocketAddr,
) {
    let access_list_mode = config.access_list.mode;

    match res_request {
        Ok(Request::Connect(request)) => {
            let connection_id = ConnectionId(rng.gen());

            connections.insert(connection_id, src, valid_until);

            let response = Response::Connect(ConnectResponse {
                connection_id,
                transaction_id: request.transaction_id,
            });

            local_responses.push((response, src))
        }
        Ok(Request::Announce(request)) => {
            if connections.contains(request.connection_id, src) {
                if access_list_cache
                    .load()
                    .allows(access_list_mode, &request.info_hash.0)
                {
                    let worker_index =
                        RequestWorkerIndex::from_info_hash(config, request.info_hash);

                    request_sender.try_send_to(
                        worker_index,
                        ConnectedRequest::Announce(request),
                        src,
                    );
                } else {
                    let response = Response::Error(ErrorResponse {
                        transaction_id: request.transaction_id,
                        message: "Info hash not allowed".into(),
                    });

                    local_responses.push((response, src))
                }
            }
        }
        Ok(Request::Scrape(request)) => {
            if connections.contains(request.connection_id, src) {
                let mut requests: AHashIndexMap<RequestWorkerIndex, PendingScrapeRequest> =
                    Default::default();

                let transaction_id = request.transaction_id;

                for (i, info_hash) in request.info_hashes.into_iter().enumerate() {
                    let pending = requests
                        .entry(RequestWorkerIndex::from_info_hash(&config, info_hash))
                        .or_insert_with(|| PendingScrapeRequest {
                            transaction_id,
                            info_hashes: BTreeMap::new(),
                        });

                    pending.info_hashes.insert(i, info_hash);
                }

                pending_scrape_responses.prepare(transaction_id, requests.len(), valid_until);

                for (request_worker_index, request) in requests {
                    request_sender.try_send_to(
                        request_worker_index,
                        ConnectedRequest::Scrape(request),
                        src,
                    );
                }
            }
        }
        Err(err) => {
            ::log::debug!("Request::from_bytes error: {:?}", err);

            if let RequestParseError::Sendable {
                connection_id,
                transaction_id,
                err,
            } = err
            {
                if connections.contains(connection_id, src) {
                    let response = ErrorResponse {
                        transaction_id,
                        message: err.right_or("Parse error").into(),
                    };

                    local_responses.push((response.into(), src));
                }
            }
        }
    }
}

#[inline]
fn send_responses(
    state: &State,
    config: &Config,
    socket: &mut UdpSocket,
    buffer: &mut [u8],
    response_receiver: &Receiver<(ConnectedResponse, SocketAddr)>,
    pending_scrape_responses: &mut PendingScrapeResponseMap,
    local_responses: Drain<(Response, SocketAddr)>,
) {
    let mut responses_sent: usize = 0;
    let mut bytes_sent: usize = 0;

    for (response, addr) in local_responses {
        send_response(
            config,
            socket,
            buffer,
            &mut responses_sent,
            &mut bytes_sent,
            response,
            addr,
        );
    }

    for (response, addr) in response_receiver.try_iter() {
        let opt_response = match response {
            ConnectedResponse::Scrape(r) => pending_scrape_responses.add_and_get_finished(r),
            ConnectedResponse::AnnounceIpv4(r) => Some(Response::AnnounceIpv4(r)),
            ConnectedResponse::AnnounceIpv6(r) => Some(Response::AnnounceIpv6(r)),
        };

        if let Some(response) = opt_response {
            send_response(
                config,
                socket,
                buffer,
                &mut responses_sent,
                &mut bytes_sent,
                response,
                addr,
            );
        }
    }

    if config.statistics.interval != 0 {
        state
            .statistics
            .responses_sent
            .fetch_add(responses_sent, Ordering::SeqCst);
        state
            .statistics
            .bytes_sent
            .fetch_add(bytes_sent, Ordering::SeqCst);
    }
}

fn send_response(
    config: &Config,
    socket: &mut UdpSocket,
    buffer: &mut [u8],
    responses_sent: &mut usize,
    bytes_sent: &mut usize,
    response: Response,
    addr: SocketAddr,
) {
    let mut cursor = Cursor::new(buffer);

    let addr = if config.network.address.is_ipv4() {
        if let SocketAddr::V4(addr) = addr {
            SocketAddr::V4(addr)
        } else {
            unreachable!()
        }
    } else {
        match addr {
            SocketAddr::V4(addr) => {
                let ip = addr.ip().to_ipv6_mapped();

                SocketAddr::V6(SocketAddrV6::new(ip, addr.port(), 0, 0))
            }
            addr => addr,
        }
    };

    match response.write(&mut cursor) {
        Ok(()) => {
            let amt = cursor.position() as usize;

            match socket.send_to(&cursor.get_ref()[..amt], addr) {
                Ok(amt) => {
                    *responses_sent += 1;
                    *bytes_sent += amt;
                }
                Err(err) => {
                    ::log::info!("send_to error: {}", err);
                }
            }
        }
        Err(err) => {
            ::log::error!("Response::write error: {:?}", err);
        }
    }
}

pub fn create_socket(config: &Config) -> ::std::net::UdpSocket {
    let socket = if config.network.address.is_ipv4() {
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
    } else {
        Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
    }
    .expect("create socket");

    if config.network.only_ipv6 {
        socket.set_only_v6(true).expect("socket: set only ipv6");
    }

    socket.set_reuse_port(true).expect("socket: set reuse port");

    socket
        .set_nonblocking(true)
        .expect("socket: set nonblocking");

    socket
        .bind(&config.network.address.into())
        .unwrap_or_else(|err| panic!("socket: bind to {}: {:?}", config.network.address, err));

    let recv_buffer_size = config.network.socket_recv_buffer_size;

    if recv_buffer_size != 0 {
        if let Err(err) = socket.set_recv_buffer_size(recv_buffer_size) {
            ::log::error!(
                "socket: failed setting recv buffer to {}: {:?}",
                recv_buffer_size,
                err
            );
        }
    }

    socket.into()
}
