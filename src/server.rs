use std::str;
use std::time;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono;
use futures::{future, Future, Stream};
use hyper;
use hyper::service::service_fn;
use hyper::Server;
use hyper::{Body, Method, Request, Response, StatusCode};
use lazy_static::lazy_static;
use log::{debug, error, info};
use regex::Regex;
use serde_derive::{Deserialize, Serialize};
use serde_json;
use uuid::Uuid;

use super::types::{Config, Host, Registration, Storage, Tag};
use super::v2xds::{
    hosts_to_locality_lb_endpoints, ClusterLoadAssignment, DiscoveryRequest, EdsDiscoveryResponse,
    EDS_TYPE_URL,
};

type BoxFut = Box<Future<Item = Response<Body>, Error = hyper::Error> + Send>;

#[derive(Serialize, Deserialize, Debug)]
struct RegistrationParam {
    ip: String,
    port: u16,
    revision: String,
    tags: Tag,
}

#[derive(Serialize, Debug)]
struct ErrorResponse {
    // Machine readable error code.
    id: ErrorId,
    // Error description for human.
    reason: String,
}

#[derive(Serialize, Debug)]
enum ErrorId {
    HostNotFound,
}

pub fn run<S: Storage>(c: &Config, s: S) {
    // XXX: ipv4 only
    let addr = ([0, 0, 0, 0], c.listen_port).into();
    let new_service = move || {
        let st = s.clone();
        service_fn(move |req| {
            let stt = st.clone();
            route(stt, req)
        })
    };
    let server = Server::bind(&addr)
        .serve(new_service)
        .map_err(|e| error!("server error: {}", e));
    info!("Listening on {}", addr);
    let mut builder = tokio::runtime::Builder::new();
    if let Some(num) = get_core_threads() {
        log::info!("Set core_threads to {}", num);
        builder.core_threads(num);
    }
    let mut entered = tokio_executor::enter().expect("nested tokio::run");
    let mut runtime = builder.build().expect("failed to start new Runtime");
    runtime.spawn(server);
    entered
        .block_on(runtime.shutdown_on_idle())
        .expect("shutdown cannot error");
}

fn get_core_threads() -> Option<usize> {
    std::env::var("CORE_THREADS")
        .ok()
        .and_then(|core_threads| match core_threads.parse() {
            Ok(num) => Some(num),
            Err(e) => {
                log::warn!("unable to parse CORE_THREADS into usize: {}", e);
                None
            }
        })
}

fn route<S: Storage>(s: S, req: Request<Body>) -> BoxFut {
    info!(
        "Recieve request: method={}, path={}",
        req.method(),
        req.uri().to_owned().path()
    );
    match *req.method() {
        Method::GET => route_get_req(&s, req),
        Method::POST => route_post_req(s, req),
        Method::DELETE => route_delete_req(&s, req),
        _ => res_404(),
    }
}

fn route_get_req<S: Storage>(s: &S, req: Request<Body>) -> BoxFut {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"^/v1/registration/([^/]+)/?$").unwrap();
    }

    let uri = req.uri().to_owned();
    match uri.path() {
        "/" => show_usage(req),
        "/hc" => check_health(req),
        _ => match RE.captures(uri.path()) {
            Some(caps) => match caps.get(1) {
                Some(m) => get_registration(s, req, m.as_str()),
                _ => res_404(),
            },
            _ => res_404(),
        },
    }
}

fn route_post_req<S: Storage>(s: S, req: Request<Body>) -> BoxFut {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"^/v1/registration/([^/]+)/?$").unwrap();
    }

    let uri = req.uri().to_owned();
    match uri.path() {
        "/" => show_usage(req),
        "/hc" => check_health(req),
        "/v2/discovery:endpoints" => get_registration_v2(&s, req),
        _ => match RE.captures(uri.path()) {
            Some(caps) => match caps.get(1) {
                Some(m) => register_hosts(s, req, m.as_str()),
                _ => res_404(),
            },
            _ => res_404(),
        },
    }
}

fn route_delete_req<S: Storage>(s: &S, req: Request<Body>) -> BoxFut {
    lazy_static! {
        static ref RE: Regex =
            Regex::new(r"^/v1/registration/([^/]+)/([^/:]+):([^/:]+)/?$").unwrap();
    }

    let uri = req.uri().to_owned();
    match uri.path() {
        "/" => show_usage(req),
        "/hc" => check_health(req),
        _ => match RE.captures(uri.path()) {
            Some(caps) => match caps.get(1) {
                Some(m_service) => match caps.get(2) {
                    Some(m_ip) => match caps.get(3) {
                        Some(m_port) => delete_host(
                            s,
                            m_service.as_str(),
                            m_ip.as_str().to_string(),
                            m_port.as_str(),
                        ),
                        _ => res_404(),
                    },
                    _ => res_404(),
                },
                _ => res_404(),
            },
            _ => res_404(),
        },
    }
}

fn get_registration<S: Storage>(s: &S, _: Request<Body>, name: &str) -> BoxFut {
    let hosts = match s.query_items(name) {
        Ok(v) => v,
        Err(e) => return res_500(e.to_string()),
    };
    let registration = Registration {
        service: name.to_owned(),
        env: "production".to_owned(),
        hosts,
    };
    let body = match serde_json::to_string(&registration) {
        Ok(v) => v,
        Err(e) => return res_500(e.to_string()),
    };
    info!("Build 200 response: body-size={}", body.len());
    wrap_future(Response::new(Body::from(body)))
}

