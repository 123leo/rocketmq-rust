/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rocketmq_common::common::base::service_state::ServiceState;
use rocketmq_common::common::constant::PermName;
use rocketmq_common::common::message::message_queue::MessageQueue;
use rocketmq_common::common::mix_all;
use rocketmq_common::ArcRefCellWrapper;
use rocketmq_common::TimeUtils::get_current_millis;
use rocketmq_remoting::base::connection_net_event::ConnectionNetEvent;
use rocketmq_remoting::protocol::heartbeat::consumer_data::ConsumerData;
use rocketmq_remoting::protocol::heartbeat::heartbeat_data::HeartbeatData;
use rocketmq_remoting::protocol::heartbeat::producer_data::ProducerData;
use rocketmq_remoting::protocol::route::topic_route_data::TopicRouteData;
use rocketmq_remoting::protocol::RemotingSerializable;
use rocketmq_remoting::rpc::client_metadata::ClientMetadata;
use rocketmq_remoting::runtime::config::client_config::TokioClientConfig;
use rocketmq_remoting::runtime::RPCHook;
use rocketmq_runtime::RocketMQRuntime;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tracing::info;
use tracing::warn;

use crate::admin::mq_admin_ext_inner::MQAdminExtInner;
use crate::base::client_config::ClientConfig;
use crate::consumer::consumer_impl::pull_message_service::PullMessageService;
use crate::consumer::consumer_impl::rebalance_service::RebalanceService;
use crate::consumer::mq_consumer_inner::MQConsumerInner;
use crate::error::MQClientError::MQClientException;
use crate::implementation::client_remoting_processor::ClientRemotingProcessor;
use crate::implementation::mq_admin_impl::MQAdminImpl;
use crate::implementation::mq_client_api_impl::MQClientAPIImpl;
use crate::producer::default_mq_producer::DefaultMQProducer;
use crate::producer::default_mq_producer::ProducerConfig;
use crate::producer::producer_impl::mq_producer_inner::MQProducerInner;
use crate::producer::producer_impl::topic_publish_info::TopicPublishInfo;
use crate::Result;

#[derive(Clone)]
pub struct MQClientInstance {
    client_config: Arc<ClientConfig>,
    pub(crate) client_id: String,
    boot_timestamp: u64,
    /**
     * The container of the producer in the current client. The key is the name of
     * producerGroup.
     */
    producer_table: Arc<RwLock<HashMap<String, Box<dyn MQProducerInner>>>>,
    /**
     * The container of the consumer in the current client. The key is the name of
     * consumerGroup.
     */
    consumer_table: Arc<RwLock<HashMap<String, Box<dyn MQConsumerInner>>>>,
    /**
     * The container of the adminExt in the current client. The key is the name of
     * adminExtGroup.
     */
    admin_ext_table: Arc<RwLock<HashMap<String, Box<dyn MQAdminExtInner>>>>,
    pub(crate) mq_client_api_impl: ArcRefCellWrapper<MQClientAPIImpl>,
    pub(crate) mq_admin_impl: ArcRefCellWrapper<MQAdminImpl>,
    pub(crate) topic_route_table: Arc<RwLock<HashMap<String /* Topic */, TopicRouteData>>>,
    topic_end_points_table:
        Arc<RwLock<HashMap<String /* Topic */, HashMap<MessageQueue, String /* brokerName */>>>>,
    lock_namesrv: Arc<Mutex<()>>,
    lock_heartbeat: Arc<Mutex<()>>,

    service_state: ServiceState,
    pull_message_service: ArcRefCellWrapper<PullMessageService>,
    rebalance_service: ArcRefCellWrapper<RebalanceService>,
    default_mqproducer: ArcRefCellWrapper<DefaultMQProducer>,
    instance_runtime: Arc<RocketMQRuntime>,
    broker_addr_table: Arc<RwLock<HashMap<String, HashMap<i64, String>>>>,
    broker_version_table:
        Arc<RwLock<HashMap<String /* Broker Name */, HashMap<String /* address */, i32>>>>,
    send_heartbeat_times_total: Arc<AtomicI64>,
}

