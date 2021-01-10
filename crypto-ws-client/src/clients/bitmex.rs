use crate::WSClient;
use std::{cell::RefCell, rc::Rc};
use std::{collections::HashMap, time::Duration};

use super::{
    utils::CHANNEL_PAIR_DELIMITER,
    ws_client_internal::{MiscMessage, WSClientInternal},
    Candlestick, OrderBook, OrderBookSnapshot, Ticker, Trade, BBO,
};
use log::*;
use serde_json::Value;

pub(super) const EXCHANGE_NAME: &str = "BitMEX";

const WEBSOCKET_URL: &str = "wss://www.bitmex.com/realtime";

/// The WebSocket client for BitMEX.
///
/// BitMEX has Swap and Future markets.
///
///   * WebSocket API doc: <https://www.bitmex.com/app/wsAPI>
///   * Trading at: <https://www.bitmex.com/app/trade/>
pub struct BitMEXWSClient<'a> {
    client: WSClientInternal<'a>,
}

fn channels_to_commands(channels: &[String], subscribe: bool) -> Vec<String> {
    let channels_to_parse: Vec<&String> =
        channels.iter().filter(|ch| !ch.starts_with('{')).collect();
    let mut all_commands: Vec<String> = channels
        .iter()
        .filter(|ch| ch.starts_with('{'))
        .map(|s| s.to_string())
        .collect();

    if !channels_to_parse.is_empty() {
        all_commands.append(&mut vec![format!(
            r#"{{"op":"{}","args":{}}}"#,
            if subscribe {
                "subscribe"
            } else {
                "unsubscribe"
            },
            serde_json::to_string(channels).unwrap()
        )])
    };

    all_commands
}

// see https://www.bitmex.com/app/wsAPI#Response-Format
fn on_misc_msg(msg: &str) -> MiscMessage {
    if msg == "pong" {
        return MiscMessage::Misc;
    }
    let resp = serde_json::from_str::<HashMap<String, Value>>(&msg);
    if resp.is_err() {
        error!("{} is not a JSON string, {}", msg, EXCHANGE_NAME);
        return MiscMessage::Misc;
    }
    let obj = resp.unwrap();

    if obj.contains_key("error") {
        let code = obj.get("status").unwrap().as_i64().unwrap();
        error!("Received {} from {}", msg, EXCHANGE_NAME);
        match code {
            // Rate limit exceeded
            429 => {
                std::thread::sleep(Duration::from_secs(3));
                MiscMessage::Misc
            }
            // You are already subscribed to this topic
            400 => MiscMessage::Misc,
            _ => MiscMessage::Misc,
        }
    } else if obj.contains_key("success") {
        info!("{} is not a JSON string, {}", msg, EXCHANGE_NAME);
        MiscMessage::Misc
    } else if obj.contains_key("table") {
        debug_assert!(obj.contains_key("action"));
        debug_assert!(obj.contains_key("data"));
        MiscMessage::Normal
    } else {
        warn!("Received {} from {}", msg, EXCHANGE_NAME);
        MiscMessage::Misc
    }
}

fn to_raw_channel(channel: &str, pair: &str) -> String {
    format!("{}{}{}", channel, CHANNEL_PAIR_DELIMITER, pair)
}

#[rustfmt::skip]
impl_trait!(Trade, BitMEXWSClient, subscribe_trade, "trade", to_raw_channel);
#[rustfmt::skip]
impl_trait!(BBO, BitMEXWSClient, subscribe_bbo, "quote", to_raw_channel);
#[rustfmt::skip]
impl_trait!(OrderBook, BitMEXWSClient, subscribe_orderbook, "orderBookL2_25", to_raw_channel);
#[rustfmt::skip]
impl_trait!(OrderBookSnapshot, BitMEXWSClient, subscribe_orderbook_snapshot, "orderBook10", to_raw_channel);

impl<'a> Ticker for BitMEXWSClient<'a> {
    fn subscribe_ticker(&self, _pairs: &[String]) {
        panic!("BitMEX WebSocket does NOT have ticker channel");
    }
}

fn to_candlestick_raw_channel(pair: &str, interval: u32) -> String {
    let interval_str = match interval {
        60 => "1m",
        300 => "5m",
        3600 => "1h",
        86400 => "1d",
        _ => panic!("BitMEX has intervals 1m,5m,1h,1d"),
    };
    format!("tradeBin{}:{}", interval_str, pair)
}

impl_candlestick!(BitMEXWSClient);

define_client!(
    BitMEXWSClient,
    EXCHANGE_NAME,
    WEBSOCKET_URL,
    channels_to_commands,
    on_misc_msg
);

#[cfg(test)]
mod tests {
    #[test]
    fn test_one_channel() {
        let commands = super::channels_to_commands(&vec!["trade:XBTUSD".to_string()], true);
        assert_eq!(1, commands.len());
        assert_eq!(r#"{"op":"subscribe","args":["trade:XBTUSD"]}"#, commands[0]);
    }

    #[test]
    fn test_multiple_channels() {
        let commands = super::channels_to_commands(
            &vec![
                "trade:XBTUSD".to_string(),
                "quote:XBTUSD".to_string(),
                "orderBookL2_25:XBTUSD".to_string(),
                "tradeBin1m:XBTUSD".to_string(),
            ],
            true,
        );
        assert_eq!(1, commands.len());
        assert_eq!(
            r#"{"op":"subscribe","args":["trade:XBTUSD","quote:XBTUSD","orderBookL2_25:XBTUSD","tradeBin1m:XBTUSD"]}"#,
            commands[0]
        );
    }
}