fn get_registration_v2<S: Storage>(s: &S, req: Request<Body>) -> BoxFut {
    let st = s.clone();
    let f = req
        .into_body()
        .concat2()
        .map(move |buffer| match str::from_utf8(&buffer) {
            Ok(body) => match serde_json::from_str::<DiscoveryRequest>(&body) {
                Ok(d_req) => {
                    let mut resources = Vec::new();
                    for name in &d_req.resource_names {
                        let hosts = match st.query_items(&name) {
                            Ok(v) => v,
                            Err(e) => return build_500(e.to_string()),
                        };
                        let lle_vec = hosts_to_locality_lb_endpoints(hosts);
                        resources.push(ClusterLoadAssignment {
                            type_url: EDS_TYPE_URL.to_string(),
                            cluster_name: name.to_owned(),
                            endpoints: lle_vec,
                        });
                    }

                    let d_res = EdsDiscoveryResponse {
                        version_info: Uuid::new_v4().to_string(),
                        resources,
                    };
                    let body = match serde_json::to_string(&d_res) {
                        Ok(v) => v,
                        Err(e) => return build_500(e.to_string()),
                    };
                    info!("Build 200 response: body-size={}", body.len());
                    Response::new(Body::from(body))
                }
                Err(m) => {
                    let mut msg = "Invalid JSON string: ".to_owned();
                    msg.push_str(&m.to_string());
                    debug!("invalid json: {:?}", msg);
                    debug!("invalid request: {:?}", body);
                    build_400(msg)
                }
            },
            Err(_) => build_400("Invalid UTF-8 string".to_owned()),
        });
    Box::new(f)
}

fn register_hosts<S: Storage>(s: S, req: Request<Body>, name: &str) -> BoxFut {
    let st = s.clone();
    let name = name.to_owned();
    let f = req
        .into_body()
        .concat2()
        .map(move |buffer| match str::from_utf8(&buffer) {
            Ok(body) => match serde_json::from_str::<RegistrationParam>(&body) {
                Ok(param) => {
                    let host = match convert_param_to_host(&name, param, s.ttl()) {
                        Ok(v) => v,
                        Err(_) => {
                            error!("Failed to fetch system time");
                            return build_500("Failed to fetch system time".to_owned());
                        }
                    };
                    if let Err(e) = st.store_item(&name, host) {
                        return build_500(e.to_string());
                    }

                    info!("Build 202 response");
                    Response::builder()
                        .status(StatusCode::ACCEPTED)
                        .body(Body::empty())
                        .unwrap()
                }
                Err(m) => {
                    let mut msg = "Invalid JSON string: ".to_owned();
                    msg.push_str(&m.to_string());
                    build_400(msg)
                }
            },
            Err(_) => build_400("Invalid UTF-8 string".to_owned()),
        });
    Box::new(f)
}

fn delete_host<S: Storage>(s: &S, name: &str, ip: String, port_string: &str) -> BoxFut {
    let port = match port_string.parse() {
        Ok(v) => v,
        Err(_e) => return res_400(format!("Given port is invalid as integer: {}", port_string)),
    };

    match s.delete_item(name, ip, port) {
        Ok(res) => {
            if res.is_none() {
                let r = ErrorResponse {
                    id: ErrorId::HostNotFound,
                    reason: "Not found the entry".to_owned(),
                };
                let body = match serde_json::to_string(&r) {
                    Ok(v) => v,
                    Err(e) => return res_500(e.to_string()),
                };
                return res_400(body);
            }
        }
        Err(e) => return res_500(e.to_string()),
    }

    info!("Build 202 response");
    wrap_future(
        Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap(),
    )
}

fn convert_param_to_host(
    name: &str,
    p: RegistrationParam,
    ttl: u64,
) -> Result<Host, time::SystemTimeError> {
    let last_check_in = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S%:z")
        .to_string();
    let expire_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + ttl;
    Ok(Host {
        ip_address: p.ip,
        port: p.port,
        last_check_in,
        expire_time,
        revision: p.revision,
        service: name.to_owned(),
        tags: p.tags,
    })
}

fn show_usage(_: Request<Body>) -> BoxFut {
    let usage = "GET /v1/registration/:service, POST /v1/registration/:service, DELETE \
                 /v1/registration/:service/:ip_address";
    wrap_future(Response::new(Body::from(usage)))
}

fn check_health(_: Request<Body>) -> BoxFut {
    wrap_future(Response::new(Body::from("ok")))
}

fn build_400(msg: String) -> Response<Body> {
    info!("Build 400 response");
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from(msg))
        .unwrap()
}

fn res_400(msg: String) -> BoxFut {
    wrap_future(build_400(msg))
}

fn res_404() -> BoxFut {
    wrap_future(
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    )
}

fn build_500(msg: String) -> Response<Body> {
    info!("Build 500 response: body={}", msg);
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from(msg))
        .unwrap()
}

fn res_500(msg: String) -> BoxFut {
    wrap_future(build_500(msg))
}

fn wrap_future(res: Response<Body>) -> BoxFut {
    Box::new(future::ok(res))
}