impl MQClientInstance {
    pub fn new(
        client_config: ClientConfig,
        instance_index: i32,
        client_id: String,
        rpc_hook: Option<Arc<Box<dyn RPCHook>>>,
    ) -> Self {
        let broker_addr_table = Arc::new(Default::default());
        let (tx, _) = tokio::sync::broadcast::channel::<ConnectionNetEvent>(16);
        let mut rx = tx.subscribe();
        let mq_client_api_impl = ArcRefCellWrapper::new(MQClientAPIImpl::new(
            Arc::new(TokioClientConfig::default()),
            ClientRemotingProcessor {},
            rpc_hook,
            client_config.clone(),
            Some(tx),
        ));
        if let Some(namesrv_addr) = client_config.namesrv_addr.as_deref() {
            let handle = Handle::current();
            let mq_client_api_impl_cloned = mq_client_api_impl.clone();
            let namesrv_addr = namesrv_addr.to_string();
            thread::spawn(move || {
                handle.block_on(async move {
                    mq_client_api_impl_cloned
                        .update_name_server_address_list(namesrv_addr.as_str())
                        .await;
                })
            });
        }
        let instance = MQClientInstance {
            client_config: Arc::new(client_config.clone()),
            client_id,
            boot_timestamp: get_current_millis(),
            producer_table: Arc::new(RwLock::new(HashMap::new())),
            consumer_table: Arc::new(Default::default()),
            admin_ext_table: Arc::new(Default::default()),
            mq_client_api_impl,
            mq_admin_impl: ArcRefCellWrapper::new(MQAdminImpl::new()),
            topic_route_table: Arc::new(Default::default()),
            topic_end_points_table: Arc::new(Default::default()),
            lock_namesrv: Default::default(),
            lock_heartbeat: Default::default(),
            service_state: ServiceState::CreateJust,
            pull_message_service: ArcRefCellWrapper::new(PullMessageService {}),
            rebalance_service: ArcRefCellWrapper::new(RebalanceService {}),
            default_mqproducer: ArcRefCellWrapper::new(
                DefaultMQProducer::builder()
                    .producer_group(mix_all::CLIENT_INNER_PRODUCER_GROUP)
                    .client_config(client_config.clone())
                    .build(),
            ),
            instance_runtime: Arc::new(RocketMQRuntime::new_multi(
                num_cpus::get(),
                "mq-client-instance",
            )),
            broker_addr_table,
            broker_version_table: Arc::new(Default::default()),
            send_heartbeat_times_total: Arc::new(AtomicI64::new(0)),
        };
        let instance_ = instance.clone();
        tokio::spawn(async move {
            while let Ok(value) = rx.recv().await {
                match value {
                    ConnectionNetEvent::CONNECTED(remote_address) => {
                        info!("ConnectionNetEvent CONNECTED");
                        let broker_addr_table = instance_.broker_addr_table.read().await;
                        for (broker_name, broker_addrs) in broker_addr_table.iter() {
                            for (id, addr) in broker_addrs.iter() {
                                if addr == remote_address.to_string().as_str()
                                    && instance_
                                        .send_heartbeat_to_broker(*id, broker_name, addr)
                                        .await
                                {
                                    instance_.re_balance_immediately().await;
                                }
                            }
                        }
                    }
                    ConnectionNetEvent::DISCONNECTED => {}
                    ConnectionNetEvent::EXCEPTION => {}
                }
            }
            warn!("ConnectionNetEvent recv error");
        });
        instance
    }

    pub async fn re_balance_immediately(&self) {
        println!("re_balance_immediately")
    }

    pub async fn start(&mut self) -> Result<()> {
        match self.service_state {
            ServiceState::CreateJust => {
                self.service_state = ServiceState::StartFailed;
                // If not specified,looking address from name remoting_server
                if self.client_config.namesrv_addr.is_none() {
                    self.mq_client_api_impl.fetch_name_server_addr().await;
                }
                // Start request-response channel
                self.mq_client_api_impl.start().await;
                // Start various schedule tasks
                self.start_scheduled_task();
                // Start pull service
                self.pull_message_service.start().await;
                // Start rebalance service
                self.rebalance_service.start().await;
                // Start push service
                self.default_mqproducer
                    .default_mqproducer_impl
                    .as_mut()
                    .unwrap()
                    .start_with_factory(false)
                    .await?;
                info!("the client factory[{}] start OK", self.client_id);
                self.service_state = ServiceState::Running;
            }
            ServiceState::Running => {}
            ServiceState::ShutdownAlready => {}
            ServiceState::StartFailed => {
                return Err(MQClientException(
                    -1,
                    format!(
                        "The Factory object[{}] has been created before, and failed.",
                        self.client_id
                    ),
                ));
            }
        }
        Ok(())
    }

