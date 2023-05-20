use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Deref,
    task::Poll,
};

use async_compression::{self, futures::write::ZstdEncoder};
use base64::{
    alphabet::URL_SAFE,
    engine::{GeneralPurpose, GeneralPurposeConfig},
    Engine,
};
use futures_util::{ready, AsyncRead, AsyncWriteExt, Future, StreamExt};
use js_sys::{ArrayBuffer, AsyncIterator, Function, IteratorNext, Reflect, Symbol, Uint8Array};
use pin_project::pin_project;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{prelude::Closure, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, CloseEvent, Event, FileList, Headers, HtmlInputElement, HtmlSelectElement, MessageEvent,
    Request, RequestInit, Response, WebSocket,
};

mod bindgen;

use bindgen::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Message<'a> {
    ChangePath(&'a str),
    EditNewDir(&'a str),
    ChoseFile(Vec<&'a str>),
    OpenFileDialog,
    ClearDownloadPath,
    Refresh,
    AddPath,
    RecvExistingPaths(Vec<&'a str>),

    // Backend exclusive
    WSConnection,
    WSClose,
    FocusWindow,
}

struct FileListIter(pub FileList, u32);

impl FileListIter {
    fn new(file_list: FileList) -> Self {
        Self(file_list, 0)
    }
}

impl Iterator for FileListIter {
    type Item = web_sys::File;

    fn next(&mut self) -> Option<Self::Item> {
        let res = self.0.get(self.1);
        self.1 += 1;
        res
    }
}

#[pin_project]
struct AsyncIterReader {
    pub iter: AsyncIterator,
    #[pin]
    pub future: JsFuture,
    pub state: ReadState,
}

enum ReadState {
    Ready { chunk: Vec<u8>, chunk_start: usize },
    PendingChunk,
    Eof,
}

unsafe impl Send for AsyncIterReader {}
unsafe impl Sync for AsyncIterReader {}

impl AsyncRead for AsyncIterReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut this = self.project();

        loop {
            match this.state {
                ReadState::Ready { chunk, chunk_start } => {
                    let len = buf.len().min(chunk.len() - *chunk_start);

                    buf[..len].copy_from_slice(&chunk[*chunk_start..*chunk_start + len]);
                    *chunk_start += len;

                    if chunk.len() == *chunk_start {
                        *this.state = ReadState::PendingChunk;
                    }

                    return Poll::Ready(Ok(len));
                }
                ReadState::PendingChunk => match ready!(this.future.as_mut().poll(cx)) {
                    Ok(chunk) => {
                        let next: IteratorNext = chunk.unchecked_into();
                        if next.done() {
                            *this.state = ReadState::Eof;
                            return Poll::Ready(Ok(0));
                        } else {
                            let array: Uint8Array = next.value().dyn_into().unwrap();

                            *this.state = ReadState::Ready {
                                chunk: array.to_vec(),
                                chunk_start: 0,
                            };
                            *this.future = JsFuture::from(this.iter.next().unwrap());
                        }
                    }
                    Err(_) => {
                        *this.state = ReadState::Eof;
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "An error occurred",
                        )));
                    }
                },
                ReadState::Eof => {
                    return Poll::Ready(Ok(0));
                }
            }
        }
    }
}

