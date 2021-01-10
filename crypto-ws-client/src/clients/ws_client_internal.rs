use super::utils::connect_with_retry;
use super::ws_stream::WebSocketStream;
use std::{cell::RefCell, rc::Rc};

use std::time::{Duration, Instant};
use std::{collections::HashSet, thread};
use std::{
    io::prelude::*,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use flate2::read::{DeflateDecoder, GzDecoder};
use log::*;
use tungstenite::{Error, Message};

pub(super) enum MiscMessage {
    WebSocket(Message), // WebSocket message that needs to be sent to the server
    Reconnect,          // Needs to reconnect
    Misc,               // Misc message
    Normal,             // Normal message will be passed to on_msg
}

pub(super) struct WSClientInternal<'a> {
    exchange: &'static str, // Eexchange name
    pub(super) url: String, // Websocket base url
    ws_stream: RefCell<WebSocketStream>,
    channels: RefCell<HashSet<String>>, // subscribed channels
    on_msg: Rc<RefCell<dyn FnMut(String) + 'a>>, // user defined message callback
    on_misc_msg: fn(&str) -> MiscMessage, // handle misc messages
    // converts raw channels to subscribe/unsubscribe commands
    channels_to_commands: fn(&[String], bool) -> Vec<String>,
    should_stop: Arc<AtomicBool>, // used by close() and run()
}