    pub async fn register_producer(&mut self, group: &str, producer: impl MQProducerInner) -> bool {
        if group.is_empty() {
            return false;
        }
        let mut producer_table = self.producer_table.write().await;
        if producer_table.contains_key(group) {
            warn!("the producer group[{}] exist already.", group);
            return false;
        }
        producer_table.insert(group.to_string(), Box::new(producer));
        true
    }

    fn start_scheduled_task(&mut self) {
        if self.client_config.namesrv_addr.is_none() {
            let mut mq_client_api_impl = self.mq_client_api_impl.clone();
            self.instance_runtime.get_handle().spawn(async move {
                info!("ScheduledTask fetchNameServerAddr started");
                tokio::time::sleep(Duration::from_secs(10)).await;
                loop {
                    let current_execution_time = tokio::time::Instant::now();
                    mq_client_api_impl.fetch_name_server_addr().await;
                    let next_execution_time = current_execution_time + Duration::from_secs(120);
                    let delay =
                        next_execution_time.saturating_duration_since(tokio::time::Instant::now());
                    tokio::time::sleep(delay).await;
                }
            });
        }

        let mut client_instance = self.clone();
        let poll_name_server_interval = self.client_config.poll_name_server_interval;
        self.instance_runtime.get_handle().spawn(async move {
            info!("ScheduledTask updateTopicRouteInfoFromNameServer started");
            tokio::time::sleep(Duration::from_millis(10)).await;
            loop {
                let current_execution_time = tokio::time::Instant::now();
                client_instance
                    .update_topic_route_info_from_name_server()
                    .await;
                let next_execution_time = current_execution_time
                    + Duration::from_millis(poll_name_server_interval as u64);
                let delay =
                    next_execution_time.saturating_duration_since(tokio::time::Instant::now());
                tokio::time::sleep(delay).await;
            }
        });

        let mut client_instance = self.clone();
        let heartbeat_broker_interval = self.client_config.heartbeat_broker_interval;
        self.instance_runtime.get_handle().spawn(async move {
            info!("ScheduledTask send_heartbeat_to_all_broker started");
            tokio::time::sleep(Duration::from_secs(1)).await;
            loop {
                let current_execution_time = tokio::time::Instant::now();
                client_instance.clean_offline_broker().await;
                client_instance
                    .send_heartbeat_to_all_broker_with_lock()
                    .await;
                let next_execution_time = current_execution_time
                    + Duration::from_millis(heartbeat_broker_interval as u64);
                let delay =
                    next_execution_time.saturating_duration_since(tokio::time::Instant::now());
                tokio::time::sleep(delay).await;
            }
        });

        let mut client_instance = self.clone();
        let persist_consumer_offset_interval =
            self.client_config.persist_consumer_offset_interval as u64;
        self.instance_runtime.get_handle().spawn(async move {
            info!("ScheduledTask persistAllConsumerOffset started");
            tokio::time::sleep(Duration::from_secs(10)).await;
            loop {
                let current_execution_time = tokio::time::Instant::now();
                client_instance.persist_all_consumer_offset().await;
                let next_execution_time = current_execution_time
                    + Duration::from_millis(persist_consumer_offset_interval);
                let delay =
                    next_execution_time.saturating_duration_since(tokio::time::Instant::now());
                tokio::time::sleep(delay).await;
            }
        });
    }

    pub async fn update_topic_route_info_from_name_server(&mut self) {
        println!("updateTopicRouteInfoFromNameServer")
    }

    #[inline]
    pub async fn update_topic_route_info_from_name_server_topic(&mut self, topic: &str) -> bool {
        self.update_topic_route_info_from_name_server_default(topic, false, None)
            .await
    }

