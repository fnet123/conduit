#![cfg_attr(feature = "cargo-clippy", allow(clone_on_ref_ptr))]

use support::*;

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use self::bytes::BufMut;
use self::conduit_proxy::control::pb;
use self::futures::sync::mpsc;
use self::prost::Message;

pub fn new() -> Controller {
    Controller::new()
}

#[derive(Debug)]
pub struct Controller {
    destinations: Vec<(String, Option<pb::destination::Update>)>,
    reports: Option<mpsc::UnboundedSender<pb::telemetry::ReportRequest>>,
}

#[derive(Debug)]
pub struct Listening {
    pub addr: SocketAddr,
    shutdown: Shutdown,
}

impl Controller {
    pub fn new() -> Self {
        Controller {
            destinations: Vec::new(),
            reports: None,
        }
    }

    pub fn destination(mut self, dest: &str, addr: SocketAddr) -> Self {
        self.destinations
            .push((dest.into(), Some(destination_update(addr))));
        self
    }

    pub fn destination_close(mut self, dest: &str) -> Self {
        self.destinations.push((dest.into(), None));
        self
    }

    pub fn reports(&mut self) -> mpsc::UnboundedReceiver<pb::telemetry::ReportRequest> {
        let (tx, rx) = mpsc::unbounded();
        self.reports = Some(tx);
        rx
    }

    pub fn run(self) -> Listening {
        run(self)
    }
}

type Response = self::http::Response<GrpcBody>;
type Destinations = Arc<Mutex<Vec<(String, Option<pb::destination::Update>)>>>;

const DESTINATION_GET: &str = "/conduit.proxy.destination.Destination/Get";
const TELEMETRY_REPORT: &str = "/conduit.proxy.telemetry.Telemetry/Report";

#[derive(Debug)]
struct Svc {
    destinations: Destinations,
    reports: Option<mpsc::UnboundedSender<pb::telemetry::ReportRequest>>,
}

impl Svc {
    fn route(
        &self,
        path: &str,
        body: RecvBodyStream,
    ) -> Box<Future<Item = Response, Error = h2::Error>> {
        let mut rsp = http::Response::builder();
        rsp.version(http::Version::HTTP_2);

        match path {
            DESTINATION_GET => {
                let destinations = self.destinations.clone();
                Box::new(body.concat2().and_then(move |_bytes| {
                    let update = {
                        let mut vec = destinations.lock().unwrap();
                        //TODO: decode `_bytes` and compare with `.0`
                        if !vec.is_empty() {
                            vec.remove(0).1
                        } else {
                            None
                        }
                    }.unwrap_or_default();
                    let len = update.encoded_len();
                    let mut buf = BytesMut::with_capacity(len + 5);
                    buf.put(0u8);
                    buf.put_u32::<BigEndian>(len as u32);
                    update.encode(&mut buf).unwrap();
                    let body = GrpcBody::new(buf.freeze());
                    let rsp = rsp.body(body).unwrap();
                    Ok(rsp)
                }))
            }
            TELEMETRY_REPORT => {
                let mut reports = self.reports.clone();
                Box::new(body.concat2().and_then(move |mut bytes| {
                    if let Some(ref mut report) = reports {
                        let req = Message::decode(bytes.split_off(5)).unwrap();
                        let _ = report.unbounded_send(req);
                    }
                    let body = GrpcBody::new([0u8; 5][..].into());
                    let rsp = rsp.body(body).unwrap();
                    Ok(rsp)
                }))
            }
            unknown => {
                println!("unknown route: {:?}", unknown);
                let body = GrpcBody::unimplemented();
                let rsp = rsp.body(body).unwrap();
                Box::new(future::ok(rsp))
            }
        }
    }
}

impl Service for Svc {
    type Request = Request<RecvBody>;
    type Response = Response;
    type Error = h2::Error;
    type Future = Box<Future<Item = Response, Error = h2::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Request<RecvBody>) -> Self::Future {
        let (head, body) = req.into_parts();
        self.route(head.uri.path(), RecvBodyStream(body))
    }
}

struct GrpcBody {
    message: Bytes,
    status: &'static str,
}

impl GrpcBody {
    fn new(body: Bytes) -> Self {
        GrpcBody {
            message: body,
            status: "0",
        }
    }

    fn unimplemented() -> Self {
        GrpcBody {
            message: Bytes::new(),
            status: "12",
        }
    }
}


