#![windows_subsystem = "windows"]

use std::{
    backtrace::Backtrace,
    io::ErrorKind,
    panic::PanicInfo,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use async_compression::tokio::{
    bufread::ZstdDecoder,
    write::{GzipEncoder, ZstdEncoder},
};
use atomic_float::AtomicF32;
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{self, WebSocket},
        DefaultBodyLimit, Path, WebSocketUpgrade,
    },
    headers::{ContentEncoding, ContentLength},
    http::{header::ACCEPT_ENCODING, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router, TypedHeader,
};
use iced::{
    executor,
    futures::{
        channel::mpsc::Sender,
        future::{self, BoxFuture},
        FutureExt, SinkExt, StreamExt, TryStreamExt,
    },
    subscription,
    widget::{
        column, row,
        runtime::{command, window},
        Button, PickList, ProgressBar, Scrollable, Text, TextInput,
    },
    Alignment, Application, Color, Command, Length, Settings, Subscription, Theme,
};
use panicui::{app::PanicApplication, style::Style, window::PanicWindow};
use pin_project::pin_project;
use serde::{Deserialize, Serialize};
use slab::Slab;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    sync::{broadcast, RwLock},
};
use tokio_util::io::StreamReader;
use tower::ServiceExt;
use tower_http::{
    compression::Compression,
    services::{ServeDir, ServeFile},
};

type CurrentUpload = Arc<RwLock<Slab<InnerCurrentUpload>>>;

type InnerCurrentUpload = Arc<(ConcurrentUploadStatus, RwLock<Slab<ArcUploadStatus>>)>;

type ArcUploadStatus = Arc<UploadStatus>;

#[derive(Debug)]
struct Uploader {
    current_dir: Vec<String>,
    output_path: Arc<RwLock<Option<String>>>,
    new_dir_input: String,
    instructions: String,
    current_upload: CurrentUpload,
    download_path: Arc<RwLock<Vec<PathBuf>>>,
    clients_connected: u32,
    ws_send: Arc<broadcast::Sender<Message>>,
}

#[derive(Debug)]
struct UploadStatus {
    name: String,
    num_bytes: AtomicF32,
    bytes_uploaded: AtomicF32,
}

#[derive(Debug)]
struct ConcurrentUploadStatus {
    num_files: AtomicF32,
    files_uploaded: AtomicF32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Message {
    ChangePath(String),
    EditNewDir(String),
    ChoseFile(Vec<PathBuf>),
    OpenFileDialog,
    ClearDownloadPath,
    Refresh,
    AddPath,
    RecvExisitingPaths(Vec<String>),
    // Backend exclusive
    WSConnection,
    WSClose,
    FocusWindow,
}

impl Application for Uploader {
    type Message = Message;
    type Theme = Theme;
    type Flags = ();
    type Executor = executor::Default;