    pub async fn update_topic_route_info_from_name_server_default(
        &mut self,
        topic: &str,
        is_default: bool,
        producer_config: Option<&Arc<ProducerConfig>>,
    ) -> bool {
        let lock = self.lock_namesrv.lock().await;
        let topic_route_data = if is_default && producer_config.is_some() {
            let mut result = self
                .mq_client_api_impl
                .get_default_topic_route_info_from_name_server(
                    self.client_config.mq_client_api_timeout,
                )
                .await
                .unwrap_or(None);
            if let Some(topic_route_data) = result.as_mut() {
                for data in topic_route_data.queue_datas.iter_mut() {
                    let queue_nums = producer_config
                        .unwrap()
                        .default_topic_queue_nums()
                        .max(data.read_queue_nums);
                    data.read_queue_nums = queue_nums;
                    data.write_queue_nums = queue_nums;
                }
            }
            result
        } else {
            self.mq_client_api_impl
                .get_topic_route_info_from_name_server(
                    topic,
                    self.client_config.mq_client_api_timeout,
                )
                .await
                .unwrap_or(None)
        };
        if let Some(mut topic_route_data) = topic_route_data {
            let mut topic_route_table = self.topic_route_table.write().await;
            let old = topic_route_table.get(topic);
            let mut changed = topic_route_data.topic_route_data_changed(old);
            if !changed {
                changed = self.is_need_update_topic_route_info(topic).await;
            } else {
                info!(
                    "the topic[{}] route info changed, old[{:?}] ,new[{:?}]",
                    topic, old, topic_route_data
                )
            }
            if changed {
                let mut broker_addr_table = self.broker_addr_table.write().await;
                for bd in topic_route_data.broker_datas.iter() {
                    broker_addr_table
                        .insert(bd.broker_name().to_string(), bd.broker_addrs().clone());
                }
                drop(broker_addr_table);

                // Update endpoint map
                {
                    let mq_end_points = ClientMetadata::topic_route_data2endpoints_for_static_topic(
                        topic,
                        &topic_route_data,
                    );
                    if let Some(mq_end_points) = mq_end_points {
                        if !mq_end_points.is_empty() {
                            let mut topic_end_points_table =
                                self.topic_end_points_table.write().await;
                            topic_end_points_table.insert(topic.to_string(), mq_end_points);
                        }
                    }
                }

                // Update Pub info
                {
                    let mut publish_info =
                        topic_route_data2topic_publish_info(topic, &mut topic_route_data);
                    publish_info.have_topic_router_info = true;
                    let mut producer_table = self.producer_table.write().await;
                    for (_, value) in producer_table.iter_mut() {
                        value.update_topic_publish_info(
                            topic.to_string(),
                            Some(publish_info.clone()),
                        );
                    }
                }

                // Update sub info
                {
                    let mut consumer_table = self.consumer_table.write().await;
                    if !consumer_table.is_empty() {
                        let subscribe_info =
                            topic_route_data2topic_subscribe_info(topic, &topic_route_data);
                        for (_, value) in consumer_table.iter_mut() {
                            value.update_topic_subscribe_info(topic, &subscribe_info);
                        }
                    }
                }
                let clone_topic_route_data = TopicRouteData::from_existing(&topic_route_data);
                topic_route_table.insert(topic.to_string(), clone_topic_route_data);
                return true;
            }
        } else {
            warn!(
                "updateTopicRouteInfoFromNameServer, getTopicRouteInfoFromNameServer return null, \
                 Topic: {}. [{}]",
                topic, self.client_id
            );
        }

        drop(lock);
        false
    }

    async fn is_need_update_topic_route_info(&self, topic: &str) -> bool {
        let mut result = false;
        let producer_table = self.producer_table.read().await;
        for (key, value) in producer_table.iter() {
            if !result {
                result = value.is_publish_topic_need_update(topic);
                break;
            }
        }
        if result {
            return true;
        }

        let consumer_table = self.consumer_table.read().await;
        for (key, value) in consumer_table.iter() {
            if !result {
                result = value.is_subscribe_topic_need_update(topic);
                break;
            }
        }
        result
    }

    pub async fn persist_all_consumer_offset(&mut self) {
        println!("updateTopicRouteInfoFromNameServer")
    }

    pub async fn clean_offline_broker(&mut self) {
        println!("cleanOfflineBroker")
    }
    pub async fn send_heartbeat_to_all_broker_with_lock(&mut self) -> bool {
        match self.lock_heartbeat.try_lock() {
            Ok(_) => {
                if self.client_config.use_heartbeat_v2 {
                    self.send_heartbeat_to_all_broker_v2(false).await
                } else {
                    self.send_heartbeat_to_all_broker().await
                }
            }
            Err(_) => {
                warn!("lock heartBeat, but failed. [{}]", self.client_id);
                false
            }
        }
    }

    pub async fn send_heartbeat_to_all_broker_with_lock_v2(&mut self, is_rebalance: bool) -> bool {
        match self.lock_heartbeat.try_lock() {
            Ok(_) => {
                if self.client_config.use_heartbeat_v2 {
                    self.send_heartbeat_to_all_broker_v2(false).await
                } else {
                    self.send_heartbeat_to_all_broker().await
                }
            }
            Err(_) => {
                warn!("lock heartBeat, but failed. [{}]", self.client_id);
                false
            }
        }
    }