impl Body for GrpcBody {
    type Data = Bytes;

    fn poll_data(&mut self) -> Poll<Option<Bytes>, self::h2::Error> {
        let data = self.message.split_off(0);
        let data = if data.is_empty() { None } else { Some(data) };

        Ok(Async::Ready(data))
    }

    fn poll_trailers(&mut self) -> Poll<Option<HeaderMap>, self::h2::Error> {
        let mut map = HeaderMap::new();
        map.insert("grpc-status", HeaderValue::from_static(self.status));
        Ok(Async::Ready(Some(map)))
    }
}

#[derive(Debug)]
struct NewSvc {
    destinations: Destinations,
    reports: Option<mpsc::UnboundedSender<pb::telemetry::ReportRequest>>,
}
impl NewService for NewSvc {
    type Request = Request<RecvBody>;
    type Response = Response;
    type Error = h2::Error;
    type InitError = ::std::io::Error;
    type Service = Svc;
    type Future = future::FutureResult<Svc, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        future::ok(Svc {
            destinations: self.destinations.clone(),
            reports: self.reports.clone(),
        })
    }
}

fn run(controller: Controller) -> Listening {
    let (tx, rx) = shutdown_signal();
    let (addr_tx, addr_rx) = oneshot::channel();

    ::std::thread::Builder::new()
        .name("support controller".into())
        .spawn(move || {
            let mut core = Core::new().unwrap();
            let reactor = core.handle();

            let factory = NewSvc {
                destinations: Arc::new(Mutex::new(controller.destinations)),
                reports: controller.reports,
            };
            let h2 = tower_h2::Server::new(factory, Default::default(), reactor.clone());

            let addr = ([127, 0, 0, 1], 0).into();
            let bind = TcpListener::bind(&addr, &reactor).expect("bind");

            let _ = addr_tx.send(bind.local_addr().expect("addr"));

            let serve = bind.incoming()
                .fold((h2, reactor), |(h2, reactor), (sock, _)| {
                    if let Err(e) = sock.set_nodelay(true) {
                        return Err(e);
                    }

                    let serve = h2.serve(sock);
                    reactor.spawn(serve.map_err(|e| println!("controller error: {:?}", e)));

                    Ok((h2, reactor))
                });


            core.handle().spawn(
                serve
                    .map(|_| ())
                    .map_err(|e| println!("controller error: {}", e)),
            );

            core.run(rx).unwrap();
        })
        .unwrap();

    let addr = addr_rx.wait().expect("addr");
    Listening {
        addr,
        shutdown: tx,
    }
}

fn destination_update(addr: SocketAddr) -> pb::destination::Update {
    pb::destination::Update {
        update: Some(pb::destination::update::Update::Add(
            pb::destination::WeightedAddrSet {
                addrs: vec![
                    pb::destination::WeightedAddr {
                        addr: Some(pb::common::TcpAddress {
                            ip: Some(ip_conv(addr.ip())),
                            port: u32::from(addr.port()),
                        }),
                        weight: 0,
                    },
                ],
            },
        )),
    }
}

fn ip_conv(ip: IpAddr) -> pb::common::IpAddress {
    match ip {
        IpAddr::V4(v4) => pb::common::IpAddress {
            ip: Some(pb::common::ip_address::Ip::Ipv4(v4.into())),
        },
        IpAddr::V6(v6) => {
            let (first, last) = octets_to_u64s(v6.octets());
            pb::common::IpAddress {
                ip: Some(pb::common::ip_address::Ip::Ipv6(pb::common::IPv6 {
                    first,
                    last,
                })),
            }
        }
    }
}

fn octets_to_u64s(octets: [u8; 16]) -> (u64, u64) {
    let first = (u64::from(octets[0]) << 56) + (u64::from(octets[1]) << 48)
        + (u64::from(octets[2]) << 40) + (u64::from(octets[3]) << 32)
        + (u64::from(octets[4]) << 24) + (u64::from(octets[5]) << 16)
        + (u64::from(octets[6]) << 8) + u64::from(octets[7]);
    let last = (u64::from(octets[8]) << 56) + (u64::from(octets[9]) << 48)
        + (u64::from(octets[10]) << 40) + (u64::from(octets[11]) << 32)
        + (u64::from(octets[12]) << 24) + (u64::from(octets[13]) << 16)
        + (u64::from(octets[14]) << 8) + u64::from(octets[15]);
    (first, last)
}