    fn new(_: Self::Flags) -> (Self, Command<Message>) {
        let current_dir = {
            let mut vec: Vec<String> = std::fs::read_dir(std::env::current_dir().unwrap())
                .unwrap()
                .flatten()
                .filter_map(|p| {
                    if p.path().is_dir() {
                        let file_name = p.file_name().into_string().unwrap();
                        if !file_name.starts_with('.') && file_name != "dist" {
                            Some(file_name)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect();
            vec.sort_unstable();
            vec
        };
        let (ws_send, _) = broadcast::channel(100);
        (
            Self {
                output_path: Arc::new(RwLock::new(current_dir.get(0).cloned())),
                current_dir,
                new_dir_input: String::new(),
                instructions: {
                    if let Ok(ip) = local_ip_address::local_ip() {
                        format!("Open a web browser and type in {}:8080", ip)
                    } else {
                        String::from(
                            "You need to connect your computer to WiFi to use this uploader. Once \
                             connected, restart this app",
                        )
                    }
                },
                current_upload: Arc::new(RwLock::new(Slab::new())),

                download_path: Arc::new(RwLock::new(Vec::new())),
                clients_connected: 0,
                ws_send: Arc::new(ws_send),
            },
            Command::none(),
        )
    }

    fn title(&self) -> String {
        String::from("LocaDrive")
    }

    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        match &message {
            Message::ChangePath(p) => *self.output_path.blocking_write() = Some(p.clone()),
            Message::EditNewDir(p) => self.new_dir_input = p.clone(),
            Message::AddPath => {
                if !self.new_dir_input.is_empty() {
                    self.current_dir.push(self.new_dir_input.trim().to_string());
                    self.current_dir.sort_unstable();
                    *self.output_path.blocking_write() = Some(self.new_dir_input.clone());
                    self.new_dir_input.clear();
                }
            }
            Message::OpenFileDialog => {
                return Command::perform(rfd::AsyncFileDialog::new().pick_files(), |fh| {
                    let vec: Vec<PathBuf> = fh
                        .map(|f| f.into_iter().map(|f| f.path().to_path_buf()).collect())
                        .unwrap_or_default();
                    if vec.is_empty() {
                        Message::ClearDownloadPath
                    } else {
                        Message::ChoseFile(vec)
                    }
                });
            }
            Message::ChoseFile(p) => *self.download_path.blocking_write() = p.clone(),
            Message::ClearDownloadPath => {
                let mut download_path = self.download_path.blocking_write();
                if download_path.is_empty() {
                    return Command::none();
                } else {
                    download_path.clear();
                }
            }
            Message::Refresh => return Command::none(),
            Message::WSConnection => {
                self.clients_connected += 1;
                self.ws_send
                    .send(Message::EditNewDir(self.new_dir_input.clone()))
                    .unwrap();
                self.ws_send
                    .send(Message::RecvExisitingPaths(self.current_dir.clone()))
                    .unwrap();
                if let Some(path) = self.output_path.blocking_read().clone() {
                    self.ws_send.send(Message::ChangePath(path)).unwrap();
                }
                let download_path = self.download_path.blocking_read().clone();
                if !download_path.is_empty() {
                    self.ws_send
                        .send(Message::ChoseFile(download_path))
                        .unwrap();
                }
                return Command::none();
            }
            Message::WSClose => {
                self.clients_connected -= 1;
                return Command::none();
            }
            Message::FocusWindow => {
                return Command::batch([
                    Command::single(command::Action::Window(window::Action::GainFocus)),
                    Command::single(command::Action::Window(window::Action::Minimize(false))),
                ])
            }
            _ => {}
        }
        let _ = self.ws_send.send(message);
        Command::none()
    }

    fn view(&self) -> iced::Element<'_, Self::Message, iced::Renderer<Self::Theme>> {
        let mut column = column![Text::new("LocaDrive").size(40), self.instructions.as_str(),]
            .spacing(16)
            .padding(16)
            .align_items(Alignment::Center)
            .width(Length::Fill);
        if self.clients_connected > 0 {
            column = column.push(
                Text::new(format!("Clients connected: {}", self.clients_connected))
                    .style(Color::from_rgb(0.0, 1.0, 0.0)),
            );
        }
        column = column
            .push("Output to:")
            .push(
                row![
                    "Existing Folder:",
                    PickList::new(
                        self.current_dir.as_slice(),
                        self.output_path.blocking_read().clone(),
                        Message::ChangePath,
                    ),
                    "OR, New Folder:",
                    TextInput::new(
                        "Type the folder to create and press enter",
                        &self.new_dir_input
                    )
                    .on_input(Message::EditNewDir)
                    .on_submit(Message::AddPath),
                ]
                .spacing(16),
            )
            .push(
                row![
                    "File to transfer to phone:",
                    Button::new("Select file").on_press(Message::OpenFileDialog),
                    {
                        let mut button = Button::new("Clear path");
                        if !self.download_path.blocking_read().is_empty() {
                            button = button.on_press(Message::ClearDownloadPath);
                        }
                        button
                    }
                ]
                .spacing(16),
            );
        for (_, upload) in self.current_upload.blocking_read().iter() {
            column = column
                .push(Text::new(format!(
                    "{}/{}",
                    upload
                        .0
                        .files_uploaded
                        .load(std::sync::atomic::Ordering::Relaxed),
                    upload
                        .0
                        .num_files
                        .load(std::sync::atomic::Ordering::Relaxed)
                )))
                .push(ProgressBar::new(
                    0.0..=upload
                        .0
                        .num_files
                        .load(std::sync::atomic::Ordering::Relaxed),
                    upload
                        .0
                        .files_uploaded
                        .load(std::sync::atomic::Ordering::Relaxed),
                ));
            for (_, upload) in upload.1.blocking_read().iter() {
                column = column
                    .push(Text::new(upload.name.clone()))
                    .push(ProgressBar::new(
                        0.0..=upload.num_bytes.load(std::sync::atomic::Ordering::Relaxed),
                        upload
                            .bytes_uploaded
                            .load(std::sync::atomic::Ordering::Relaxed),
                    ));
            }
        }
        Scrollable::new(column).into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let output_path = Arc::clone(&self.output_path);
        let download_path = Arc::clone(&self.download_path);
        let progress_bars = Arc::clone(&self.current_upload);
        let ws_send = Arc::clone(&self.ws_send);
        subscription::channel(std::any::TypeId::of::<Router>(), 100, move |output| {
            let output_path = Arc::clone(&output_path);
            let download_path = Arc::clone(&download_path);
            let progress_bars = Arc::clone(&progress_bars);
            let ws_send = ws_send.clone();
            async move {
                let app = Router::new()
                    .route(
                        "/",
                        get({
                            |request: Request<Body>| async move {
                                let service = Compression::new(ServeFile::new("dist/index.html"));
                                let result = service.oneshot(request).await;
                                result.into_response()
                            }
                        }),
                    )
                    .route(
                        "/upload/:key",
                        post({
                            let p = Arc::clone(&progress_bars);
                            let mut o = output.clone();
                            |TypedHeader(content_length): TypedHeader<ContentLength>,
                             TypedHeader(content_encoding): TypedHeader<ContentEncoding>,
                             Path(key): Path<String>,
                             req: Request<Body>| async move {
                                async move {
                                    let file_name = req
                                        .headers()
                                        .get("File-Name")
                                        .unwrap()
                                        .to_str()
                                        .unwrap()
                                        .to_string();
                                    let progress_bar = Arc::new(UploadStatus {
                                        name: format!("Upload: {}", file_name),
                                        num_bytes: AtomicF32::new(content_length.0 as f32),
                                        bytes_uploaded: AtomicF32::new(0.0),
                                    });
                                    let key: usize = key.parse().unwrap();
                                    let pb_arc = Arc::clone(&p.read().await.get(key).unwrap());
                                    let pb =
                                        pb_arc.1.write().await.insert(Arc::clone(&progress_bar));
                                    let rdr = StreamReader::new(
                                        req.into_body()
                                            .map_err(|e| std::io::Error::new(ErrorKind::Other, e)),
                                    );
                                    let path = std::env::current_dir().unwrap().join(
                                        output_path
                                            .read()
                                            .await
                                            .as_ref()
                                            .map(|s| s.as_str())
                                            .unwrap_or("out"),
                                    );

                                    if !path.exists() {
                                        std::fs::create_dir(&path)?;
                                    }

                                    let path = path.join(file_name);

                                    let mut file = BufWriter::new(File::create(path).await?);
                                    if content_encoding.contains("zstd") {
                                        let reader = ZstdDecoder::new(rdr);

                                        let mut adapter = ProgressReadAdapter(
                                            reader,
                                            &mut o,
                                            &progress_bar.bytes_uploaded,
                                            Instant::now(),
                                        );
                                        tokio::io::copy(&mut adapter, &mut file).await?;
                                    } else {
                                        let mut adapter = ProgressReadAdapter(
                                            rdr,
                                            &mut o,
                                            &progress_bar.bytes_uploaded,
                                            Instant::now(),
                                        );
                                        tokio::io::copy(&mut adapter, &mut file).await?;
                                    }
                                    file.shutdown().await?;
                                    {
                                        pb_arc
                                            .0
                                            .files_uploaded
                                            .fetch_add(1.0, std::sync::atomic::Ordering::Relaxed);
                                        pb_arc.1.write().await.remove(pb);
                                    }
                                    o.try_send(Message::Refresh).map_err(|e| {
                                        std::io::Error::new(ErrorKind::ConnectionReset, e)
                                    })?;
                                    Ok::<(), std::io::Error>(())
                                }
                                .await
                                .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
                            }
                        })
                        .put({
                            let p = Arc::clone(&progress_bars);
                            let mut o = output.clone();
                            |Path(num_files): Path<String>| async move {
                                let progress_bar = Arc::new((
                                    ConcurrentUploadStatus {
                                        num_files: AtomicF32::new(num_files.parse().unwrap()),
                                        files_uploaded: AtomicF32::new(0.0),
                                    },
                                    RwLock::new(Slab::new()),
                                ));
                                let mut w = p.write().await;
                                let res = w.insert(Arc::clone(&progress_bar)).to_string();
                                o.try_send(Message::Refresh)
                                    .map_err(|e| std::io::Error::new(ErrorKind::ConnectionReset, e))
                                    .unwrap();
                                res
                            }
                        })
                        .delete({
                            let p = Arc::clone(&progress_bars);
                            let mut o = output.clone();
                            |Path(key): Path<String>| async move {
                                p.write().await.remove(key.parse().unwrap());
                                o.try_send(Message::Refresh)
                                    .map_err(|e| std::io::Error::new(ErrorKind::ConnectionReset, e))
                                    .unwrap();
                            }
                        }),
                    )
                    .route(
                        "/dl/gz/:file_id",
                        get({
                            let d = Arc::clone(&download_path);
                            let o = output.clone();
                            let pb = Arc::clone(&progress_bars);
                            || async move {
                                Response::new(
                                    ZipStreamer::<GzipEncoder<Vec<u8>>>::new(
                                        d.read().await.to_owned(),
                                        pb,
                                        o,
                                    )
                                    .await,
                                )
                            }
                        }),
                    )
                    .route(
                        "/dl/zstd/:file_id",
                        get({
                            let d = Arc::clone(&download_path);
                            let o = output.clone();
                            let pb = Arc::clone(&progress_bars);
                            || async move {
                                Response::new(
                                    ZipStreamer::<ZstdEncoder<Vec<u8>>>::new(
                                        d.read().await.to_owned(),
                                        pb,
                                        o,
                                    )
                                    .await,
                                )
                            }
                        }),
                    )
                    .route(
                        "/dl/:file_id",
                        get({
                            let d = Arc::clone(&download_path);
                            let o = output.clone();
                            let pb = Arc::clone(&progress_bars);
                            |request: Request<Body>| async move {
                                let download = d.read().await;
                                if !download.is_empty() {
                                    if download.len() <= 1 {
                                        let entry = download.get(0).unwrap();
                                        let service = Compression::new(ServeFile::new(entry));
                                        let result = service.oneshot(request).await;
                                        result.into_response()
                                    } else {
                                        let content_encoding =
                                            request.headers().get(ACCEPT_ENCODING);
                                        if let Some(ce) = content_encoding {
                                            if ce.to_str().unwrap().contains("zstd") {
                                                let mut resp = Response::new(axum::body::boxed(
                                                    ZipStreamer::<ZstdEncoder<Vec<u8>>>::new(
                                                        download.to_owned(),
                                                        pb,
                                                        o,
                                                    )
                                                    .await,
                                                ));
                                                resp.headers_mut().append(
                                                    "Content-Encoding",
                                                    HeaderValue::from_static("zstd"),
                                                );
                                                resp
                                            } else if ce.to_str().unwrap().contains("gzip") {
                                                let mut resp = Response::new(axum::body::boxed(
                                                    ZipStreamer::<GzipEncoder<Vec<u8>>>::new(
                                                        download.to_owned(),
                                                        pb,
                                                        o,
                                                    )
                                                    .await,
                                                ));
                                                resp.headers_mut().append(
                                                    "Content-Encoding",
                                                    HeaderValue::from_static("gzip"),
                                                );
                                                resp
                                            } else {
                                                Response::new(axum::body::boxed(
                                                    ZipStreamer::<Vec<u8>>::new(
                                                        download.to_owned(),
                                                        pb,
                                                        o,
                                                    )
                                                    .await,
                                                ))
                                            }
                                        } else {
                                            Response::new(axum::body::boxed(
                                                ZipStreamer::<Vec<u8>>::new(
                                                    download.to_owned(),
                                                    pb,
                                                    o,
                                                )
                                                .await,
                                            ))
                                        }
                                    }
                                } else {
                                    ().into_response()
                                }
                            }
                        }),
                    )
                    .route(
                        "/focus",
                        get({
                            let mut o = output.clone();
                            || async move {
                                o.try_send(Message::FocusWindow).unwrap();
                            }
                        }),
                    )
                    .route(
                        "/ws",
                        get({
                            let o = output.clone();
                            let ws_send = Arc::clone(&ws_send);
                            |ws: WebSocketUpgrade| async move {
                                ws.on_upgrade(move |ws| websocket(ws, o, ws_send.subscribe()))
                            }
                        }),
                    )
                    .fallback_service(Compression::new(ServeDir::new("dist")))
                    .layer(DefaultBodyLimit::disable());

                axum::Server::bind(&"0.0.0.0:8080".parse().unwrap())
                    .serve(app.into_make_service())
                    .await
                    .unwrap();
                std::process::exit(0)
            }
        })
    }
}

async fn websocket(
    ws: WebSocket,
    mut sender: Sender<Message>,
    mut ws_recv: broadcast::Receiver<Message>,
) {
    let (mut tx, mut rx) = ws.split();
    sender.try_send(Message::WSConnection).unwrap();

    loop {
        tokio::select! {
            message = ws_recv.recv() => {
                let _ = tx.send(ws::Message::Binary(
                    postcard::to_allocvec(&message.unwrap()).unwrap(),
                ))
                .await;
            }
            message = rx.next() => {
                match message {
                    Some(Ok(ws::Message::Binary(msg))) => {
                        let message: Message = postcard::from_bytes(msg.as_slice()).unwrap();
                        sender.try_send(message).unwrap();
                    }
                    Some(Ok(ws::Message::Close(_))) => {
                        sender.try_send(Message::WSClose).unwrap();
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

#[pin_project]
struct ProgressReadAdapter<'a, R: AsyncRead>(
    #[pin] pub R,
    pub &'a mut Sender<Message>,
    pub &'a AtomicF32,
    pub Instant,
);

impl<'a, R: AsyncRead> AsyncRead for ProgressReadAdapter<'a, R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.project();

        let before = buf.filled().len();
        let result = this.0.poll_read(cx, buf);
        let after = buf.filled().len();

        this.2.fetch_add(
            (after - before) as f32,
            std::sync::atomic::Ordering::Relaxed,
        );
        if this.3.elapsed() > Duration::from_millis(500) {
            this.1.try_send(Message::Refresh).unwrap();
            *this.3 = Instant::now();
        }

        result
    }
}

#[pin_project]
struct ProgressWriteAdapter<W: AsyncWrite>(
    #[pin] pub W,
    pub Sender<Message>,
    pub Arc<UploadStatus>,
    pub Instant,
);

impl<W: AsyncWrite> AsyncWrite for ProgressWriteAdapter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.project();

        let result = this.0.poll_write(cx, buf);

        match result {
            Poll::Ready(Ok(r)) => {
                this.2
                    .bytes_uploaded
                    .fetch_add(r as f32, std::sync::atomic::Ordering::Relaxed);
                if this.3.elapsed() > Duration::from_millis(500) {
                    this.1.try_send(Message::Refresh).unwrap();
                    *this.3 = Instant::now();
                }
                Poll::Ready(Ok(r))
            }
            err @ Poll::Ready(Err(_)) => err,
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().0.poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().0.poll_shutdown(cx)
    }
}

macro_rules! buf_vec {
    () => {
        Vec::with_capacity(8 * 1024)
    };
}

enum ZipState {
    Reading,
    WaitingOnLock,
    None,
}

trait CompressorEncoder: AsyncWrite + Unpin + Send + 'static {
    fn new_buffered_vec() -> Self;
    fn new_vec() -> Self;
    fn get_vec(self) -> Vec<u8>;
    fn get_mut_vec(&mut self) -> &mut Vec<u8>;
    fn get_ref_vec(&self) -> &Vec<u8>;
}

impl CompressorEncoder for GzipEncoder<Vec<u8>> {
    #[inline(always)]
    fn new_buffered_vec() -> Self {
        GzipEncoder::new(buf_vec!())
    }

    #[inline(always)]
    fn new_vec() -> Self {
        GzipEncoder::new(Vec::new())
    }

    #[inline(always)]
    fn get_vec(self) -> Vec<u8> {
        self.into_inner()
    }

    #[inline(always)]
    fn get_mut_vec(&mut self) -> &mut Vec<u8> {
        self.get_mut()
    }

    #[inline(always)]
    fn get_ref_vec(&self) -> &Vec<u8> {
        self.get_ref()
    }
}

impl CompressorEncoder for ZstdEncoder<Vec<u8>> {
    #[inline(always)]
    fn new_buffered_vec() -> Self {
        ZstdEncoder::new(buf_vec!())
    }

    #[inline(always)]
    fn new_vec() -> Self {
        ZstdEncoder::new(Vec::new())
    }

    #[inline(always)]
    fn get_vec(self) -> Vec<u8> {
        self.into_inner()
    }

    #[inline(always)]
    fn get_mut_vec(&mut self) -> &mut Vec<u8> {
        self.get_mut()
    }

    #[inline(always)]
    fn get_ref_vec(&self) -> &Vec<u8> {
        self.get_ref()
    }
}

impl CompressorEncoder for Vec<u8> {
    #[inline(always)]
    fn new_buffered_vec() -> Self {
        buf_vec!()
    }

    #[inline(always)]
    fn new_vec() -> Self {
        Vec::new()
    }

    #[inline(always)]
    fn get_vec(self) -> Vec<u8> {
        self
    }

    #[inline(always)]
    fn get_mut_vec(&mut self) -> &mut Vec<u8> {
        self
    }

    #[inline(always)]
    fn get_ref_vec(&self) -> &Vec<u8> {
        self
    }
}

struct ZipStreamer<C: CompressorEncoder> {
    tar: tokio_tar::Builder<ProgressWriteAdapter<C>>,
    path_iter: std::vec::IntoIter<PathBuf>,
    zip_state: ZipState,
    future: BoxFuture<'static, std::io::Result<()>>,
    upload_statuses: CurrentUpload,
    key: usize,
    upload_status_con: InnerCurrentUpload,
}

impl<C: CompressorEncoder> ZipStreamer<C> {
    async fn new(
        files: Vec<PathBuf>,
        pb: CurrentUpload,
        sender: Sender<Message>,
    ) -> ZipStreamer<C> {
        let len = files.len();
        let mut len_total = 0;
        for p in files.iter() {
            len_total += p.metadata().unwrap().len();
        }
        let iter = files.into_iter();
        let mut status_slab = Slab::new();
        let status = Arc::new(UploadStatus {
            name: format!("Download: {} files", len),
            num_bytes: AtomicF32::new(len_total as f32),
            bytes_uploaded: AtomicF32::new(0.0),
        });
        status_slab.insert(Arc::clone(&status));
        let status_con = Arc::new((
            ConcurrentUploadStatus {
                num_files: AtomicF32::new(len as f32),
                files_uploaded: AtomicF32::new(-1.0),
            },
            RwLock::new(status_slab),
        ));
        let writer = tokio_tar::Builder::new(ProgressWriteAdapter(
            C::new_buffered_vec(),
            sender,
            Arc::clone(&status),
            Instant::now(),
        ));
        let key = pb.write().await.insert(Arc::clone(&status_con));
        Self {
            tar: writer,
            path_iter: iter,
            zip_state: ZipState::Reading,
            future: Box::pin(future::ready(Ok(()))),
            upload_statuses: pb,
            key,
            upload_status_con: status_con,
        }
    }
}

impl<C: CompressorEncoder> axum::body::HttpBody for ZipStreamer<C> {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_data(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut();

        match this.zip_state {
            ZipState::Reading => {
                let s0: &'static mut tokio_tar::Builder<ProgressWriteAdapter<C>> =
                    unsafe { std::mem::transmute::<&mut _, &'static mut _>(&mut this.tar) };
                match this.future.as_mut().poll(cx) {
                    Poll::Ready(r) => {
                        if let Err(e) = r {
                            return Poll::Ready(Some(Err(e)));
                        }
                        this.upload_status_con
                            .0
                            .files_uploaded
                            .fetch_add(1.0, std::sync::atomic::Ordering::Relaxed);
                        if let Some(next) = this.path_iter.next() {
                            let mut vec = buf_vec!();
                            std::mem::swap(s0.get_mut().0.get_mut_vec(), &mut vec);
                            let file_name = next.file_name().unwrap().to_os_string();
                            this.future = Box::pin(s0.append_path_with_name(next, file_name));

                            Poll::Ready(Some(Ok(vec.into())))
                        } else {
                            match Box::pin(s0.finish()).poll_unpin(cx) {
                                Poll::Ready(r) => {
                                    if let Err(e) = r {
                                        return Poll::Ready(Some(Err(e)));
                                    }
                                    // Encoder will never be used again
                                    let mut encoder = C::new_vec();
                                    std::mem::swap(&mut this.tar.get_mut().0, &mut encoder);
                                    match AsyncWrite::poll_shutdown(Pin::new(&mut encoder), cx) {
                                        Poll::Ready(r) => {
                                            if let Err(e) = r {
                                                return Poll::Ready(Some(Err(e)));
                                            }
                                            this.zip_state = ZipState::WaitingOnLock;
                                            match Box::pin(this.upload_statuses.write())
                                                .poll_unpin(cx)
                                            {
                                                Poll::Ready(mut s4) => {
                                                    s4.remove(this.key);
                                                    this.tar
                                                        .get_mut()
                                                        .1
                                                        .try_send(Message::Refresh)
                                                        .unwrap();
                                                    this.zip_state = ZipState::None;
                                                }
                                                Poll::Pending => {}
                                            }
                                            Poll::Ready(Some(Ok(encoder.get_vec().into())))
                                        }
                                        Poll::Pending => unreachable!(),
                                    }
                                }
                                Poll::Pending => unreachable!(),
                            }
                        }
                    }
                    Poll::Pending => {
                        if !s0.get_ref().0.get_ref_vec().is_empty() {
                            let mut vec = buf_vec!();
                            std::mem::swap(s0.get_mut().0.get_mut_vec(), &mut vec);

                            Poll::Ready(Some(Ok(vec.into())))
                        } else {
                            Poll::Pending
                        }
                    }
                }
            }
            ZipState::WaitingOnLock => {
                match Box::pin(this.upload_statuses.write()).poll_unpin(cx) {
                    Poll::Ready(mut s4) => {
                        s4.remove(this.key);
                        this.tar.get_mut().1.try_send(Message::Refresh).unwrap();
                        this.zip_state = ZipState::None;
                        Poll::Ready(None)
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            ZipState::None => Poll::Ready(None),
        }
    }

    fn poll_trailers(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<axum::http::HeaderMap>, Self::Error>> {
        std::task::Poll::Ready(Ok(None))
    }
}

fn panic_hook(info: &PanicInfo) {
    let backtrace = Backtrace::force_capture();
    let crash_text = format!("{info}zstd{backtrace}");

    let win = PanicWindow::new(Style::default(), crash_text);

    let mut app = PanicApplication::new(win);
    app.run().unwrap();
}

fn main() -> Result<(), iced::Error> {
    std::panic::set_hook(Box::new(panic_hook));
    if ureq::get("http://127.0.0.1:8080/focus").call().is_err() {
        Uploader::run(Settings::default())
    } else {
        Ok(())
    }
}