    pub fn get_mq_client_api_impl(&self) -> ArcRefCellWrapper<MQClientAPIImpl> {
        self.mq_client_api_impl.clone()
    }

    pub async fn get_broker_name_from_message_queue(&self, message_queue: &MessageQueue) -> String {
        let guard = self.topic_end_points_table.read().await;
        if let Some(broker_name) = guard.get(message_queue.get_topic()) {
            if let Some(addr) = broker_name.get(message_queue) {
                return addr.clone();
            }
        }
        message_queue.get_broker_name().to_string()
    }

    pub async fn find_broker_address_in_publish(&self, broker_name: &str) -> Option<String> {
        if broker_name.is_empty() {
            return None;
        }
        let guard = self.broker_addr_table.read().await;
        let map = guard.get(broker_name);
        if let Some(map) = map {
            return map.get(&(mix_all::MASTER_ID as i64)).cloned();
        }
        None
    }

    async fn send_heartbeat_to_all_broker_v2(&self, is_rebalance: bool) -> bool {
        unimplemented!()
    }

    async fn send_heartbeat_to_all_broker(&self) -> bool {
        let heartbeat_data = self.prepare_heartbeat_data(false).await;
        let producer_empty = heartbeat_data.producer_data_set.is_empty();
        let consumer_empty = heartbeat_data.consumer_data_set.is_empty();
        if producer_empty && consumer_empty {
            warn!(
                "sending heartbeat, but no consumer and no producer. [{}]",
                self.client_id
            );
            return false;
        }
        let broker_addr_table = self.broker_addr_table.read().await;
        if broker_addr_table.is_empty() {
            return false;
        }
        for (broker_name, broker_addrs) in broker_addr_table.iter() {
            if broker_addrs.is_empty() {
                continue;
            }
            for (id, addr) in broker_addrs.iter() {
                if addr.is_empty() {
                    continue;
                }
                if consumer_empty && *id != mix_all::MASTER_ID as i64 {
                    continue;
                }
                self.send_heartbeat_to_broker_inner(*id, broker_name, addr, &heartbeat_data)
                    .await;
            }
        }

        true
    }

    pub async fn send_heartbeat_to_broker(&self, id: i64, broker_name: &str, addr: &str) -> bool {
        if self.lock_heartbeat.try_lock().is_ok() {
            let heartbeat_data = self.prepare_heartbeat_data(false).await;
            let producer_empty = heartbeat_data.producer_data_set.is_empty();
            let consumer_empty = heartbeat_data.consumer_data_set.is_empty();
            if producer_empty && consumer_empty {
                warn!(
                    "sending heartbeat, but no consumer and no producer. [{}]",
                    self.client_id
                );
                return false;
            }

            if self.client_config.use_heartbeat_v2 {
                unimplemented!("sendHeartbeatToBrokerV2")
            } else {
                self.send_heartbeat_to_broker_inner(id, broker_name, addr, &heartbeat_data)
                    .await
            }
        } else {
            false
        }
    }

    async fn send_heartbeat_to_broker_inner(
        &self,
        id: i64,
        broker_name: &str,
        addr: &str,
        heartbeat_data: &HeartbeatData,
    ) -> bool {
        if let Ok(version) = self
            .mq_client_api_impl
            .mut_from_ref()
            .send_heartbeat(
                addr,
                heartbeat_data,
                self.client_config.mq_client_api_timeout,
            )
            .await
        {
            let mut broker_version_table = self.broker_version_table.write().await;
            let map = broker_version_table.get_mut(broker_name);
            if let Some(map) = map {
                map.insert(addr.to_string(), version);
            } else {
                let mut map = HashMap::new();
                map.insert(addr.to_string(), version);
                broker_version_table.insert(broker_name.to_string(), map);
            }

            let times = self
                .send_heartbeat_times_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if times % 20 == 0 {
                info!(
                    "send heart beat to broker[{} {} {}] success",
                    broker_name, id, addr,
                );
            }
            return true;
        }
        if self.is_broker_in_name_server(addr).await {
            warn!(
                "send heart beat to broker[{} {} {}] failed",
                broker_name, id, addr
            );
        } else {
            warn!(
                "send heart beat to broker[{} {} {}] exception, because the broker not up, forget \
                 it",
                broker_name, id, addr
            )
        }
        false
    }

