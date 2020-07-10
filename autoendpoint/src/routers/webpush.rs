use crate::error::{ApiErrorKind, ApiResult};
use crate::extractors::notification::Notification;
use crate::routers::{Router, RouterError, RouterResponse};
use async_trait::async_trait;
use autopush_common::db::{DynamoDbUser, DynamoStorage};
use autopush_common::errors::ErrorKind;
use cadence::{Counted, StatsdClient};
use futures::compat::Future01CompatExt;
use reqwest::{Response, StatusCode};
use std::collections::HashMap;
use url::Url;
use uuid::Uuid;

/// The router for desktop user agents.
///
/// These agents are connected via an Autopush connection server. The correct
/// server is located via the database routing table. If the server is busy or
/// not available, the notification is stored in the database.
pub struct WebPushRouter {
    pub ddb: DynamoStorage,
    pub metrics: StatsdClient,
    pub http: reqwest::Client,
    pub endpoint_url: Url,
}

#[async_trait(?Send)]
impl Router for WebPushRouter {
    async fn route_notification(&self, notification: &Notification) -> ApiResult<RouterResponse> {
        let user = &notification.subscription.user;
        debug!(
            "Routing WebPush notification to UAID {}",
            notification.subscription.user.uaid
        );
        trace!("Notification = {:?}", notification);

        // Check if there is a node connected to the client
        if let Some(node_id) = &user.node_id {
            trace!("User has a node ID, sending notification to node");

            // Try to send the notification to the node
            match self.send_notification(notification, node_id).await {
                Ok(response) => {
                    // The node might be busy, make sure it accepted the notification
                    if response.status() == 200 {
                        // The node has received the notification
                        trace!("Node received notification");
                        return Ok(self.make_delivered_response(notification));
                    }

                    trace!(
                        "Node did not receive the notification, response = {:?}",
                        response
                    );
                }
                Err(error) => {
                    // We should stop sending notifications to this node for this user
                    debug!("Error while sending webpush notification: {}", error);
                    self.remove_node_id(user, node_id.clone()).await?;
                }
            }
        }

        // Save notification, node is not present or busy
        trace!("Node is not present or busy, storing notification");
        self.store_notification(notification).await?;

        // Retrieve the user data again, they may have reconnected or the node
        // is no longer busy.
        trace!("Re-fetching user to trigger notification check");
        let user = match self.ddb.get_user(&user.uaid).compat().await {
            Ok(user) => user,
            Err(e) => {
                return match e.kind() {
                    ErrorKind::Msg(msg) if msg == "No user record found" => {
                        trace!("No user found, must have been deleted");
                        Err(ApiErrorKind::Router(RouterError::UserWasDeleted).into())
                    }
                    // Database error, but we already stored the message so it's ok
                    _ => {
                        debug!("Database error while re-fetching user: {}", e);
                        Ok(self.make_stored_response(notification))
                    }
                };
            }
        };

        // Try to notify the node the user is currently connected to
        let node_id = match &user.node_id {
            Some(id) => id,
            // The user is not connected to a node, nothing more to do
            None => {
                trace!("User is not connected to a node, returning stored response");
                return Ok(self.make_stored_response(notification));
            }
        };

        // Notify the node to check for messages
        trace!("Notifying node to check for messages");
        match self.trigger_notification_check(&user.uaid, &node_id).await {
            Ok(response) => {
                trace!("Response = {:?}", response);
                if response.status() == 200 {
                    trace!("Node has delivered the message");
                    Ok(self.make_delivered_response(notification))
                } else {
                    trace!("Node has not delivered the message, returning stored response");
                    Ok(self.make_stored_response(notification))
                }
            }
            Err(error) => {
                // Can't communicate with the node, so we should stop using it
                debug!("Error while triggering notification check: {}", error);
                self.remove_node_id(&user, node_id.clone()).await?;
                Ok(self.make_stored_response(notification))
            }
        }
    }
}

impl WebPushRouter {
    /// Send the notification to the node
    async fn send_notification(
        &self,
        notification: &Notification,
        node_id: &str,
    ) -> Result<Response, reqwest::Error> {
        let url = format!("{}/push/{}", node_id, notification.subscription.user.uaid);
        let notification = notification.serialize_for_delivery();

        self.http.put(&url).json(&notification).send().await
    }

    /// Notify the node to check for notifications for the user
    async fn trigger_notification_check(
        &self,
        uaid: &Uuid,
        node_id: &str,
    ) -> Result<Response, reqwest::Error> {
        let url = format!("{}/notif/{}", node_id, uaid);

        self.http.put(&url).send().await
    }

    /// Store a notification in the database
    async fn store_notification(&self, notification: &Notification) -> ApiResult<()> {
        self.ddb
            .store_message(
                &notification.subscription.user.uaid,
                notification
                    .subscription
                    .user
                    .current_month
                    .clone()
                    .unwrap_or_else(|| self.ddb.current_message_month.clone()),
                notification.clone().into(),
            )
            .compat()
            .await
            .map_err(|e| ApiErrorKind::Router(RouterError::SaveDb(e)).into())
    }

    /// Remove the node ID from a user. This is done if the user is no longer
    /// connected to the node.
    async fn remove_node_id(&self, user: &DynamoDbUser, node_id: String) -> ApiResult<()> {
        self.metrics.incr("updates.client.host_gone").ok();

        self.ddb
            .remove_node_id(&user.uaid, node_id, user.connected_at)
            .compat()
            .await
            .map_err(|e| ApiErrorKind::Database(e).into())
    }

    /// Update metrics and create a response for when a notification has been directly forwarded to
    /// an autopush server.
    fn make_delivered_response(&self, notification: &Notification) -> RouterResponse {
        self.make_response(notification, "Direct", StatusCode::OK)
    }

    /// Update metrics and create a response for when a notification has been stored in the database
    /// for future transmission.
    fn make_stored_response(&self, notification: &Notification) -> RouterResponse {
        self.make_response(notification, "Stored", StatusCode::ACCEPTED)
    }

    /// Update metrics and create a response after routing a notification
    fn make_response(
        &self,
        notification: &Notification,
        destination_tag: &str,
        status: StatusCode,
    ) -> RouterResponse {
        self.metrics
            .count_with_tags(
                "notification.message_data",
                notification.data.as_ref().map(String::len).unwrap_or(0) as i64,
            )
            .with_tag("destination", destination_tag)
            .send();

        RouterResponse {
            status,
            headers: {
                let mut map = HashMap::new();
                map.insert(
                    "Location",
                    self.endpoint_url
                        .join(&format!("/m/{}", notification.message_id))
                        .expect("Message ID is not URL-safe")
                        .to_string(),
                );
                map.insert("TTL", notification.headers.ttl.to_string());
                map
            },
            body: None,
        }
    }
}