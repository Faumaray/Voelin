#![cfg(feature = "server")]

use std::{
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use schema_api::{
    api::addon::{
        user_addon_service_server::{UserAddonService, UserAddonServiceServer},
        Download, FileData, GetDownload, Platform, ReturnCode,
    },
    push::{
        push_service_server::{PushService, PushServiceServer},
        Message,
    },
    ApiClient,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    codegen::{http, Service},
    metadata::{MetadataMap, MetadataValue},
    server::NamedService,
    transport::{Channel, Server},
    Code, Request, Response, Status,
};

type RecordedRequests = Arc<Mutex<Vec<(MetadataMap, GetDownload)>>>;

#[derive(Clone, Default)]
struct AddonMock {
    requests: RecordedRequests,
}

#[tonic::async_trait]
impl UserAddonService for AddonMock {
    async fn request_download(
        &self,
        request: Request<GetDownload>,
    ) -> Result<Response<Download>, Status> {
        self.requests
            .lock()
            .unwrap()
            .push((request.metadata().clone(), request.get_ref().clone()));

        match request.get_ref().addon_uuid.as_str() {
            "denied" => {
                let mut status = Status::with_details(
                    Code::PermissionDenied,
                    "download denied",
                    b"opaque-status-details".as_slice().into(),
                );
                status
                    .metadata_mut()
                    .insert("x-policy", "private-addon".parse().unwrap());
                Err(status)
            }
            // The transport deadline must cancel an RPC that never resolves.
            "wait" => std::future::pending().await,
            _ => {
                let mut response = Response::new(Download {
                    download_info: Some(FileData {
                        url: "https://downloads.invalid/addon.zip".into(),
                        size: 1024,
                        sha1: "0123456789abcdef".into(),
                    }),
                    current_version: request.get_ref().current_version + 1,
                    return_code: ReturnCode::Update as i32,
                    addon_list: vec![request.into_inner().addon_uuid],
                    host_url: "https://downloads.invalid".into(),
                });
                response
                    .metadata_mut()
                    .insert("x-response", "received".parse().unwrap());
                Ok(response)
            }
        }
    }
}

#[derive(Clone)]
struct PushMock;

#[tonic::async_trait]
impl PushService for PushMock {
    #[allow(non_camel_case_types)]
    type longPullStream = tokio_stream::Iter<std::vec::IntoIter<Result<Message, Status>>>;

    async fn long_pull(
        &self,
        request: Request<()>,
    ) -> Result<Response<Self::longPullStream>, Status> {
        let mut frames = vec![
            Ok(Message {
                payload: vec![0, 1, 2, 255],
            }),
            Ok(Message {
                payload: b"second-frame".to_vec(),
            }),
        ];
        if request.metadata().contains_key("x-stream-failure") {
            frames.push(Err(Status::unavailable("stream interrupted")));
        }
        Ok(Response::new(tokio_stream::iter(frames)))
    }
}

/// Record the actual HTTP/2 paths, independently of generated server routing.
#[derive(Clone)]
struct RecordPath<S> {
    inner: S,
    paths: Arc<Mutex<Vec<String>>>,
}

impl<S: NamedService> NamedService for RecordPath<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, B> Service<http::Request<B>> for RecordPath<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        self.paths
            .lock()
            .unwrap()
            .push(request.uri().path().to_owned());
        self.inner.call(request)
    }
}

struct TestServer {
    url: String,
    requests: RecordedRequests,
    paths: Arc<Mutex<Vec<String>>>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl TestServer {
    async fn start() -> Self {
        // Bind first; the client can immediately connect without timing sleeps.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let addon = AddonMock::default();
        let requests = addon.requests.clone();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let addon = RecordPath {
            inner: UserAddonServiceServer::new(addon),
            paths: paths.clone(),
        };
        let push = RecordPath {
            inner: PushServiceServer::new(PushMock),
            paths: paths.clone(),
        };
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            Server::builder()
                .add_service(addon)
                .add_service(push)
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = stopped.await;
                })
                .await
        });
        Self {
            url,
            requests,
            paths,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("local server did not shut down")
            .expect("local server task panicked")
            .expect("local server failed");
    }
}

fn download_request(id: &str) -> GetDownload {
    GetDownload {
        addon_uuid: id.into(),
        current_version: 41,
        platform: Platform::LinuxX8664 as i32,
        ..Default::default()
    }
}

