//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use crate::config::Config;
use crate::mqtt_helpers::*;
use async_channel::{Receiver, Sender};
use async_std::sync::RwLock;
use lazy_static::__Deref;
use ntex::util::{ByteString, Bytes};
use std::convert::TryInto;
use std::{collections::HashMap, sync::Arc};
use zenoh::plugins::ZResult;
use zenoh::prelude::r#async::*;
use zenoh::subscriber::Subscriber;

#[derive(Debug)]
pub(crate) struct MqttSessionState<'a> {
    pub(crate) client_id: String,
    pub(crate) zsession: Arc<Session>,
    pub(crate) config: Arc<Config>,
    pub(crate) subs: RwLock<HashMap<String, Subscriber<'a, ()>>>,
    pub(crate) tx: Sender<(ByteString, Bytes)>,
}

impl MqttSessionState<'_> {
    pub(crate) fn new<'a>(
        client_id: String,
        zsession: Arc<Session>,
        config: Arc<Config>,
        sink: MqttSink,
    ) -> MqttSessionState<'a> {
        let (tx, rx) = async_channel::bounded::<(ByteString, Bytes)>(1);
        spawn_mqtt_publisher(client_id.clone(), rx, sink);

        MqttSessionState {
            client_id,
            zsession,
            config,
            subs: RwLock::new(HashMap::new()),
            tx,
        }
    }

    pub(crate) async fn map_mqtt_subscription<'a>(&'a self, topic: &str) -> ZResult<()> {
        let sub_origin = if is_allowed(topic, &self.config) {
            // if topic is allowed, subscribe to publications coming from anywhere
            Locality::Any
        } else {
            // if topic is NOT allowed, subscribe to publications coming only from this plugin (for MQTT-to-MQTT routing only)
            log::debug!(
                "MQTT Client {}: topic '{}' is not allowed to be routed over Zenoh (see your 'allow' or 'deny' configuration) - re-publish only from MQTT publishers",
                self.client_id,
                topic
            );
            Locality::SessionLocal
        };

        let mut subs = self.subs.write().await;
        if !subs.contains_key(topic) {
            let ke = mqtt_topic_to_ke(topic, &self.config.scope)?;
            let client_id = self.client_id.clone();
            let config = self.config.clone();
            let tx = self.tx.clone();
            let sub = self
                .zsession
                .declare_subscriber(ke)
                .callback(move |sample| {
                    if let Err(e) = route_zenoh_to_mqtt(sample, &client_id, &config, &tx) {
                        log::warn!("{}", e);
                    }
                })
                .allowed_origin(sub_origin)
                .res()
                .await?;
            subs.insert(topic.into(), sub);
            Ok(())
        } else {
            log::debug!(
                "MQTT Client {} already subscribes to {} => ignore",
                self.client_id,
                topic
            );
            Ok(())
        }
    }

    pub(crate) async fn route_mqtt_to_zenoh(
        &self,
        mqtt_topic: &ntex::router::Path<ByteString>,
        payload: &Bytes,
    ) -> ZResult<()> {
        let topic = mqtt_topic.get_ref().as_str();
        let destination = if is_allowed(topic, &self.config) {
            // if topic is allowed, publish to anywhere
            Locality::Any
        } else {
            // if topic is NOT allowed, publish only to this plugin (for MQTT-to-MQTT routing only)
            log::trace!(
                "MQTT Client {}: topic '{}' is not allowed to be routed over Zenoh (see your 'allow' or 'deny' configuration) - re-publish only to MQTT subscriber",
                self.client_id,
                topic
            );
            Locality::SessionLocal
        };

        let ke: KeyExpr = if let Some(scope) = &self.config.scope {
            (scope / topic.try_into()?).into()
        } else {
            topic.try_into()?
        };
        let encoding = guess_encoding(payload.deref());
        // TODO: check allow/deny
        log::trace!(
            "MQTT client {}: route from MQTT '{}' to Zenoh '{}' (encoding={})",
            self.client_id,
            topic,
            ke,
            encoding
        );
        self.zsession
            .put(ke, payload.deref())
            .encoding(encoding)
            .allowed_destination(destination)
            .res()
            .await
    }
}

fn route_zenoh_to_mqtt(
    sample: Sample,
    client_id: &str,
    config: &Config,
    tx: &Sender<(ByteString, Bytes)>,
) -> ZResult<()> {
    let topic = ke_to_mqtt_topic_publish(&sample.key_expr, &config.scope)?;
    log::trace!(
        "MQTT client {}: route from Zenoh '{}' to MQTT '{}'",
        client_id,
        sample.key_expr,
        topic
    );
    tx.send_blocking((topic, sample.payload.contiguous().to_vec().into()))
        .map_err(|e| {
            zerror!(
                "MQTT client {}: error re-publishing on MQTT a Zenoh publication on {}: {}",
                client_id,
                sample.key_expr,
                e
            )
            .into()
        })
}

fn spawn_mqtt_publisher(client_id: String, rx: Receiver<(ByteString, Bytes)>, sink: MqttSink) {
    ntex::rt::spawn(async move {
        loop {
            match rx.recv().await {
                Ok((topic, payload)) => {
                    if sink.is_open() {
                        if let Err(e) = sink.publish_at_most_once(topic, payload) {
                            log::trace!(
                                "Failed to send MQTT message for client {} - {}",
                                client_id,
                                e
                            );
                            sink.close();
                            break;
                        }
                    } else {
                        log::trace!("MQTT sink closed for client {}", client_id);
                        break;
                    }
                }
                Err(_) => {
                    log::trace!("MPSC Channel closed for client {}", client_id);
                    break;
                }
            }
        }
    });
}