impl<'a> WSClientInternal<'a> {
    pub fn new(
        exchange: &'static str,
        url: &str,
        on_msg: Rc<RefCell<dyn FnMut(String) + 'a>>,
        on_misc_msg: fn(&str) -> MiscMessage,
        channels_to_commands: fn(&[String], bool) -> Vec<String>,
    ) -> Self {
        let stream = connect_with_retry(url);
        WSClientInternal {
            exchange,
            url: url.to_string(),
            ws_stream: RefCell::new(WebSocketStream::new(stream)),
            on_msg,
            on_misc_msg,
            channels: RefCell::new(HashSet::new()),
            channels_to_commands,
            should_stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn subscribe(&self, channels: &[String]) {
        let mut diff = Vec::<String>::new();
        for ch in channels.iter() {
            if self.channels.borrow_mut().insert(ch.clone()) {
                diff.push(ch.clone());
            }
        }
        if !diff.is_empty() {
            let commands = (self.channels_to_commands)(channels, true);
            commands.into_iter().for_each(|command| {
                self.ws_stream
                    .borrow()
                    .write_message(Message::Text(command));
            });
        }
    }

    pub fn unsubscribe(&self, channels: &[String]) {
        let mut diff = Vec::<String>::new();
        for ch in channels.iter() {
            if self.channels.borrow_mut().remove(ch) {
                diff.push(ch.clone());
            }
        }
        if !diff.is_empty() {
            let commands = (self.channels_to_commands)(channels, false);
            commands.into_iter().for_each(|command| {
                self.ws_stream
                    .borrow()
                    .write_message(Message::Text(command));
            });
        }
    }

    // reconnect and subscribe all channels
    fn reconnect(&self) {
        warn!("Reconnecting to {}", &self.url);
        self.ws_stream
            .replace(WebSocketStream::new(connect_with_retry(self.url.as_str())));
        let channels = self
            .channels
            .borrow()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        if !channels.is_empty() {
            let commands = (self.channels_to_commands)(&channels, true);
            commands.into_iter().for_each(|command| {
                self.ws_stream
                    .borrow()
                    .write_message(Message::Text(command));
            });
        }
        // avoid too frequent reconnect
        std::thread::sleep(Duration::from_secs(5));
    }

    // Handle a text msg from Message::Text or Message::Binary
    // Returns true if gets a normal message, otherwise false
    fn handle_msg(&self, txt: &str) -> bool {
        match (self.on_misc_msg)(txt) {
            MiscMessage::Misc => false,
            MiscMessage::Reconnect => {
                self.reconnect();
                false
            }
            MiscMessage::WebSocket(ws_msg) => {
                self.ws_stream.borrow().write_message(ws_msg);
                false
            }
            MiscMessage::Normal => {
                if self.exchange == super::mxc::EXCHANGE_NAME
                    && self.url.as_str() == super::mxc::SPOT_WEBSOCKET_URL
                {
                    // special logic for MXC Spot
                    match txt.strip_prefix("42") {
                        Some(msg) => (self.on_msg.borrow_mut())(msg.to_string()),
                        None => error!(
                            "{}, Not possible, should be handled by {}.on_misc_msg() previously",
                            txt, self.exchange
                        ),
                    }
                } else {
                    (self.on_msg.borrow_mut())(txt.to_string());
                }
                true
            }
        }
    }

    pub fn run(&self, duration: Option<u64>) {
        // start the ping thread
        WSClientInternal::auto_ping(
            self.exchange,
            self.url.to_string(),
            self.ws_stream.borrow().clone(),
        );

        let now = Instant::now();
        while !self.should_stop.load(Ordering::Relaxed) {
            let resp = self.ws_stream.borrow().read_message();
            let normal = match resp {
                Ok(msg) => match msg {
                    Message::Text(txt) => self.handle_msg(&txt),
                    Message::Binary(binary) => {
                        let mut txt = String::new();
                        let resp = if self.exchange == super::huobi::EXCHANGE_NAME
                            || self.exchange == super::binance::EXCHANGE_NAME
                        {
                            let mut decoder = GzDecoder::new(&binary[..]);
                            decoder.read_to_string(&mut txt)
                        } else if self.exchange == super::okex::EXCHANGE_NAME {
                            let mut decoder = DeflateDecoder::new(&binary[..]);
                            decoder.read_to_string(&mut txt)
                        } else {
                            panic!("Unknown binary format from {}", self.url)
                        };

                        match resp {
                            Ok(_) => self.handle_msg(&txt),
                            Err(err) => {
                                error!("Decompression failed, {}", err);
                                false
                            }
                        }
                    }
                    Message::Ping(resp) => {
                        let tmp = std::str::from_utf8(&resp);
                        warn!("Received a ping frame: {}", tmp.unwrap());
                        false
                    }
                    Message::Pong(resp) => {
                        let tmp = std::str::from_utf8(&resp);
                        warn!("Received a pong frame: {}", tmp.unwrap());
                        false
                    }
                    Message::Close(resp) => {
                        match resp {
                            Some(frame) => warn!("Received a Message::Close message with a CloseFrame: code: {}, reason: {}", frame.code, frame.reason),
                            None => warn!("Received a close message without CloseFrame"),
                        }
                        false
                    }
                },
                Err(err) => {
                    match err {
                        Error::ConnectionClosed => {
                            warn!("tungstenite::Error::ConnectionClosed");
                            self.reconnect();
                        }
                        _ => error!("{}", err),
                    };
                    false
                }
            };

            if let Some(seconds) = duration {
                if now.elapsed() > Duration::from_secs(seconds) && normal {
                    break;
                }
            }
        }
    }

    pub fn close(&self) {
        self.should_stop.store(true, Ordering::Relaxed);
        self.ws_stream.borrow().close();
    }

    // Send ping per interval
    fn auto_ping(exchange: &'static str, url: String, ws_stream: WebSocketStream) {
        thread::spawn(move || {
            loop {
                let ping_msg = match exchange {
                    super::mxc::EXCHANGE_NAME => {
                        if url.as_str() == super::mxc::SPOT_WEBSOCKET_URL {
                            // ping per 5 seconds
                            Some((5, Message::Text("2".to_string()))) // socket.io ping
                        } else if url.as_str() == super::mxc::SWAP_WEBSOCKET_URL {
                            // ping per 10 seconds
                            Some((10, Message::Text(r#"{"method":"ping"}"#.to_string())))
                        } else {
                            None
                        }
                    }
                    // https://www.bitmex.com/app/wsAPI#Heartbeats
                    super::bitmex::EXCHANGE_NAME => Some((5, Message::Text("ping".to_string()))),
                    _ => None,
                };

                match ping_msg {
                    Some(msg) => {
                        ws_stream.write_message(msg.1);
                        thread::sleep(Duration::from_secs(msg.0));
                    }
                    None => return,
                }
            }
        });
    }
}

/// Define exchange specific client.
macro_rules! define_client {
    ($struct_name:ident, $exchange:ident, $default_url:ident, $channels_to_commands:ident, $on_misc_msg:ident) => {
        impl<'a> WSClient<'a> for $struct_name<'a> {
            fn new(
                on_msg: Rc<RefCell<dyn FnMut(String) + 'a>>,
                url: Option<&str>,
            ) -> $struct_name<'a> {
                let real_url = match url {
                    Some(endpoint) => endpoint,
                    None => $default_url,
                };
                $struct_name {
                    client: WSClientInternal::new(
                        $exchange,
                        real_url,
                        on_msg,
                        $on_misc_msg,
                        $channels_to_commands,
                    ),
                }
            }

            fn subscribe_trade(&self, channels: &[String]) {
                <$struct_name as Trade>::subscribe_trade(self, channels);
            }

            fn subscribe_orderbook(&self, channels: &[String]) {
                <$struct_name as OrderBook>::subscribe_orderbook(self, channels);
            }

            fn subscribe_orderbook_snapshot(&self, channels: &[String]) {
                <$struct_name as OrderBookSnapshot>::subscribe_orderbook_snapshot(self, channels);
            }

            fn subscribe_ticker(&self, channels: &[String]) {
                <$struct_name as Ticker>::subscribe_ticker(self, channels);
            }

            fn subscribe_bbo(&self, channels: &[String]) {
                <$struct_name as BBO>::subscribe_bbo(self, channels);
            }

            fn subscribe_candlestick(&self, pairs: &[String], interval: u32) {
                <$struct_name as Candlestick>::subscribe_candlestick(self, pairs, interval);
            }

            fn subscribe(&self, channels: &[String]) {
                self.client.subscribe(channels);
            }

            fn unsubscribe(&self, channels: &[String]) {
                self.client.unsubscribe(channels);
            }

            fn run(&self, duration: Option<u64>) {
                self.client.run(duration);
            }

            fn close(&self) {
                self.client.close();
            }
        }
    };
}