#[tokio::test]
async fn unary_round_trip_preserves_original_route_and_protobuf_fields() {
    let server = TestServer::start().await;
    let channel = Channel::from_shared(server.url.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let client = ApiClient::from_channel(channel);
    let response = client
        .addon()
        .request_download(download_request("example-addon"))
        .await
        .unwrap();

    assert_eq!(response.metadata().get("x-response").unwrap(), "received");
    let response = response.into_inner();
    assert_eq!(response.current_version, 42);
    assert_eq!(response.return_code(), ReturnCode::Update);
    assert_eq!(response.addon_list, ["example-addon"]);
    assert_eq!(response.download_info.unwrap().size, 1024);
    assert_eq!(
        server.requests.lock().unwrap()[0].1.platform(),
        Platform::LinuxX8664
    );
    assert_eq!(
        *server.paths.lock().unwrap(),
        ["/com.teamspeak.myteamspeak.proto.addon.UserAddonService/requestDownload"]
    );

    drop(client);
    server.stop().await;
}

#[tokio::test]
async fn default_metadata_is_sent_and_request_metadata_takes_precedence() {
    let server = TestServer::start().await;
    let mut metadata = MetadataMap::new();
    metadata.insert("x-tenant", "default-tenant".parse().unwrap());
    metadata.insert("x-client", "rust-sdk".parse().unwrap());
    metadata.insert_bin("x-context-bin", MetadataValue::from_bytes(b"\0\xffcontext"));
    let client = ApiClient::builder(server.url.clone())
        .unwrap()
        .metadata(metadata)
        .connect()
        .await
        .unwrap();

    client
        .addon()
        .request_download(download_request("first"))
        .await
        .unwrap();
    let mut request = Request::new(download_request("second"));
    request
        .metadata_mut()
        .insert("x-tenant", "specific-tenant".parse().unwrap());
    client.addon().request_download(request).await.unwrap();

    {
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0.get("x-tenant").unwrap(), "default-tenant");
        assert_eq!(requests[1].0.get("x-tenant").unwrap(), "specific-tenant");
        for (metadata, _) in requests.iter() {
            assert_eq!(metadata.get("x-client").unwrap(), "rust-sdk");
            assert_eq!(
                metadata
                    .get_bin("x-context-bin")
                    .unwrap()
                    .to_bytes()
                    .unwrap(),
                &b"\0\xffcontext"[..]
            );
        }
    }

    drop(client);
    server.stop().await;
}

#[tokio::test]
async fn grpc_status_preserves_code_message_details_and_metadata() {
    let server = TestServer::start().await;
    let client = ApiClient::connect(server.url.clone()).await.unwrap();
    let error = client
        .addon()
        .request_download(download_request("denied"))
        .await
        .unwrap_err();

    assert_eq!(error.code(), Code::PermissionDenied);
    assert_eq!(error.message(), "download denied");
    assert_eq!(error.details(), b"opaque-status-details");
    assert_eq!(error.metadata().get("x-policy").unwrap(), "private-addon");

    drop(client);
    server.stop().await;
}

#[tokio::test]
async fn push_stream_delivers_frames_eof_and_terminal_status() {
    let server = TestServer::start().await;
    let client = ApiClient::connect(server.url.clone()).await.unwrap();

    for fail in [false, true] {
        let mut request = Request::new(());
        if fail {
            request
                .metadata_mut()
                .insert("x-stream-failure", "true".parse().unwrap());
        }
        let mut stream = client.push().long_pull(request).await.unwrap().into_inner();
        assert_eq!(
            stream.message().await.unwrap().unwrap().payload,
            [0, 1, 2, 255]
        );
        assert_eq!(
            stream.message().await.unwrap().unwrap().payload,
            b"second-frame"
        );
        if fail {
            let error = stream.message().await.unwrap_err();
            assert_eq!(error.code(), Code::Unavailable);
            assert_eq!(error.message(), "stream interrupted");
        } else {
            assert!(stream.message().await.unwrap().is_none());
        }
    }
    assert_eq!(
        *server.paths.lock().unwrap(),
        [
            "/com.teamspeak.push.proto.PushService/longPull",
            "/com.teamspeak.push.proto.PushService/longPull",
        ]
    );

    drop(client);
    server.stop().await;
}

#[tokio::test]
async fn configured_deadline_cancels_a_pending_unary_rpc() {
    let server = TestServer::start().await;
    let client = ApiClient::builder(server.url.clone())
        .unwrap()
        .request_timeout(Duration::from_millis(100))
        .connect()
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        client.addon().request_download(download_request("wait")),
    )
    .await
    .expect("the configured deadline did not terminate the RPC")
    .unwrap_err();

    // Tonic currently reports its transport timeout as Cancelled. Servers can
    // independently enforce the propagated deadline as DeadlineExceeded.
    assert!(matches!(
        error.code(),
        Code::Cancelled | Code::DeadlineExceeded
    ));
    assert!(server.requests.lock().unwrap()[0]
        .0
        .contains_key("grpc-timeout"));

    drop(client);
    server.stop().await;
}
