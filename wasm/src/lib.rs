use futures::io::{AsyncRead, Error};
use futures::FutureExt;
use std::borrow::Cow;
use std::future::Future;
use std::panic;
use std::pin::Pin;
use std::task::{Context, Poll};

use magic_wormhole::{transfer, transit, AppID, Code, Wormhole};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    fn alert(s: &str);

    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

macro_rules! console_log {
    ($($t:tt)*) => (log(&format_args!($($t)*).to_string()))
}

#[wasm_bindgen]
pub fn init() {
    panic::set_hook(Box::new(console_error_panic_hook::hook));
}

struct NoOpFuture {}

impl Future for NoOpFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

#[wasm_bindgen]
pub struct ClientConfig {
    appid: AppID,
    rendezvous_url: String,
    transit_server_url: String,
    passphrase_component_len: usize,
}

#[wasm_bindgen]
impl ClientConfig {
    pub fn client_init(
        appid: &str,
        rendezvous_url: &str,
        transit_server_url: &str,
        passphrase_component_len: usize,
    ) -> Self {
        Self {
            appid: appid.to_string().into(),
            rendezvous_url: rendezvous_url.to_string(),
            transit_server_url: transit_server_url.to_string(),
            passphrase_component_len: passphrase_component_len,
        }
    }

    pub async fn send(&self, file: web_sys::File) -> Result<JsValue, JsValue> {
        match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
            Ok(file_content) => {
                let array = js_sys::Uint8Array::new(&file_content);
                let len = array.byte_length() as u64;
                console_log!("Read raw data ({} bytes)", len);

                console_log!("connecting...");

                let rendezvous = Box::new(self.rendezvous_url.as_str());
                let config =
                    transfer::APP_CONFIG.rendezvous_url(Cow::Owned(rendezvous.to_string()));
                let connect = Wormhole::connect_without_code(config, 2);

                match connect.await {
                    Ok((server_welcome, wormhole_future)) => {
                        console_log!("wormhole code:  {}", server_welcome.code);

                        match wormhole_future.await {
                            Ok(wormhole) => {
                                console_log!("receiver connected {:?}", wormhole);
                                let file_name = file.name();
                                let file_size = file.size() as u64;
                                let mut file_wrapper = FileWrapper::new(file);
                                match transfer::send_file(
                                    wormhole,
                                    vec![transit::RelayHint::new(
                                        None,
                                        vec![],
                                        vec![url::Url::parse(&self.transit_server_url).unwrap()],
                                    )],
                                    &mut file_wrapper,
                                    file_name,
                                    file_size,
                                    transit::Abilities::FORCE_RELAY,
                                    |info| {
                                        console_log!("Connected to '{:?}'", info);
                                    },
                                    |cur, total| {
                                        console_log!("Progress: {}/{}", cur, total);
                                    },
                                    NoOpFuture {},
                                )
                                .await
                                {
                                    Ok(_) => {
                                        console_log!("Data sent");
                                        Ok(0.into())
                                    },
                                    Err(e) => {
                                        console_log!("Error in data transfer: {:?}", e);
                                        Err(1.into())
                                    },
                                }
                            },
                            Err(_) => {
                                console_log!("Error waiting for connection");
                                Err(1.into())
                            },
                        }
                    },
                    Err(_) => {
                        console_log!("Error waiting for connection");
                        Err(1.into())
                    },
                }
            },
            Err(_) => {
                console_log!("Error reading file");
                Err(1.into())
            },
        }
    }

    pub async fn receive(&self, code: String) -> Option<JsValue> {
        let rendezvous = Box::new(self.rendezvous_url.as_str());
        let connect = Wormhole::connect_with_code(
            transfer::APP_CONFIG.rendezvous_url(Cow::Owned(rendezvous.to_string())),
            Code(code),
        );

        return match connect.await {
            Ok((_, wormhole)) => {
                let req = transfer::request_file(
                    wormhole,
                    vec![transit::RelayHint::new(
                        None,
                        vec![],
                        vec![url::Url::parse(&self.transit_server_url).unwrap()],
                    )],
                    transit::Abilities::FORCE_RELAY,
                    NoOpFuture {},
                )
                .await;

                let mut file: Vec<u8> = Vec::new();

                match req {
                    Ok(Some(req)) => {
                        let filename = req.filename.clone();
                        let filesize = req.filesize;
                        console_log!("File name: {:?}, size: {}", filename, filesize);
                        let file_accept = req.accept(
                            |info| {
                                console_log!("Connected to '{:?}'", info);
                            },
                            |cur, total| {
                                console_log!("Progress: {}/{}", cur, total);
                            },
                            &mut file,
                            NoOpFuture {},
                        );

                        match file_accept.await {
                            Ok(_) => {
                                console_log!("Data received, length: {}", file.len());
                                let result = ReceiveResult {
                                    data: file,
                                    filename: filename.to_str().unwrap_or_default().into(),
                                    filesize,
                                };
                                return Some(serde_wasm_bindgen::to_value(&result).unwrap());
                            },
                            Err(e) => {
                                console_log!("Error in data transfer: {:?}", e);
                                None
                            },
                        }
                    },
                    _ => {
                        console_log!("No ReceiveRequest");
                        None
                    },
                }
            },
            Err(_) => {
                console_log!("Error in connection");
                None
            },
        };
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ReceiveResult {
    data: Vec<u8>,
    filename: String,
    filesize: u64,
}

struct FileWrapper {
    file: web_sys::File,
    size: i32,
    index: i32,
    future: Option<JsFuture>,
}

impl FileWrapper {
    fn new(file: web_sys::File) -> Self {
        let size = file.size();
        FileWrapper {
            file: file,
            size: size as i32,
            index: 0,
            future: None,
        }
    }
}

impl AsyncRead for FileWrapper {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        let start = self.index;
        let end = i32::min(start + buf.len() as i32, self.size);
        let size = end - start;

        let blob = self.file.slice_with_i32_and_i32(start, end).unwrap();

        match &mut self.future {
            Some(x) => match x.poll_unpin(cx) {
                Poll::Ready(x) => {
                    js_sys::Uint8Array::new(&x.unwrap()).copy_to(buf);
                    self.index += size;
                    self.future = Some(blob.array_buffer().into());

                    Poll::Ready(Ok(size as usize))
                },
                Poll::Pending => Poll::Pending,
            },
            None => {
                self.future = Some(blob.array_buffer().into());
                Poll::Pending
            },
        }
    }
}