pub fn main() {
    std::panic::set_hook(Box::new(console_error_panic_hook::hook));
    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();
    let location = document.location().unwrap();
    let hostname = location.hostname();
    let file_node = document.get_element_by_id("file_input").unwrap();
    let num_files_el: HtmlInputElement = document
        .get_element_by_id("num_files")
        .unwrap()
        .unchecked_into();
    let on_file_change = Closure::<dyn FnMut(_)>::new(move |e: Event| {
        let target = match e.target() {
            Some(target) => target,
            None => return,
        };

        let el = JsCast::unchecked_ref::<HtmlInputElement>(&target);
        let files = el.files().unwrap();
        let num_files = files.length();
        num_files_el.set_value(&num_files.to_string());
    });

    file_node
        .add_event_listener_with_callback("change", on_file_change.as_ref().unchecked_ref())
        .unwrap();
    on_file_change.forget();

    let new_dir_node: HtmlInputElement = document
        .get_element_by_id("new_directory")
        .unwrap()
        .unchecked_into();
    let new_dir_button_node = document.get_element_by_id("new_dir_button").unwrap();
    let ws = WebSocket::new(&format!("ws://{}:8080/ws", hostname.unwrap())).unwrap();
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
    let on_new_dir_change = Closure::<dyn FnMut(_)>::new({
        let ws = ws.clone();
        move |e: Event| {
            let target = match e.target() {
                Some(target) => target,
                None => return,
            };

            let el = JsCast::unchecked_ref::<HtmlInputElement>(&target);
            ws.send_with_u8_array(
                &postcard::to_allocvec(&Message::EditNewDir(&el.value())).unwrap(),
            )
            .unwrap();
        }
    });

    new_dir_node
        .add_event_listener_with_callback("input", on_new_dir_change.as_ref().unchecked_ref())
        .unwrap();
    on_new_dir_change.forget();

    let on_new_dir_button = Closure::<dyn FnMut(_)>::new({
        let ws = ws.clone();
        move |_: Event| {
            ws.send_with_u8_array(&postcard::to_allocvec(&Message::AddPath).unwrap())
                .unwrap();
        }
    });

    new_dir_button_node
        .add_event_listener_with_callback("click", on_new_dir_button.as_ref().unchecked_ref())
        .unwrap();
    on_new_dir_button.forget();

    let existing_dir_select: HtmlSelectElement = document
        .get_element_by_id("exist_dir_select")
        .unwrap()
        .unchecked_into();

    let on_dir_select = Closure::<dyn FnMut(_)>::new({
        let ws = ws.clone();
        move |e: Event| {
            let target = match e.target() {
                Some(target) => target,
                None => return,
            };

            let el = JsCast::unchecked_ref::<HtmlSelectElement>(&target);
            ws.send_with_u8_array(
                &postcard::to_allocvec(&Message::ChangePath(&el.value())).unwrap(),
            )
            .unwrap();
        }
    })
    .into_js_value();

    existing_dir_select
        .add_event_listener_with_callback("change", on_dir_select.unchecked_ref())
        .unwrap();

    let submit_button_node = document.get_element_by_id("submit_button").unwrap();
    let upload_complete_header = document
        .get_element_by_id("upload_complete_header")
        .unwrap();
    let on_submit_button = Closure::<dyn FnMut(_)>::new(move |_: Event| {
        let file_el: &HtmlInputElement = file_node.unchecked_ref();
        let iter = FileListIter::new(file_el.files().unwrap());
        let window = window.clone();
        let upload_complete_header = upload_complete_header.clone();
        wasm_bindgen_futures::spawn_local(async move {
            upload_complete_header
                .set_attribute("style", "visibility: hidden")
                .unwrap();
            let key: u64 = {
                let mut opts = RequestInit::new();
                opts.method("PUT");

                let request =
                    Request::new_with_str_and_init(&format!("/upload/{}", iter.0.length()), &opts)
                        .unwrap();
                let resp: Response = JsFuture::from(window.fetch_with_request(&request))
                    .await
                    .unwrap()
                    .unchecked_into();
                let text: String = JsFuture::from(resp.text().unwrap())
                    .await
                    .unwrap()
                    .as_string()
                    .unwrap();
                text.parse().unwrap()
            };
            let async_window = window.clone();
            futures_util::stream::iter(iter.filter_map(move |f| {
                let async_window = async_window.clone();
                let headers = Headers::new().unwrap();
                if headers
                    .append("File-Name", &deunicode::deunicode(&f.name()))
                    .is_err()
                {
                    None
                } else {
                    Some(async move {
                        let mut opts = RequestInit::new();
                        opts.method("POST");

                        let f_type = f.type_();
                        // Don't compress pre-compressed formats
                        if f_type.starts_with("image")
                            || f_type.starts_with("video")
                            || (f_type.starts_with("audio") && f_type != "audio/wav")
                        {
                            let blob: &Blob = f.deref();
                            opts.body(Some(blob));
                        } else {
                            let stream = f.stream();

                            let iter_sym = Symbol::async_iterator();
                            let iter_fn = Reflect::get(stream.as_ref(), iter_sym.as_ref()).unwrap();

                            let iter_fn: Function = iter_fn.unchecked_into();

                            let it: AsyncIterator =
                                iter_fn.call0(stream.as_ref()).unwrap().unchecked_into();

                            let future = it.next().map(|p| JsFuture::from(p)).unwrap();
                            let rdr = AsyncIterReader {
                                iter: it,
                                future,
                                state: ReadState::PendingChunk,
                            };

                            headers.append("Content-Encoding", "zstd").unwrap();
                            let mut result_encoded = ZstdEncoder::new(Vec::new());
                            futures_util::io::copy(rdr, &mut result_encoded)
                                .await
                                .unwrap();
                            result_encoded.flush().await.unwrap();
                            result_encoded.close().await.unwrap();
                            let final_array = result_encoded.into_inner();
                            let final_body = Uint8Array::from(final_array.as_slice());
                            opts.body(Some(final_body.as_ref()));
                        }
                        opts.headers(headers.as_ref());
                        let request =
                            Request::new_with_str_and_init(&format!("/upload/{}", key), &opts)
                                .unwrap();
                        JsFuture::from(async_window.fetch_with_request(&request))
                            .await
                            .unwrap();
                    })
                }
            }))
            .for_each_concurrent(3, |f| f)
            .await;

            let mut opts = RequestInit::new();
            opts.method("DELETE");

            let request =
                Request::new_with_str_and_init(&format!("/upload/{}", key), &opts).unwrap();
            let resp: Response = JsFuture::from(window.fetch_with_request(&request))
                .await
                .unwrap()
                .unchecked_into();
            JsFuture::from(resp.text().unwrap()).await.unwrap();
            upload_complete_header
                .set_attribute("style", "text-align: center")
                .unwrap();
        });
    })
    .into_js_value();

    submit_button_node
        .add_event_listener_with_callback("click", on_submit_button.unchecked_ref())
        .unwrap();

    let mut channel = Channel::default();

    let onmessage_callback = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
        if let Ok(buf) = e.data().dyn_into::<ArrayBuffer>() {
            let array = js_sys::Uint8Array::new(&buf).to_vec();
            let message: Message = postcard::from_bytes(array.as_slice()).unwrap();
            match message {
                Message::EditNewDir(s) => new_dir_node.set_value(&s),
                Message::AddPath => {
                    let value = new_dir_node.value();
                    channel.add_path(value.as_str());
                    channel.flush();
                }
                Message::ChoseFile(paths) => {
                    channel.unhide_transfer_buttons();
                    if paths.len() > 1 {
                        let paths_hash = {
                            let mut hasher = DefaultHasher::new();
                            paths.hash(&mut hasher);
                            let hash = hasher.finish();
                            GeneralPurpose::new(&URL_SAFE, GeneralPurposeConfig::new())
                                .encode(hash.to_ne_bytes())
                        };
                        let ph = &paths_hash[..memchr::memchr(b'=', paths_hash.as_bytes())
                            .unwrap_or_else(|| paths_hash.len())];
                        channel.set_transfer_buttons(
                            format_args!("/dl/{}.tar", ph),
                            format_args!("/dl/zstd/{}.tar.zst", ph),
                            format_args!("/dl/gz/{}.tar.gz", ph),
                        );
                    } else {
                        let ph = if &paths[0][1..3] == ":\\" {
                            &paths[0][memchr::memrchr(b'\\', paths[0].as_bytes()).unwrap() + 1..]
                        } else {
                            &paths[0][memchr::memrchr(b'/', paths[0].as_bytes()).unwrap() + 1..]
                        };
                        channel.set_transfer_buttons(
                            format_args!("/dl/{}", ph),
                            format_args!("/dl/zstd/{}.tar.zst", ph),
                            format_args!("/dl/gz/{}.tar.gz", ph),
                        );
                    }
                    channel.flush();
                }
                Message::ClearDownloadPath => {
                    channel.hide_transfer_buttons();
                    channel.flush();
                }
                Message::ChangePath(p) => existing_dir_select.set_value(p),
                Message::RecvExistingPaths(paths) => {
                    channel.clear_dir_select();
                    for p in paths {
                        channel.append_path(p);
                    }
                    channel.flush();
                }
                message => web_sys::console::log_1(&format!("{:#?}", message).into()),
            }
        }
    });
    let onclose_callback =
        Closure::<dyn FnMut(_)>::new(move |_: CloseEvent| location.reload().unwrap());
    ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
    ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
    onmessage_callback.forget();
    onclose_callback.forget();
}
