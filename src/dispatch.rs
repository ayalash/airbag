use crossbeam::channel::{bounded, Sender};
use lazy_static::lazy_static;
use log::error;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

use crate::utils::DeliverReceipt;

lazy_static! {
    pub(crate) static ref PAGERDUTY_INTEGRATION_KEY: Arc<RwLock<Option<String>>> =
        Default::default();
    pub(crate) static ref HUB: RwLock<Hub> = Default::default();
}

const BUFFER_SIZE: usize = 100;

pub(crate) enum Message {
    Alert(Value, DeliverReceipt),
    Terminate(DeliverReceipt),
}
pub(crate) enum Hub {
    Empty,
    Configured {
        routing_key: String,
        extra_details: Option<Value>,
        sender: Sender<Message>,
    },
}

impl Default for Hub {
    fn default() -> Self {
        Self::Empty
    }
}

impl Hub {
    pub(crate) fn dispatch_and_block<F: Fn() -> Value>(&self, f: F) {
        if let Some(receipt) = self.dispatch(f) {
            receipt.wait()
        }
    }

    pub(crate) fn dispatch<F: Fn() -> Value>(&self, f: F) -> Option<DeliverReceipt> {
        if let Hub::Configured {
            sender,
            extra_details,
            routing_key,
        } = self
        {
            let mut dispatched = f();

            let mut details = dispatched["payload"]["custom_details"].take();

            if matches!(&details, Value::Object(_)) {
                details["additional_details"] = json!(extra_details.clone());
            } else {
                details = json!({
                    "details":  details,
                    "additional_details": extra_details.clone(),
                })
            }

            dispatched["payload"]["custom_details"] = details;

            dispatched["routing_key"] = Value::String(routing_key.clone());
            let receipt = DeliverReceipt::default();
            if sender
                .try_send(Message::Alert(dispatched, receipt.clone()))
                .is_err()
            {
                error!("Failed sending airbag alert: buffer is full");
            }
            Some(receipt)
        } else {
            None
        }
    }
}

#[must_use = "Airbag guard must be stored to flush messages on program end"]
pub fn configure_pagerduty(
    routing_key: impl Into<String>,
    extra_details: Option<Value>,
) -> AirbagGuard {
    let (sender, receiver) = bounded(BUFFER_SIZE);
    let guard = AirbagGuard {
        sender: sender.clone(),
    };
    *HUB.write() = Hub::Configured {
        routing_key: routing_key.into(),
        extra_details,
        sender,
    };

    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Can't create HTTP client");

        // Err means disconnection of the sender side
        while let Some(event) = receiver.recv().ok() {
            match &event {
                Message::Alert(alert, receipt) => {
                    log::debug!("Got PD alert to send");
                    while let Err(e) = client
                        .post("https://events.pagerduty.com/v2/enqueue")
                        .json(alert)
                        .send()
                        .and_then(|resp| resp.error_for_status())
                    {
                        error!("Failed dispatching PD event ({:?}). Going to retry...", e);
                        std::thread::sleep(Duration::from_secs(5));
                    }
                    log::info!("Sent successfully");
                    receipt.signal()
                }
                Message::Terminate(receipt) => {
                    receipt.signal();
                    break;
                }
            }
        }
    });

    crate::panic_handler::install();

    guard
}

pub struct AirbagGuard {
    sender: Sender<Message>,
}

impl Drop for AirbagGuard {
    fn drop(&mut self) {
        let receipt = DeliverReceipt::default();
        let _ = self.sender.send(Message::Terminate(receipt.clone()));
        log::info!("Waiting for Airbag message to flush...");
        receipt.wait()
    }
}