    async fn is_broker_in_name_server(&self, broker_name: &str) -> bool {
        let broker_addr_table = self.topic_route_table.read().await;
        for (_, value) in broker_addr_table.iter() {
            for bd in value.broker_datas.iter() {
                for (_, value) in bd.broker_addrs().iter() {
                    if value.as_str() == broker_name {
                        return true;
                    }
                }
            }
        }
        false
    }

    async fn prepare_heartbeat_data(&self, is_without_sub: bool) -> HeartbeatData {
        let mut heartbeat_data = HeartbeatData {
            client_id: self.client_id.clone(),
            ..Default::default()
        };

        let consumer_table = self.consumer_table.read().await;
        for (_, value) in consumer_table.iter() {
            let mut consumer_data = ConsumerData {
                group_name: value.group_name().to_json(),
                consume_type: value.consume_type(),
                message_model: value.message_model(),
                consume_from_where: value.consume_from_where(),
                subscription_data_set: value.subscriptions().clone(),
                unit_mode: value.is_unit_mode(),
            };
            if !is_without_sub {
                value.subscriptions().iter().for_each(|sub| {
                    consumer_data.subscription_data_set.insert(sub.clone());
                });
            }
            heartbeat_data.consumer_data_set.insert(consumer_data);
        }
        drop(consumer_table);
        let producer_table = self.producer_table.read().await;
        for (group_name, _) in producer_table.iter() {
            let producer_data = ProducerData {
                group_name: group_name.clone(),
            };
            heartbeat_data.producer_data_set.insert(producer_data);
        }
        drop(producer_table);
        heartbeat_data.is_without_sub = is_without_sub;
        heartbeat_data
    }
}

pub fn topic_route_data2topic_publish_info(
    topic: &str,
    route: &mut TopicRouteData,
) -> TopicPublishInfo {
    let mut info = TopicPublishInfo {
        topic_route_data: Some(route.clone()),
        ..Default::default()
    };
    if route.order_topic_conf.is_some() && !route.order_topic_conf.as_ref().unwrap().is_empty() {
        let brokers = route
            .order_topic_conf
            .as_ref()
            .unwrap()
            .split(";")
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        for broker in brokers {
            let item = broker.split(":").collect::<Vec<&str>>();
            if item.len() == 2 {
                let queue_num = item[1].parse::<i32>().unwrap();
                for i in 0..queue_num {
                    let mq = MessageQueue::from_parts(topic, item[0], i);
                    info.message_queue_list.push(mq);
                }
            }
        }
        info.order_topic = true;
    } else if route.order_topic_conf.is_none()
        && route.topic_queue_mapping_by_broker.is_some()
        && !route
            .topic_queue_mapping_by_broker
            .as_ref()
            .unwrap()
            .is_empty()
    {
        info.order_topic = false;
        let mq_end_points =
            ClientMetadata::topic_route_data2endpoints_for_static_topic(topic, route);
        if let Some(mq_end_points) = mq_end_points {
            for (mq, broker_name) in mq_end_points {
                info.message_queue_list.push(mq);
            }
        }
        info.message_queue_list
            .sort_by(|a, b| match a.get_queue_id().cmp(&b.get_queue_id()) {
                Ordering::Less => std::cmp::Ordering::Less,
                Ordering::Equal => std::cmp::Ordering::Equal,
                Ordering::Greater => std::cmp::Ordering::Greater,
            });
    } else {
        route.queue_datas.sort();
        for queue_data in route.queue_datas.iter() {
            if PermName::is_writeable(queue_data.perm) {
                let mut broker_data = None;
                for bd in route.broker_datas.iter() {
                    if bd.broker_name() == queue_data.broker_name {
                        broker_data = Some(bd.clone());
                        break;
                    }
                }
                if broker_data.is_none() {
                    continue;
                }
                if !broker_data
                    .as_ref()
                    .unwrap()
                    .broker_addrs()
                    .contains_key(&(mix_all::MASTER_ID as i64))
                {
                    continue;
                }
                for i in 0..queue_data.write_queue_nums {
                    let mq =
                        MessageQueue::from_parts(topic, queue_data.broker_name.as_str(), i as i32);
                    info.message_queue_list.push(mq);
                }
            }
        }
    }
    info
}

pub fn topic_route_data2topic_subscribe_info(
    topic: &str,
    topic_route_data: &TopicRouteData,
) -> HashSet<MessageQueue> {
    unimplemented!("topicRouteData2TopicSubscribeInfo")
}
