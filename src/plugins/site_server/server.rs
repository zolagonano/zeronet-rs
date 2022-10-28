use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use actix::Addr;
use actix_files::NamedFile;
use actix_web::{
    body::BoxBody,
    dev::{ServiceFactory, ServiceRequest, ServiceResponse},
    http::header::{self, HeaderValue},
    web::{get, Data, Query},
    App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use log::*;

use crate::{
    controllers::{sites::SitesController, users::UserController},
    core::{address::Address, error::Error},
    environment::ENV,
    plugins::{
        register_plugins,
        site_server::{handlers::sites::*, wrapper::*},
        websocket,
    },
};

pub struct ZeroServer {
    pub user_controller: actix::Addr<UserController>,
    pub site_controller: actix::Addr<SitesController>,
    pub wrapper_nonces: Arc<Mutex<HashSet<String>>>,
}

pub trait AppEntryImpl = ServiceFactory<
    ServiceRequest,
    Response = ServiceResponse<BoxBody>,
    Config = (),
    InitError = (),
    Error = actix_web::Error,
>;

fn build_app(shared_data: ZeroServer) -> App<impl AppEntryImpl> {
    let app = register_plugins(App::new().app_data(Data::new(shared_data)))
        .route("/", get().to(index))
        .route("/{address:1[^/]+}", get().to(serve_site))
        .route("/{address:1[^/]+}/{inner_path:.*}", get().to(serve_site))
        .route("/uimedia/{inner_path}.{ext}", get().to(serve_uimedia))
        .route("/{inner_path}.{ext}", get().to(serve_uimedia));
    websocket::register_site_plugins(app)
}

pub async fn run(
    site_controller: Addr<SitesController>,
    user_controller: Addr<UserController>,
) -> std::io::Result<()> {
    let nonces = Arc::new(Mutex::new(HashSet::new()));
    HttpServer::new(move || {
        let shared_data = ZeroServer {
            site_controller: site_controller.clone(),
            user_controller: user_controller.clone(),
            wrapper_nonces: nonces.clone(),
        };
        build_app(shared_data)
    })
    .bind(format!("{}:{}", &*ENV.ui_ip, &ENV.ui_port))
    .unwrap()
    .run()
    .await
}

pub async fn index(_: HttpRequest) -> impl Responder {
    let mut resp = HttpResponse::PermanentRedirect();
    resp.append_header((header::LOCATION, &*ENV.homepage));
    resp
}

async fn serve_site(req: HttpRequest, query: Query<HashMap<String, String>>) -> HttpResponse {
    let data = req.app_data::<Data<ZeroServer>>().unwrap().clone();
    let mut wrapper = true;
    let address = req.match_info().query("address");
    let inner_path = req.match_info().query("inner_path");
    // let addr_str = address.to_string();
    // let site_controller = data.site_controller.clone();
    // actix::spawn(async move {
    //     info!("Sending site announce to {}", &addr_str);
    //     let address = Address::from_str(&addr_str).unwrap();
    //     let start = Instant::now();
    //     site_controller.do_send(SiteAnnounce { address });
    //     let taken = start.duration_since(start);
    //     println!("{}", taken.as_micros());
    // });
    if inner_path == "favicon.ico" || inner_path == "apple-touch-icon.png" {
        return serve_uimedia(req).await;
    } else if !inner_path.is_empty()
        && inner_path.contains('.')
        && !inner_path.ends_with(".html")
        && !inner_path.ends_with(".xhtml")
    {
        wrapper = false;
    } else {
        let mut wrapper_nonces = req
            .app_data::<Data<ZeroServer>>()
            .unwrap()
            .wrapper_nonces
            .lock()
            .unwrap();
        let wrapper_nonce = query.get("wrapper_nonce");
        if wrapper_nonce.is_some() && wrapper_nonces.contains(wrapper_nonce.unwrap()) {
            wrapper_nonces.remove(wrapper_nonce.unwrap());
            wrapper = false;
        } else if wrapper_nonce.is_some() {
            warn!("Nonce {:?} invalid!", wrapper_nonce);
        }
    }

    if wrapper {
        trace!(
            "No valid nonce provided, serving wrapper for zero:://{}",
            address
        );
        return serve_wrapper(req, data).await;
    }

    // TODO: allow nonce to be reused for any file within same zite
    match serve_file(&req, data).await {
        Ok(res) => {
            let content_type = res.content_type().clone();
            let type_ = content_type.type_();
            let subtype = content_type.subtype();
            let mut response = res.respond_to(&req);
            if !(type_ == "text" && subtype == "html") {
                response.headers_mut().append(
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    HeaderValue::from_static("*"),
                );
            }
            response
        }
        Err(err) => {
            error!("Serve Site:: Bad request {:?}", err);
            HttpResponse::BadRequest().finish()
        }
    }
}

async fn serve_file(req: &HttpRequest, data: Data<ZeroServer>) -> Result<NamedFile, Error> {
    let mut file_path = PathBuf::new();
    let address = req.match_info().query("address");
    let mut inner_path = String::from(req.match_info().query("inner_path"));
    if address == "Test" {
        file_path.push(&Path::new("test/wrapper/public"));
    } else {
        file_path = ENV.data_path.clone();
        file_path.push(&Path::new(address));
    }
    file_path.push(&Path::new(&inner_path));

    // TODO: what if a file doesn't have an extension?
    if file_path.is_dir() || !inner_path.contains('.') {
        file_path = file_path.join(PathBuf::from("index.html"));
        // TODO: should we edit inner_path here? or just create a new one?
        inner_path = format!("{}/index.html", &inner_path)
            .trim_start_matches('/')
            .to_string();
    }

    trace!(
        "Serving file: zero://{}/{} as {:?}",
        &address,
        &inner_path,
        file_path
    );

    if !file_path.exists() {
        let lookup = Lookup::Address(Address::from_str(address)?);
        let (_, addr) = data.site_controller.send(lookup).await??;
        let msg = FileGetRequest {
            inner_path,
            format: String::new(),
            timeout: 0f64,
            required: true,
        };
        let res1 = addr.send(msg).await?;
        let res: bool = match res1 {
            Ok(v) => v,
            Err(err) => {
                return Err(err);
            }
        };
        if !res {
            return Err(Error::MissingError);
        }
    }

    let file = NamedFile::open(file_path)?;
    Result::Ok(file)
}